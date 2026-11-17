//! Scripted Session transitions; offline programs or an explicit isolated broker transport.
use ringo_cli::{Agent, Mode, Session, Status, turn_policy::TurnPolicy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::ExitCode, time::{Duration, Instant}};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn cancel(_: libc::c_int) { CANCELLED.store(true, Ordering::SeqCst); }

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

#[cfg(unix)]
fn broker_client(args: &[String]) -> Result<(), String> {
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixStream;
    if args.len() != 2 && !(args.len() == 4 && args[2] == "--resume") {
        return Err("invalid broker client arguments".into());
    }
    let mut line = String::new();
    std::io::stdin().lock().take(1_048_577).read_line(&mut line).map_err(|e| e.to_string())?;
    if line.len() > 1_048_576 || !line.ends_with('\n') { return Err("invalid broker prompt".into()) }
    let message: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    let codex = args[0] == "--codex-broker-client";
    let prompt = message.pointer(if codex { "/prompt" } else { "/message/content" })
        .and_then(Value::as_str).ok_or("broker requires text")?;
    if prompt.len() > 128 * 1024 { return Err("broker prompt exceeds decoded limit".into()) }
    let resume = if codex { message["resume"].as_str() } else { args.get(3).map(String::as_str) };
    let mut socket = UnixStream::connect(&args[1]).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(360))).map_err(|e| e.to_string())?;
    writeln!(socket, "{}", json!({"prompt":prompt, "resume":resume})).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::new(socket);
    let mut output = std::io::stdout().lock();
    let mut total = 0;
    loop {
        line.clear();
        let size = (&mut reader).take(524_289).read_line(&mut line).map_err(|e| e.to_string())?;
        if size == 0 { break }
        total += size;
        if size > 524_288 || total > 8 * 1024 * 1024 || !line.ends_with('\n') {
            return Err("broker response exceeds limit".into());
        }
        output.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn run() -> Result<Value, String> {
    if std::env::var_os("RINGO_CHILD").is_some() { return Err("RINGO_CHILD is set".into()) }
    let mut args = std::env::args().skip(1);
    let mut opts = std::collections::BTreeMap::new();
    while let Some(key) = args.next() {
        if !["--project", "--scenario", "--program", "--broker-socket", "--budget-tokens", "--state", "--home", "--policy"].contains(&key.as_str()) {
            return Err(format!("unknown option {key}"));
        }
        let value = args.next().ok_or_else(|| format!("{key} needs a value"))?;
        if opts.insert(key.clone(), value).is_some() { return Err(format!("duplicate option {key}")) }
    }
    let path = |key: &str| opts.get(key).map(PathBuf::from).ok_or_else(|| format!("{key} is required"));
    let (project, scenario, state, home) = (path("--project")?, path("--scenario")?, path("--state")?, path("--home")?);
    let broker = opts.get("--broker-socket").map(PathBuf::from);
    if broker.is_some() == opts.contains_key("--program") {
        return Err("exactly one of --program and --broker-socket is required".into());
    }
    let program = if let Some(socket) = &broker {
        #[cfg(unix)] {
            use std::os::unix::fs::FileTypeExt;
            if !fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket()) { return Err("broker must be a Unix socket".into()) }
            std::env::current_exe().map_err(|e| e.to_string())?
        }
        #[cfg(not(unix))]
        { let _ = socket; return Err("isolated broker transport requires Unix".into()); }
    } else { fs::canonicalize(path("--program")?).map_err(|e| e.to_string())? };
    let token_cap = opts.get("--budget-tokens").map(|s| s.parse::<u64>()).transpose()
        .map_err(|_| "invalid budget-tokens")?;
    if broker.is_some() && token_cap.is_none_or(|n| n == 0 || n > 10_000_000) {
        return Err("broker scenarios require a positive token cap at most 10000000".into());
    }
    let stem = program.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
    let live = Agent::all().into_iter().any(|agent| {
        let name = agent.spec().program.clone();
        Path::new(&name).file_stem().is_some_and(|s| s.to_string_lossy().to_lowercase() == stem)
            || std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p)
                .any(|d| fs::canonicalize(d.join(&name)).is_ok_and(|p| p == program)))
    });
    if live { return Err("live provider programs are refused; no isolated broker profile is certified".into()) }
    let bytes = fs::read(&scenario).map_err(|e| e.to_string())?;
    let spec: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let obj = spec.as_object().ok_or("scenario must be an object")?;
    if obj.keys().any(|k| !["agent", "actions", "seconds"].contains(&k.as_str())) {
        return Err("unknown scenario field".into());
    }
    let agent = spec["agent"].as_str().and_then(Agent::from_name).ok_or("unknown agent")?;
    if broker.is_some() && !["claude", "codex"].contains(&agent.name()) { return Err("unsupported broker agent".into()) }
    let seconds = spec.get("seconds").map_or(Some(60), Value::as_u64)
        .filter(|n| (1..=300).contains(n)).ok_or("seconds must be from 1 through 300")?;
    let actions = spec["actions"].as_array().filter(|a| !a.is_empty() && a.len() <= 64)
        .ok_or("actions must have from 1 through 64 entries")?;
    for action in actions {
        let obj = action.as_object().ok_or("an action must be an object")?;
        let op = action["op"].as_str().ok_or("action needs op")?;
        let keys: &[&str] = match op {
            "turn" => &["op", "message"], "clear" | "restart" | "start_over" => &["op"],
            "compact" if agent.name() == "claude" => &["op"],
            "seed" => &["op", "threads", "records"],
            _ => return Err(format!("unsupported action {op}")),
        };
        if obj.keys().any(|k| !keys.contains(&k.as_str())) { return Err("unknown action field".into()) }
        if op == "turn" && !action["message"].is_string() { return Err("turn needs message text".into()) }
        if op == "seed" {
            let threads = serde_json::from_value::<Vec<(String, String)>>(action["threads"].clone()).map_err(|e| e.to_string())?;
            if broker.is_some() && threads.iter().any(|(a, id)| a != agent.name() || id != "$current") {
                return Err("broker seeds must refer to the current native session".into());
            }
            serde_json::from_value::<Vec<(String, u64, String)>>(action["records"].clone()).map_err(|e| e.to_string())?;
        }
    }
    let policy_value = match opts.get("--policy") {
        Some(p) => serde_json::from_slice(&fs::read(p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
        None => json!({}),
    };
    let policy_hash = hash(policy_value.to_string().as_bytes());
    let policy = TurnPolicy::from_json(&policy_value)?;
    fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    if fs::read_dir(&home).map_err(|e| e.to_string())?.next().is_some() {
        return Err("scenario home must be empty".into());
    }
    // This process is still single-threaded. A private home must precede Session configuration.
    let home = fs::canonicalize(home).map_err(|e| e.to_string())?;
    unsafe {
        for key in ["HOME", "USERPROFILE"] { std::env::set_var(key, &home); }
        std::env::set_var("CODEX_HOME", home.join(".codex"));
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
        #[cfg(unix)]
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, cancel as *const () as libc::sighandler_t);
        }
    }
    let new_session = || -> Result<Session, String> {
        let mut s = Session::with_state_home(&project, &state, &home)?;
        s.mode = Mode::Read;
        if let Some(socket) = &broker { s.set_trial_broker(socket.clone()); }
        else { s.programs.insert(agent, program.clone()); }
        s.set_turn_policy(policy.clone());
        Ok(s)
    };
    let mut session = new_session()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut events = Vec::new();
    let mut spent = 0u64;
    // Every ordinary error or cancellation leaves through this closure, followed by shutdown.
    let result = (|| -> Result<Value, String> {
    for action in actions {
        if CANCELLED.load(Ordering::SeqCst) { return Err("scenario cancelled".into()) }
        let before = session.threads();
        let op = action["op"].as_str().unwrap();
        let mut event = json!({"op": op, "before": before});
        match op {
            "clear" => session.forget(),
            "start_over" => {
                if !session.start_over(agent) {
                    return Err("start_over requires an existing conversation".into());
                }
            }
            "seed" => {
                let mut threads = serde_json::from_value::<Vec<(String, String)>>(action["threads"].clone()).unwrap();
                if broker.is_some() {
                    for (a, id) in &mut threads {
                        *id = session.threads().iter().find(|(name, _)| name == a)
                            .map(|(_, id)| id.clone()).ok_or("seed requires an existing native session")?;
                    }
                }
                session.resume(&threads, &[],
                    &serde_json::from_value::<Vec<(String, u64, String)>>(action["records"].clone()).unwrap());
            }
            "restart" => {
                let kept = session.keep(None)?;
                event["kept_sha256"] = hash(&fs::read(&kept).map_err(|e| e.to_string())?).into();
                session.shutdown();
                session = new_session()?;
                session.go_on(&kept).ok_or("restart state was unreadable")?;
            }
            _ => {
                let message = if op == "compact" { "/compact" } else { action["message"].as_str().unwrap() };
                let id = session.launch_agent(agent, false, message)?;
                while session.job(id)?.running() {
                    session.pump();
                    if token_cap.is_some_and(|cap| spent.saturating_add(session.job(id).map_or(0, |j| j.tokens_so_far())) > cap) {
                        return Err("scenario token limit reached".into());
                    }
                    if CANCELLED.load(Ordering::SeqCst) { return Err("scenario cancelled".into()) }
                    if Instant::now() >= deadline {
                        session.shutdown();
                        return Err("scenario wall limit reached".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let job = session.job(id)?;
                spent = spent.saturating_add(job.tokens.total());
                event["done"] = matches!(job.status, Status::Done).into();
                event["reply"] = job.reply().into();
                event["usage"] = json!({"input": job.tokens.input, "cache_write": job.tokens.cache_write,
                    "cache_read": job.tokens.cache_read, "output": job.tokens.output,
                    "counted": job.counted, "uncounted_calls": job.uncounted, "calls": job.calls});
                event["job_dir"] = job.dir.to_string_lossy().to_string().into();
                if !matches!(job.status, Status::Done) {
                    session.shutdown();
                    return Err(format!("scenario turn failed: {event}"));
                }
            }
        }
        event["after"] = json!(session.threads());
        events.push(event);
    }
    Ok(json!({"offline": broker.is_none(), "transport": if broker.is_some() { "isolated-native-broker" } else { "offline" },
        "scenario_sha256": hash(&bytes), "policy_sha256": policy_hash,
        "binary_sha256": hash(&fs::read(std::env::current_exe().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?),
        "program_sha256": hash(&fs::read(&program).map_err(|e| e.to_string())?), "events": events}))
    })();
    session.shutdown();
    result
}

fn main() -> ExitCode {
    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|s| s == "--broker-client" || s == "--codex-broker-client") {
            return match broker_client(&args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("broker client: {e}"); ExitCode::from(2) }
            };
        }
    }
    match run() {
        Ok(record) => { println!("{record}"); ExitCode::SUCCESS }
        Err(reason) => { println!("{}", json!({"outcome": "refused_or_failed", "reason": reason})); ExitCode::from(2) }
    }
}

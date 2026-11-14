//! Offline scripted scenarios through real Session transitions. Never a live-model launcher.
use ringo_cli::{Agent, Mode, Session, Status, turn_policy::TurnPolicy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::ExitCode, time::{Duration, Instant}};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn cancel(_: libc::c_int) { CANCELLED.store(true, Ordering::SeqCst); }

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

fn run() -> Result<Value, String> {
    if std::env::var_os("RINGO_CHILD").is_some() { return Err("RINGO_CHILD is set".into()) }
    let mut args = std::env::args().skip(1);
    let mut opts = std::collections::BTreeMap::new();
    while let Some(key) = args.next() {
        if !["--project", "--scenario", "--program", "--state", "--home", "--policy"].contains(&key.as_str()) {
            return Err(format!("unknown option {key}"));
        }
        let value = args.next().ok_or_else(|| format!("{key} needs a value"))?;
        if opts.insert(key.clone(), value).is_some() { return Err(format!("duplicate option {key}")) }
    }
    let path = |key: &str| opts.get(key).map(PathBuf::from).ok_or_else(|| format!("{key} is required"));
    let (project, scenario, program, state, home) = (path("--project")?, path("--scenario")?,
        path("--program")?, path("--state")?, path("--home")?);
    let program = fs::canonicalize(program).map_err(|e| e.to_string())?;
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
    let seconds = spec.get("seconds").map_or(Some(60), Value::as_u64)
        .filter(|n| (1..=300).contains(n)).ok_or("seconds must be from 1 through 300")?;
    let actions = spec["actions"].as_array().filter(|a| !a.is_empty() && a.len() <= 64)
        .ok_or("actions must have from 1 through 64 entries")?;
    for action in actions {
        let obj = action.as_object().ok_or("an action must be an object")?;
        let op = action["op"].as_str().ok_or("action needs op")?;
        let keys: &[&str] = match op {
            "turn" => &["op", "message"], "clear" | "restart" => &["op"],
            "compact" if agent.name() == "claude" => &["op"],
            "seed" => &["op", "threads", "records"],
            _ => return Err(format!("unsupported action {op}")),
        };
        if obj.keys().any(|k| !keys.contains(&k.as_str())) { return Err("unknown action field".into()) }
        if op == "turn" && !action["message"].is_string() { return Err("turn needs message text".into()) }
        if op == "seed" {
            serde_json::from_value::<Vec<(String, String)>>(action["threads"].clone()).map_err(|e| e.to_string())?;
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
        s.programs.insert(agent, program.clone());
        s.set_turn_policy(policy.clone());
        Ok(s)
    };
    let mut session = new_session()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut events = Vec::new();
    // Every ordinary error or cancellation leaves through this closure, followed by shutdown.
    let result = (|| -> Result<Value, String> {
    for action in actions {
        if CANCELLED.load(Ordering::SeqCst) { return Err("scenario cancelled".into()) }
        let before = session.threads();
        let op = action["op"].as_str().unwrap();
        let mut event = json!({"op": op, "before": before});
        match op {
            "clear" => session.forget(),
            "seed" => session.resume(
                &serde_json::from_value::<Vec<(String, String)>>(action["threads"].clone()).unwrap(), &[],
                &serde_json::from_value::<Vec<(String, u64, String)>>(action["records"].clone()).unwrap()),
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
                    if CANCELLED.load(Ordering::SeqCst) { return Err("scenario cancelled".into()) }
                    if Instant::now() >= deadline {
                        session.shutdown();
                        return Err("scenario wall limit reached".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let job = session.job(id)?;
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
    Ok(json!({"offline": true, "scenario_sha256": hash(&bytes), "policy_sha256": policy_hash,
        "binary_sha256": hash(&fs::read(std::env::current_exe().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?),
        "program_sha256": hash(&fs::read(&program).map_err(|e| e.to_string())?), "events": events}))
    })();
    session.shutdown();
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(record) => { println!("{record}"); ExitCode::SUCCESS }
        Err(reason) => { println!("{}", json!({"outcome": "refused_or_failed", "reason": reason})); ExitCode::from(2) }
    }
}

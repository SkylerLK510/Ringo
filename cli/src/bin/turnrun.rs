//! `turnrun`: one isolated shell turn for offline measurement (RRSI Phase 1).
//!
//! This is the measurement runner the RRSI plan's Phase 1 asks for: it exercises the actual shell
//! path — the same `Session` conductor the TUI drives, with the same agent protocols, permission
//! gate, token accounting and handoff logic — against a prepared task environment, and writes one
//! JSON record of what the turn did and what it cost. It makes no model call of its own and takes
//! no part in improvement decisions; the Python driver (`turnrun.py`) builds fixtures, assigns
//! tasks, keeps the ledger and runs the protected outcome checks.
//!
//! What it enforces itself:
//! - A private state folder and home: the turn sees no owner configuration, history or keys
//!   (`agent_env` strips API keys and sets `RINGO_CHILD`, so an agent cannot start Ringo again).
//! - Per-run budgets: a token cap and a wall cap. The cap is enforced by polling, so it detects a
//!   breach when counts arrive: Claude reports per message (`live`), Codex and agy report at turn
//!   end (`post_turn`), a plain agent reports nothing (`none`). A post-turn cap may overshoot; the
//!   record says which detection applied. A breach stops this turn's process group only.
//! - A clean fixture: the project must be a git repository with a clean tree, so a run's starting
//!   point is pinned and a rerun cannot silently continue a half-done task.
//!
//! It does NOT claim network isolation. The agent's program is itself the transport to its
//! provider, and nothing here separates that transport from the task's own network access. So a
//! run needs `--program` (an offline fake agent): without it the run would start the installed
//! agent, a live session, and it is refused. There is no flag that turns live runs on.
//!
//! Exit codes: 0 the turn finished; 1 it failed, was cancelled, or was refused after launch;
//! 2 bad usage or refused setup; 3 stopped at a budget; 4 the record could not be written (the
//! record is the run's evidence; without it the run did not happen).

use ringo_cli::{Adapter, Agent, Mode, Session, Tier, COMMIT};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "\
turnrun — one isolated shell turn for offline measurement

usage: turnrun --project DIR --agent NAME --message TEXT | --message-file PATH
              [--program PATH] [--state DIR] [--home DIR] [--record PATH]
              [--mode ask|read|auto] [--tier quick|standard|deep] [--write]
              [--resume-json PATH] [--handoff-at N] [--source-id LABEL]
              [--budget-tokens N] [--budget-seconds N]

The turn runs in DIR (a clean git repository) with a private state folder and home.
--program runs PATH for the agent instead of its installed command, and is required:
live runs are refused until the provider transport is separated from the task's
network access. --resume-json seeds the session from a kept conversation before the
message; a file that cannot be read refuses the run. Records name every path they use; keep them
outside the repository, as the driver already does.
";

/// The flags, once parsed. `parse` only checks shapes; the run refuses what it cannot pin down.
struct Set {
    project: String,
    agent: String,
    message: String,
    program: Option<String>,
    state: Option<String>,
    home: Option<String>,
    record: Option<PathBuf>,
    mode: Option<String>,
    tier: Option<String>,
    write: bool,
    resume: Option<PathBuf>,
    handoff_at: Option<u64>,
    source_id: Option<String>,
    budget_tokens: Option<u64>,
    budget_seconds: Option<u64>,
}

fn parse(args: Vec<String>) -> Result<Set, String> {
    let mut set = Set {
        project: String::new(), agent: String::new(), message: String::new(), program: None, state: None,
        home: None, record: None, mode: None, tier: None, write: false, resume: None, handoff_at: None,
        source_id: None, budget_tokens: None, budget_seconds: None,
    };
    let mut have_message = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--project" => set.project = value("--project")?,
            "--agent" => set.agent = value("--agent")?,
            "--message" => { set.message = value("--message")?; have_message = true; }
            "--message-file" => {
                let path = value("--message-file")?;
                set.message = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
                have_message = true;
            }
            "--program" => set.program = Some(value("--program")?),
            "--state" => set.state = Some(value("--state")?),
            "--home" => set.home = Some(value("--home")?),
            "--record" => set.record = Some(PathBuf::from(value("--record")?)),
            "--mode" => set.mode = Some(value("--mode")?),
            "--tier" => set.tier = Some(value("--tier")?),
            "--write" => set.write = true,
            "--resume-json" => set.resume = Some(PathBuf::from(value("--resume-json")?)),
            "--handoff-at" => set.handoff_at = Some(value("--handoff-at")?.parse::<u64>().map_err(|_| "--handoff-at needs a number")?),
            "--source-id" => set.source_id = Some(value("--source-id")?),
            "--budget-tokens" => set.budget_tokens = Some(value("--budget-tokens")?.parse::<u64>().map_err(|_| "--budget-tokens needs a number")?),
            "--budget-seconds" => set.budget_seconds = Some(value("--budget-seconds")?.parse::<u64>().map_err(|_| "--budget-seconds needs a number")?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if set.project.is_empty() || set.agent.is_empty() {
        return Err("--project and --agent are required".into());
    }
    if !have_message {
        return Err("exactly one of --message, --message-file is required".into());
    }
    Ok(set)
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(format!("{:x}", h.finalize()))
}

/// `git -C DIR ARGS`, its stdout as a string, None when git refuses.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// How soon a cap breach can be seen for this adapter: Claude reports usage per message; Codex and
/// agy only at a turn's end; a plain agent never. The record carries this, so no one reads a
/// post-turn cap as an exact stop.
fn cap_detection(agent: Agent) -> &'static str {
    match agent.spec().adapter {
        Adapter::Claude => "live",
        Adapter::Codex | Adapter::Agy => "post_turn",
        Adapter::Plain => "none",
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A refused run is a recorded run: a parse error becomes a record, if a record path can be had.
    let run = match parse(args) {
        Ok(set) => run_set(set),
        Err(e) => Err((json!({"outcome": "refused", "reason": e}), None)),
    };
    match run {
        Ok(code) => code,
        Err((record, path)) => match write_record(record, path) {
            Ok(()) => ExitCode::from(2),
            Err(e) => {
                eprintln!("turnrun: {e}");
                ExitCode::from(4)
            }
        },
    }
}

/// Either the exit code, or a record to write before exiting 2 (a refused run is a recorded run).
type Run = Result<ExitCode, (Value, Option<PathBuf>)>;

static CANCELLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

/// Set up the process environment before Session starts threads or loads agent configuration.
/// Session's home argument alone does not change what HOME a child process inherits.
fn private_home(home: &Path) -> Result<(), String> {
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    if fs::read_dir(home).map_err(|e| e.to_string())?.next().is_some() {
        return Err("the private home must be empty before a run".into());
    }
    let home = fs::canonicalize(home).map_err(|e| e.to_string())?;
    let vars = [
        ("HOME", home.clone()), ("USERPROFILE", home.clone()),
        ("CODEX_HOME", home.join(".codex")), ("CLAUDE_CONFIG_DIR", home.join(".claude")),
        ("XDG_CONFIG_HOME", home.join(".config")), ("XDG_DATA_HOME", home.join(".local/share")),
        ("XDG_CACHE_HOME", home.join(".cache")), ("APPDATA", home.join("AppData/Roaming")),
        ("LOCALAPPDATA", home.join("AppData/Local")),
    ];
    // SAFETY: run_set is still single-threaded; Session has not been constructed yet.
    unsafe {
        for (key, value) in vars { std::env::set_var(key, value); }
    }
    Ok(())
}


fn run_set(set: Set) -> Run {
    if std::env::var_os("RINGO_CHILD").is_some() {
        // An agent Ringo launched must not start a runner that starts agents: that is the recursive
        // harness review this work avoids.
        return Err((json!({"outcome": "refused", "reason": "RINGO_CHILD is set"}), set.record));
    }
    let project = PathBuf::from(&set.project);
    if !project.is_dir() {
        return Err((json!({"outcome": "refused", "reason": format!("{} is not a folder", project.display())}), set.record));
    }
    if git(&project, &["rev-parse", "--verify", "-q", "HEAD"]).is_none() {
        return Err((json!({"outcome": "refused", "reason": "the project is not a git repository with a commit"}), set.record));
    }
    if !git(&project, &["status", "--porcelain"]).is_none_or(|s| s.is_empty()) {
        // A dirty fixture is not a pinned starting point: refuse rather than measure a moving target.
        return Err((json!({"outcome": "refused", "reason": "the project's tree is not clean"}), set.record));
    }
    let head = git(&project, &["rev-parse", "HEAD"]).unwrap_or_default();

    // Defaults are per process, beside the project. Honor either explicit path independently.
    let parent = project.parent().unwrap_or(Path::new("."));
    let state = set.state.map(PathBuf::from)
        .unwrap_or_else(|| parent.join(format!(".turnrun-state-{}", std::process::id())));
    let home = set.home.map(PathBuf::from)
        .unwrap_or_else(|| parent.join(format!(".turnrun-home-{}", std::process::id())));
    private_home(&home)
        .map_err(|e| (json!({"outcome": "refused", "reason": e}), set.record.clone()))?;
    #[cfg(unix)]
    // SAFETY: the handlers only set an atomic flag; the main loop performs shutdown and accounting.
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, cancel as *const () as libc::sighandler_t);
        }
    }
    let (agent, mode, tier) = (
        Agent::from_name(&set.agent),
        set.mode.as_deref().map_or(Some(Mode::Ask), Mode::from_name),
        set.tier.as_deref().map_or(Some(Tier::Standard), Tier::from_name),
    );
    let (Some(agent), Some(mode), Some(tier)) = (agent, mode, tier) else {
        return Err((json!({"outcome": "refused", "reason": "unknown agent, mode or tier"}), set.record));
    };
    let mut session = Session::with_state_home(&project, &state, &home)
        .map_err(|e| (json!({"outcome": "refused", "reason": format!("no session: {e}")}), set.record.clone()))?;
    session.mode = mode;
    session.tier = tier;
    if let Some(n) = set.handoff_at {
        session.handoff_at = n; // Phase 3 probes the handoff threshold through this flag
    }
    let Some(program) = &set.program else {
        return Err((json!({"outcome": "refused",
                           "reason": "--program is required: live runs are refused until the provider transport is separated from the task's network access"}),
                    set.record.clone()));
    };
    let program = PathBuf::from(program);
    if !program.is_file() {
        return Err((json!({"outcome": "refused", "reason": format!("{} is not a file", program.display())}), set.record.clone()));
    }
    if let Some(live) = agent_cli(&program) {
        return Err((json!({"outcome": "refused",
                           "reason": format!("{} is the installed {live} command: a live session, refused", program.display())}),
                    set.record.clone()));
    }
    session.programs.insert(agent, program);
    let installed = session.program(agent).to_path_buf();
    if let Some(resume) = &set.resume {
        let path = fs::canonicalize(resume)
            .map_err(|e| (json!({"outcome": "refused", "reason": format!("{}: {e}", resume.display())}), set.record.clone()))?;
        // A kept conversation handed on. A file `go_on` cannot read would leave the session fresh,
        // and the run would measure an unseeded turn under a seeded label: refused instead.
        if session.go_on(&path).is_none() {
            return Err((json!({"outcome": "refused", "reason": format!("{} is not a conversation the shell can go on from", path.display())}),
                        set.record.clone()));
        }
    }

    let base = json!({
        "runner": {"name": "turnrun", "commit": COMMIT,
                   "binary_sha256": sha256_file(&std::env::current_exe().unwrap_or_default())},
        "source_id": set.source_id.clone().unwrap_or_default(),
        "project": project.display().to_string(),
        "fixture": {"head": head, "clean": true},
        "agent": {"name": agent.name(), "program": installed.display().to_string(),
                  "program_sha256": sha256_file(&installed), "cap_detection": cap_detection(agent)},
        "mode": mode.name(), "tier": tier.name(), "write": set.write,
        "budget": {"tokens": set.budget_tokens, "seconds": set.budget_seconds},
        "resume": set.resume.as_ref().map(|p| json!({"path": p.display().to_string(), "sha256": sha256_file(p)})),
    });
    let mut outcome = "running";
    let mut stopped_by = Value::Null;
    let mut status_detail = Value::Null;

    let started = Instant::now();
    let id = match session.launch_agent(agent, set.write, &set.message) {
        Ok(id) => id,
        Err(e) => {
            return finish(base, "refused", json!({"reason": e}), started, Value::Null, None, set.record)
        }
    };
    while session.job(id).map(|j| j.running()).unwrap_or(false) {
        session.pump();
        if CANCELLED.load(Ordering::SeqCst) {
            session.shutdown();
            outcome = "cancelled";
            break;
        }
        let Ok(job) = session.job(id) else { break };
        let spent = job.tokens_so_far();
        if let Some(cap) = set.budget_tokens && spent > cap {
            // Stop this turn's process group only; never a broad kill by name.
            let _ = session.kill(id);
            outcome = "cap_breach";
            stopped_by = json!({"tokens": spent, "cap": cap});
            break;
        }
        if let Some(cap) = set.budget_seconds && started.elapsed() > Duration::from_secs(cap) {
            let _ = session.kill(id);
            outcome = "timeout";
            stopped_by = json!({"seconds": started.elapsed().as_secs(), "cap": cap});
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if outcome == "running" {
        (outcome, status_detail) = match session.job(id).map(|j| j.status.clone()) {
            Ok(ringo_cli::Status::Done) => ("done", Value::Null),
            Ok(ringo_cli::Status::Cancelled) => ("cancelled", Value::Null),
            Ok(ringo_cli::Status::Failed(e)) => ("failed", json!(e)),
            _ => ("failed", Value::Null),
        };
    }
    // A stop at a cap must not leave the job's final counts unwritten: pump until it is not
    // running, bounded, then read what was counted.
    let mut waits = 0;
    while session.job(id).is_ok_and(|j| j.running()) && waits < 250 {
        session.pump();
        std::thread::sleep(Duration::from_millis(20));
        waits += 1;
    }
    let job = match session.job(id) {
        Ok(job) => job,
        Err(e) => return finish(base, outcome, json!({"reason": e}), started, stopped_by, None, set.record),
    };
    let usage = json!({
        "input": job.tokens.input + job.live.input, "cache_write": job.tokens.cache_write + job.live.cache_write,
        "cache_read": job.tokens.cache_read + job.live.cache_read, "output": job.tokens.output + job.live.output,
        "calls": job.calls, "uncounted_calls": job.uncounted, "counted": job.counted,
    });
    let reply = job.reply();
    let mut reply_h = Sha256::new();
    reply_h.update(reply.as_bytes());
    let dir = job.dir.clone();
    let (changed, steps) = (job.changed.clone(), job.steps.len());
    session.shutdown();
    let extra = json!({
        "reason": status_detail,
        "usage": usage,
        "elapsed_s": started.elapsed().as_secs_f64(),
        "stopped_by": stopped_by,
        "changed": changed,
        "steps": steps,
        "reply_chars": reply.len(),
        "reply_sha256": format!("{:x}", reply_h.finalize()),
        "job_dir": dir.display().to_string(),
        "state_dir": session.dir.display().to_string(),
        "finished_at": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    });
    finish(base, outcome, extra, started, stopped_by, set.budget_tokens, set.record)
}

/// Which agent's installed command `program` is, if it is one: by name, or because it resolves to
/// the file PATH finds for an agent. A tripwire against an accidental live run, not isolation.
fn agent_cli(program: &Path) -> Option<String> {
    let real = fs::canonicalize(program).ok()?;
    let stem = program.file_stem()?.to_string_lossy().to_ascii_lowercase();
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd"] } else { &[""] };
    Agent::all().into_iter().map(|a| a.spec().program.clone()).find(|name| {
        let name_stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase());
        name_stem.as_deref() == Some(stem.as_str())
            || std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| {
                exts.iter().any(|e| fs::canonicalize(d.join(name).with_extension(e)).is_ok_and(|p| p == real))
            }))
    })
}

/// Builds the final record from the base and the outcome's extra fields, prints it, and maps the
/// outcome to an exit code. `session.shutdown` has already run by here (or the run was refused).
/// A turn that finished past a token cap keeps outcome `done` and carries `cap_overshoot`: with a
/// backend that reports only at a turn's end, the cap saw the breach too late to stop anything, and
/// the record must not dress that up as an exact stop.
fn finish(mut base: Value, outcome: &str, extra: Value, started: Instant,
          stopped_by: Value, token_cap: Option<u64>, record: Option<PathBuf>) -> Run {
    let usage_total = ["input", "cache_write", "cache_read", "output"].into_iter()
        .filter_map(|k| extra.pointer(&format!("/usage/{k}")).and_then(Value::as_u64)).sum::<u64>();
    let usage_known = extra.pointer("/usage/counted").and_then(Value::as_bool) == Some(true)
        && extra.pointer("/usage/uncounted_calls").and_then(Value::as_u64) == Some(0);
    if let (Value::Object(base), Value::Object(add)) = (&mut base, &extra) {
        base.extend(add.clone());
    }
    base["outcome"] = json!(outcome);
    base["stopped_by"] = stopped_by;
    base["usage_total"] = if usage_known { json!(usage_total) } else { Value::Null };
    base["elapsed_s"] = json!(started.elapsed().as_secs_f64());
    if outcome == "done" && let Some(cap) = token_cap && usage_total > cap {
        base["cap_overshoot"] = json!({"tokens": usage_total, "cap": cap});
    }
    write_record(base, record.clone())
        .map_err(|e| (json!({"outcome": outcome, "reason": format!("the record was not written: {e}")}), record))?;
    Ok(ExitCode::from(match outcome {
        "done" => 0,
        "cap_breach" | "timeout" => 3,
        _ => 1,
    }))
}

/// The record is the run's evidence: written whole, then renamed, so a reader never sees a half one.
fn write_record(mut record: Value, path: Option<PathBuf>) -> Result<(), String> {
    println!("{}", record);
    let Some(path) = path else { return Ok(()) };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    if let Value::Object(o) = &mut record {
        o.insert("record".into(), json!(path.display().to_string()));
    }
    let part = path.with_extension("json.part");
    fs::write(&part, record.to_string()).and_then(|_| fs::rename(&part, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

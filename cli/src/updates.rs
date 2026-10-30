//! Engine updates. When the shell opens, a background check says which set-up agents changed version since the last
//! session and (each feed at most once a day) which have a newer release out. `/update` runs the update for you in
//! this terminal; with `/update auto on` the shell runs package-manager updates (Homebrew, npm, `claude update`) by
//! itself when it opens. No model call is made. RINGO_NO_UPDATE_CHECK=1 turns the release lookup off.

use ringo_cli::Agent;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DAY_S: u64 = 24 * 3600;

/// An update Ringo can run: the agent, the shell command, and whether it is a package-manager update (the only
/// kind run unattended; an installer script waits for `/update`).
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub agent: Agent,
    pub command: String,
    pub unattended: bool,
    /// The version installed when the check ran.
    pub have: String,
}

/// The first x.y.z in a line such as `codex-cli 0.165.0` or `2.1.314 (Claude Code)`.
fn version(text: &str) -> Option<String> {
    regex::Regex::new(r"\d+\.\d+\.\d+").ok()?.find(text).map(|m| m.as_str().to_string())
}

fn newer(latest: &str, installed: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|n| n.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(latest) > parts(installed)
}

fn runnable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    p.is_file()
}

/// The agent's program as PATH finds it, symlinks resolved (the install method shows in the real path).
fn locate(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|d| exts.iter().map(move |e| d.join(name).with_extension(e)))
        .find(|p| runnable(p))
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
}

/// Where to ask for the newest release, and the update for this install, from where the program lives.
fn source(agent: Agent, path: &Path) -> Option<(String, Update)> {
    let package = match agent {
        Agent::Codex => "@openai/codex",
        Agent::Claude => "@anthropic-ai/claude-code",
        Agent::Grok => return None, // no public release feed to ask
    };
    let real = path.to_string_lossy().replace('\\', "/");
    // A Homebrew name comes from the path, so it goes into a shell command only if it looks like one.
    let brew = |dir: &str| real.split(dir).nth(1).and_then(|rest| rest.split('/').next())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@+".contains(c)))
        .map(str::to_string);
    let npm_feed = format!("https://registry.npmjs.org/{package}/latest");
    let (feed, command, unattended) = if let Some(cask) = brew("/Caskroom/") {
        (format!("https://formulae.brew.sh/api/cask/{cask}.json"), format!("brew upgrade --cask {cask}"), true)
    } else if let Some(formula) = brew("/Cellar/") {
        (format!("https://formulae.brew.sh/api/formula/{formula}.json"), format!("brew upgrade {formula}"), true)
    } else if real.contains("/node_modules/") || real.ends_with(".cmd") {
        (npm_feed, format!("npm install -g {package}@latest"), true) // a .cmd is npm's Windows shim
    } else if agent == Agent::Claude {
        (npm_feed, "claude update".into(), true)
    } else if cfg!(windows) {
        let script = r#"powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex""#;
        (npm_feed, script.into(), false)
    } else {
        (npm_feed, "curl -fsSL https://chatgpt.com/codex/install.sh | sh".into(), false)
    };
    Some((feed, Update { agent, command, unattended, have: String::new() }))
}

/// Runs `cmd` with stdin closed and its output in `log`; kills it past `limit`. Returns whether it exited 0.
// ceiling: kills the direct child only; a package manager's own children can outlive a timeout.
fn bounded(mut cmd: Command, log: &Path, limit: Duration) -> Option<bool> {
    let out = fs::File::create(log).ok()?;
    let mut child = cmd.stdin(Stdio::null()).stdout(out.try_clone().ok()?).stderr(out).spawn().ok()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status.success());
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn fetch_version(url: &str) -> Option<String> {
    let out = Command::new("curl").args(["-fsSL", "-m", "5", url]).stdin(Stdio::null()).stderr(Stdio::null())
        .output().ok().filter(|o| o.status.success())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("version").or_else(|| v.get("versions")?.get("stable"))?.as_str().and_then(version)
}

/// `PROGRAM --version`, given 10 seconds: an agent that hangs is stopped and skipped, not waited on.
fn installed_version(path: &Path) -> Option<String> {
    let log = std::env::temp_dir().join(format!("ringo-version-{}-{:?}.txt", std::process::id(),
                                                std::thread::current().id()));
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let ok = bounded(cmd, &log, Duration::from_secs(10)).is_some();
    let text = fs::read_to_string(&log).unwrap_or_default();
    let _ = fs::remove_file(&log);
    version(&text).filter(|_| ok)
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load(state: &Path) -> serde_json::Value {
    let mut memo: serde_json::Value = fs::read(state).ok().and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    for key in ["installed", "latest"] {
        if !memo[key].is_object() {
            memo[key] = serde_json::json!({}); // a damaged file starts over instead of failing
        }
    }
    memo
}

/// Written whole and renamed into place, so two shells opening at once cannot leave half a file.
fn save(state: &Path, memo: &serde_json::Value) {
    let tmp = state.with_extension(format!("tmp{}", std::process::id()));
    if serde_json::to_vec_pretty(memo).ok().is_some_and(|text| fs::write(&tmp, text).is_ok()) {
        let _ = fs::rename(&tmp, state);
    }
}

/// One line per agent that changed since the last session or has a newer release, and the updates that would bring
/// each outdated agent up to date. `state` holds the installed versions and each feed's last answer; it is rewritten.
pub fn check(agents: &[Agent], state: &Path) -> (Vec<String>, Vec<Update>) {
    let now = now_s();
    let mut memo = load(state);
    let releases = std::env::var_os("RINGO_NO_UPDATE_CHECK").is_none();
    let (mut lines, mut pending) = (Vec::new(), Vec::new());
    for &agent in agents {
        let name = agent.name();
        let Some(path) = locate(name) else { continue };
        let Some(have) = installed_version(&path) else { continue };
        if let Some(before) = memo["installed"][name].as_str().filter(|b| *b != have) {
            lines.push(format!("  {name} changed {before} -> {have} since the last session; answers and token use \
                                can change with it"));
        }
        memo["installed"][name] = have.clone().into();
        let Some((feed, update)) = source(agent, &path).filter(|_| releases) else { continue };
        if memo["latest"][&feed]["checked"].as_u64().is_none_or(|t| now.saturating_sub(t) >= DAY_S) {
            match fetch_version(&feed) {
                Some(out) => memo["latest"][&feed] = serde_json::json!({"version": out, "checked": now}),
                None => continue, // offline or the feed moved: say nothing, ask this feed again next time
            }
        }
        if let Some(out) = memo["latest"][&feed]["version"].as_str().filter(|out| newer(out, &have)) {
            lines.push(format!("  {name} {out} is out (you have {have}); /update installs it ({})", update.command));
            pending.push(Update { have: have.clone(), ..update });
        }
    }
    save(state, &memo);
    (lines, pending)
}

fn shell(command: &str) -> Command {
    let mut cmd = Command::new(if cfg!(windows) { "cmd" } else { "sh" });
    cmd.args([if cfg!(windows) { "/C" } else { "-c" }, command]);
    cmd
}

/// What an agent reports after its update ran, and whether it is now newer than when the check ran.
fn after(update: &Update, ran: Option<bool>, log: Option<&Path>) -> (String, bool) {
    let name = update.agent.name();
    let now = locate(name).and_then(|p| installed_version(&p));
    let output = log.map_or("its output is above the shell".into(), |l| format!("its output is in {}", l.display()));
    let line = match (ran, &now) {
        (Some(true), Some(v)) if newer(v, &update.have) => format!("  {name} updated: {} -> {v}", update.have),
        (Some(true), Some(v)) => format!("  {name}: `{}` ran, but {name} is still {v} (the release may not have \
                                          reached that installer yet)", update.command),
        (Some(false), _) => format!("  {name}: `{}` failed; {output}", update.command),
        (None, _) => format!("  {name}: `{}` was stopped after 10 minutes; {output}", update.command),
        (Some(true), None) => {
            format!("  {name}: `{}` finished, but `{name} --version` no longer answers", update.command)
        }
    };
    (line, now.is_some_and(|v| newer(&v, &update.have)))
}

/// Runs package-manager updates unattended (for `/update auto on`); output goes to `engine-update-NAME.log` beside
/// `state`. Returns what happened and every update still to do: installer scripts (left for `/update`) and any
/// that did not bring a newer version.
pub fn apply_unattended(pending: Vec<Update>, state: &Path) -> (Vec<String>, Vec<Update>) {
    let (mut lines, mut left) = (Vec::new(), Vec::new());
    for update in pending {
        if !update.unattended {
            left.push(update);
            continue;
        }
        let log = state.with_file_name(format!("engine-update-{}.log", update.agent.name()));
        let (line, done) = after(&update, bounded(shell(&update.command), &log, Duration::from_secs(600)), Some(&log));
        lines.push(line);
        if !done {
            left.push(update);
        }
    }
    (lines, left)
}

/// `/update`: runs each update in this terminal (the shell steps aside, as for /setup), so prompts and progress show.
/// Returns what happened and the updates that did not bring a newer version.
pub fn apply_here(pending: Vec<Update>) -> (Vec<String>, Vec<Update>) {
    let (mut lines, mut left) = (Vec::new(), Vec::new());
    for update in pending {
        println!("\n$ {}", update.command);
        let (line, done) = after(&update, shell(&update.command).status().ok().map(|s| s.success()), None);
        lines.push(line);
        if !done {
            left.push(update);
        }
    }
    (lines, left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions_and_order() {
        assert_eq!(version("codex-cli 0.165.0").as_deref(), Some("0.165.0"));
        assert_eq!(version("2.1.314 (Claude Code)").as_deref(), Some("2.1.314"));
        assert_eq!(version("no version"), None);
        assert!(newer("0.166.0", "0.165.0"));
        assert!(newer("2.1.314", "2.1.99"));
        assert!(!newer("2.1.314", "2.1.314"));
        assert!(!newer("0.165.0", "0.166.0"));
    }

    #[test]
    fn picks_the_install_method() {
        let at = |agent, path: &str| source(agent, Path::new(path)).unwrap();
        let (feed, update) = at(Agent::Codex, "/opt/homebrew/Caskroom/codex/0.165.0/bin/codex");
        assert_eq!(feed, "https://formulae.brew.sh/api/cask/codex.json");
        assert_eq!((update.command.as_str(), update.unattended), ("brew upgrade --cask codex", true));
        let (feed, update) = at(Agent::Claude, "/opt/homebrew/Caskroom/claude-code/2.1.314/claude");
        assert_eq!(feed, "https://formulae.brew.sh/api/cask/claude-code.json");
        assert_eq!(update.command, "brew upgrade --cask claude-code");
        assert_eq!(at(Agent::Codex, "/opt/homebrew/Cellar/codex/0.165.0/bin/codex").1.command, "brew upgrade codex");
        assert_eq!(at(Agent::Codex, "/usr/local/lib/node_modules/@openai/codex/bin/codex.js").1.command,
                   "npm install -g @openai/codex@latest");
        let shim = at(Agent::Codex, r"C:\Users\u\AppData\Roaming\npm\codex.cmd");
        assert_eq!(shim.1.command, "npm install -g @openai/codex@latest");
        assert_eq!(at(Agent::Claude, "/Users/u/.local/share/claude/versions/2.1.314").1.command, "claude update");
        let native = at(Agent::Codex, "/Users/u/.local/bin/codex").1;
        assert!(native.command.contains("chatgpt.com/codex/install") && !native.unattended);
        assert!(source(Agent::Grok, Path::new("/usr/local/bin/grok")).is_none());
        // A path segment that is not a Homebrew name never reaches a shell command.
        let odd = at(Agent::Codex, "/opt/homebrew/Caskroom/codex;rm -rf ~/1/bin/codex").1;
        assert!(!odd.command.contains("rm -rf"));
    }

    #[test]
    fn a_damaged_state_file_starts_over() {
        let dir = std::env::temp_dir().join(format!("ringo-updates-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = dir.join("engine-versions.json");
        fs::write(&state, r#"{"installed": 3, "latest": []}"#).unwrap();
        assert_eq!(check(&[], &state), (vec![], vec![]));
        let memo: serde_json::Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert!(memo["installed"].is_object() && memo["latest"].is_object());
        fs::write(&state, "[1, 2]").unwrap();
        assert_eq!(check(&[], &state), (vec![], vec![]));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_update_and_a_failed_one_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("ringo-bounded-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("out.log");
        assert_eq!(bounded(shell("echo hi"), &log, Duration::from_secs(5)), Some(true));
        assert_eq!(fs::read_to_string(&log).unwrap(), "hi\n");
        assert_eq!(bounded(shell("exit 3"), &log, Duration::from_secs(5)), Some(false));
        let start = Instant::now();
        assert_eq!(bounded(shell("sleep 5"), &log, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(3));
        let _ = fs::remove_dir_all(dir);
    }
}

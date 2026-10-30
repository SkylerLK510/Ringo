//! Engine updates: when the shell opens it says which set-up agents changed version since the last session, and
//! (at most once a day) which have a newer release out, with the command that updates them. No model call is made
//! and Ringo never updates an agent itself: a new engine version can change answers and token use.
//! RINGO_NO_UPDATE_CHECK=1 turns the release lookup off.

use ringo_cli::Agent;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_S: u64 = 24 * 3600;

/// The first x.y.z in a line such as `codex-cli 0.165.0` or `2.1.314 (Claude Code)`.
fn version(text: &str) -> Option<String> {
    regex::Regex::new(r"\d+\.\d+\.\d+").ok()?.find(text).map(|m| m.as_str().to_string())
}

fn newer(latest: &str, installed: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|n| n.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(latest) > parts(installed)
}

/// The agent's program on PATH, symlinks resolved (the install method shows in the real path).
fn locate(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|d| exts.iter().map(move |e| d.join(name).with_extension(e)))
        .find(|p| p.is_file())
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
}

/// Where to ask for the newest release, the JSON field holding it, and the command that updates this install.
fn source(agent: Agent, path: &Path) -> Option<(String, String)> {
    let package = match agent {
        Agent::Codex => "@openai/codex",
        Agent::Claude => "@anthropic-ai/claude-code",
        Agent::Grok => return None, // no public release feed to ask
    };
    let real = path.to_string_lossy().replace('\\', "/");
    let name = agent.name();
    Some(if real.contains("/Caskroom/") {
        (format!("https://formulae.brew.sh/api/cask/{name}.json"), format!("brew upgrade --cask {name}"))
    } else if agent == Agent::Claude && !real.contains("node_modules") {
        (format!("https://registry.npmjs.org/{package}/latest"), "claude update".into())
    } else {
        (format!("https://registry.npmjs.org/{package}/latest"), format!("npm install -g {package}@latest"))
    })
}

fn fetch_version(url: &str) -> Option<String> {
    let out = Command::new("curl").args(["-fsSL", "-m", "5", url]).stdin(Stdio::null()).stderr(Stdio::null())
        .output().ok().filter(|o| o.status.success())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("version")?.as_str().and_then(version)
}

fn installed_version(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("--version").stdin(Stdio::null()).output().ok()?;
    version(&String::from_utf8_lossy(&out.stdout))
}

/// One line per agent that changed since the last session or has a newer release. `state` is the remembered
/// file (installed versions, plus the release versions and when they were looked up); it is rewritten.
pub fn notices(agents: &[Agent], state: &Path) -> Vec<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut memo: serde_json::Value = fs::read(state).ok().and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    for key in ["installed", "latest"] {
        if !memo[key].is_object() {
            memo[key] = serde_json::json!({}); // a damaged file starts over instead of failing
        }
    }
    let releases = std::env::var_os("RINGO_NO_UPDATE_CHECK").is_none();
    let lookup = releases && memo["checked"].as_u64().is_none_or(|t| now.saturating_sub(t) >= DAY_S);
    let mut lines = Vec::new();
    for &agent in agents {
        let name = agent.name();
        let Some(path) = locate(name) else { continue };
        let Some(have) = installed_version(&path) else { continue };
        if let Some(before) = memo["installed"][name].as_str().filter(|b| *b != have) {
            lines.push(format!("  {name} changed {before} -> {have} since the last session; answers and token use \
                                can change with it"));
        }
        memo["installed"][name] = have.clone().into();
        let Some((url, update)) = source(agent, &path).filter(|_| releases) else { continue };
        if lookup {
            match fetch_version(&url) {
                Some(out) => memo["latest"][&url] = out.into(),
                None => continue, // offline or the feed moved: say nothing, look again next time
            }
        }
        if let Some(out) = memo["latest"][&url].as_str().filter(|out| newer(out, &have)) {
            lines.push(format!("  {name} {out} is out (you have {have}); update with: {update}"));
        }
    }
    if lookup {
        memo["checked"] = now.into();
    }
    if let Ok(text) = serde_json::to_vec_pretty(&memo) {
        let _ = fs::write(state, text);
    }
    lines
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
        let cask = source(Agent::Codex, Path::new("/opt/homebrew/Caskroom/codex/0.165.0/bin/codex")).unwrap();
        assert_eq!(cask, ("https://formulae.brew.sh/api/cask/codex.json".into(), "brew upgrade --cask codex".into()));
        let npm = source(Agent::Codex, Path::new("/usr/local/lib/node_modules/@openai/codex/bin/codex.js")).unwrap();
        assert_eq!(npm.1, "npm install -g @openai/codex@latest");
        let native = source(Agent::Claude, Path::new("/Users/u/.local/share/claude/versions/2.1.314")).unwrap();
        assert_eq!(native.1, "claude update");
        assert!(source(Agent::Grok, Path::new("/usr/local/bin/grok")).is_none());
    }

    #[test]
    fn a_damaged_state_file_starts_over() {
        let dir = std::env::temp_dir().join(format!("ringo-updates-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = dir.join("engine-versions.json");
        // A recent lookup stamp means no network; the wrong-typed entries are replaced, not a panic.
        fs::write(&state, r#"{"checked": 18446744073709551615, "installed": 3, "latest": []}"#).unwrap();
        assert!(notices(&[], &state).is_empty());
        let memo: serde_json::Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        assert_eq!(memo["checked"].as_u64(), Some(u64::MAX));
        assert!(memo["installed"].is_object() && memo["latest"].is_object());
        fs::write(&state, "[1, 2]").unwrap();
        assert!(notices(&[], &state).is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}

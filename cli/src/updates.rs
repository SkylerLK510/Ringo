//! Engine updates. When the shell opens, a background check says which set-up agents changed version since the last
//! session and (each feed at most once a day) which have a newer release out. `/update` runs the update for you in
//! this terminal; with `/update auto on` the shell runs package-manager updates (Homebrew, npm, `claude update`) by
//! itself when it opens. No model call is made. RINGO_NO_UPDATE_CHECK=1 turns the release lookup off.
//!
//! Ringo updates itself too (`update_ringo`): the newest `main` is built in the background and put where this
//! binary is, and the shell says `/restart` applies it.

use ringo_cli::Agent;
use std::fs;
use std::ffi::OsStr;
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
    let spec = agent.spec();
    let package = spec.npm.as_deref()?; // no npm package: no public release feed to ask
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
    } else if let Some(own) = &spec.self_update {
        (npm_feed, own.clone(), true)
    } else {
        (npm_feed, spec.installer.clone()?, false)
    };
    Some((feed, Update { agent, command, unattended, have: String::new() }))
}

/// Runs `cmd` with stdin closed and its output in `log`; kills it past `limit`. Returns whether it exited 0.
// ceiling: kills the direct child only; a package manager's own children can outlive a timeout.
fn bounded(mut cmd: Command, log: &Path, limit: Duration) -> Option<bool> {
    // A fresh file, never one already there: create_new does not follow a planted symlink.
    let _ = fs::remove_file(log);
    let out = fs::OpenOptions::new().write(true).create_new(true).open(log).ok()?;
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
    let log = ringo_cli::state_root().join(format!("version-probe-{}-{:?}.txt", std::process::id(),
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
    for key in ["installed", "latest", "stamp"] {
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

/// The program's version, from `--version` only when the program changed since the last check: a new path
/// (Homebrew and Claude keep each version in a folder of its own) or a new modification time (an install over it).
/// Otherwise the last answer in `memo` stands, and no process starts.
fn version_of(memo: &mut serde_json::Value, name: &str, path: &Path) -> Option<String> {
    let stamp = fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|t| format!("{}\t{}", path.display(), t.as_nanos()));
    let known = memo["installed"][name].as_str().filter(|_| stamp.is_some() && memo["stamp"][name].as_str() == stamp.as_deref());
    let have = known.map(str::to_string).or_else(|| installed_version(path))?;
    memo["stamp"][name] = stamp.into();
    memo["installed"][name] = have.clone().into();
    Some(have)
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
        let Some(path) = locate(&agent.spec().program) else { continue };
        let before = memo["installed"][name].as_str().map(str::to_string);
        let Some(have) = version_of(&mut memo, name, &path) else { continue };
        if let Some(before) = before.filter(|b| *b != have) {
            lines.push(format!("  {name} changed {before} -> {have} since the last session; answers and token use \
                                can change with it"));
        }
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
    let now = locate(&update.agent.spec().program).and_then(|p| installed_version(&p));
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

// Ringo's own update.

/// git with no prompt: a background fetch that would ask for a password or passphrase fails instead (ssh asks on the
/// terminal itself, over the shell's screen).
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

/// A local git question's answer, or None when git says no.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_cmd(dir).args(args).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ringo's own update, in the background: the newest `main` from where this binary came, built in a folder of its
/// own (`install/COMMIT` in Ringo's state, its prompts and pipelines with it) and put where this binary is, so the
/// next `ringo` and `/restart` run it. An open shell keeps its own version's files until it restarts. Only a binary
/// built from a clean checkout of a commit on `main` updates: one with changes of its own, from a branch ahead of
/// main, or run with RINGO_HOME is the owner's to update. The line to show, or None when there is nothing to say.
pub fn update_ringo(exe: &Path) -> Option<String> {
    if std::env::var_os("RINGO_NO_UPDATE_CHECK").is_some() || std::env::var_os("RINGO_HOME").is_some()
        || exe.as_os_str().is_empty() {
        return None;
    }
    if cfg!(windows) {
        let _ = fs::remove_file(exe.with_extension("old")); // the one the last update moved aside
    }
    update_from(ringo_cli::COMMIT, &ringo_cli::ringo_home(), &ringo_cli::state_root().join("install"), exe,
                OsStr::new("cargo"))
}

fn update_from(commit: &str, home: &Path, install: &Path, exe: &Path, cargo: &OsStr) -> Option<String> {
    if commit.is_empty() || git(home, &["rev-parse", "HEAD"])? != commit
        || !git(home, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return None;
    }
    fs::create_dir_all(install).ok()?;
    let (repo, log) = (install.join("repo.git"), install.join("update.log"));
    if !repo.join("HEAD").is_file() {
        // Ringo's own copy of the history, so no checkout of the owner's is ever written to
        let url = git(home, &["remote", "get-url", "origin"])?;
        let tmp = install.join(format!("repo-{}.tmp", std::process::id()));
        let mut clone = git_cmd(install);
        clone.args(["clone", "--bare", "--quiet", &url]).arg(&tmp);
        if bounded(clone, &log, Duration::from_secs(120)) != Some(true) || fs::rename(&tmp, &repo).is_err() {
            let _ = fs::remove_dir_all(&tmp); // offline, or another shell made it first
        }
    }
    let mut fetch = git_cmd(&repo);
    fetch.args(["fetch", "--quiet", "origin", "+refs/heads/main:refs/heads/main"]);
    if bounded(fetch, &log, Duration::from_secs(60)) != Some(true) {
        return None; // offline: say nothing, ask again next time
    }
    let new = git(&repo, &["rev-parse", "main"])?;
    if new == commit || git(&repo, &["merge-base", "--is-ancestor", commit, &new]).is_none() {
        return None; // up to date, or this build is not on main
    }
    let changes = git(&repo, &["rev-list", "--count", &format!("{commit}..{new}")]).unwrap_or_default();
    let short = &new[..new.len().min(7)];
    let dir = install.join(&new);
    let bin = dir.join(if cfg!(windows) { "ringo.exe" } else { "ringo" });
    if !bin.is_file() {
        // one shell builds a version; another one that finds it building leaves it be (a lock older than the
        // build's limit is from a shell that ended mid-build)
        let lock = install.join(format!("{new}.lock"));
        if fs::create_dir(&lock).is_err() {
            let age = fs::metadata(&lock).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if age.is_none_or(|a| a < BUILD_LIMIT) {
                return None;
            }
            let _ = fs::remove_dir(&lock);
            fs::create_dir(&lock).ok()?;
        }
        let built = build(&repo, &new, &dir, &bin, install, cargo);
        let _ = fs::remove_dir(&lock);
        if let Err(why) = built {
            return Some(format!("  Ringo {short} is out, but it did not build: {why}"));
        }
    }
    if fs::read(exe).ok() != fs::read(&bin).ok()
        && let Err(e) = replace(exe, &bin) {
        return Some(format!("  Ringo {short} is built ({}), but {} could not be replaced: {e}", bin.display(),
                            exe.display()));
    }
    prune(&repo, install);
    let s = if changes == "1" { "" } else { "s" };
    Some(format!("  Ringo updated to {short} ({changes} new change{s}) · /restart to apply"))
}

const BUILD_LIMIT: Duration = Duration::from_secs(1800);

/// The version's own files (a worktree of Ringo's copy of the history) and its binary, built at low priority with
/// the build folder every version shares, so only Ringo itself compiles again.
fn build(repo: &Path, new: &str, dir: &Path, bin: &Path, install: &Path, cargo: &OsStr) -> Result<(), String> {
    let _ = git(repo, &["worktree", "remove", "--force", &dir.to_string_lossy()]); // a failed try's
    let _ = fs::remove_dir_all(dir);
    let _ = git(repo, &["worktree", "prune"]);
    git(repo, &["worktree", "add", "--detach", "--force", "--quiet", &dir.to_string_lossy(), new])
        .ok_or("git could not check it out")?;
    let mut cmd = if cfg!(unix) { Command::new("nice") } else { Command::new(cargo) };
    if cfg!(unix) {
        cmd.args(["-n", "10"]).arg(cargo);
    }
    let target = install.join("target");
    cmd.args(["build", "--release", "--locked", "--quiet", "--manifest-path"]).arg(dir.join("cli").join("Cargo.toml"))
        .env("RINGO_COMMIT", new).env("RINGO_BUILD_HOME", dir).env("CARGO_TARGET_DIR", &target);
    let log = install.join("build.log");
    match bounded(cmd, &log, BUILD_LIMIT) {
        Some(true) => {}
        Some(false) => return Err(format!("cargo failed; its output is in {}", log.display())),
        None => return Err("cargo did not start or did not finish in 30 minutes".into()),
    }
    let out = target.join("release").join(bin.file_name().unwrap_or_default());
    fs::copy(&out, bin).map(|_| ()).map_err(|e| format!("{}: {e}", out.display()))
}

/// Puts `new` where `exe` is, renamed into place whole, so a `ringo` starting meanwhile runs one or the other.
/// Windows cannot write over a running program but can rename it, so the old one moves aside first.
fn replace(exe: &Path, new: &Path) -> std::io::Result<()> {
    let tmp = exe.with_extension(format!("new{}", std::process::id()));
    fs::copy(new, &tmp)?; // the permissions come with it
    let old = exe.with_extension("old");
    if cfg!(windows) {
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old)?;
    }
    fs::rename(&tmp, exe).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
        if cfg!(windows) {
            let _ = fs::rename(&old, exe);
        }
    })
}

/// Keeps the three newest versions (an open shell may still run one of them) and removes the rest.
fn prune(repo: &Path, install: &Path) {
    let mut versions: Vec<(SystemTime, PathBuf)> = fs::read_dir(install).into_iter().flatten().flatten()
        .filter(|e| e.file_name().len() >= 40 && e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_hexdigit()))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    versions.sort_by_key(|v| std::cmp::Reverse(v.0));
    for (_, dir) in versions.into_iter().skip(3) {
        let _ = git(repo, &["worktree", "remove", "--force", &dir.to_string_lossy()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ringo_updates_itself_from_main_and_only_from_main() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ringo-self-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (origin, home, install, exe) = (dir.join("origin"), dir.join("home"), dir.join("install"), dir.join("bin/ringo"));
        let run = |at: &Path, args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(at).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)
                .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
            assert!(ok, "git {args:?}");
        };
        let commit = |at: &Path, text: &str| {
            fs::write(at.join("a.txt"), text).unwrap();
            run(at, &["add", "."]);
            run(at, &["commit", "-qm", text]);
            git(at, &["rev-parse", "HEAD"]).unwrap()
        };
        fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "-q", "-b", "main"]);
        let a = commit(&origin, "one");
        run(&dir, &["clone", "-q", &origin.to_string_lossy(), "home"]);
        let b = commit(&origin, "two");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "old").unwrap();
        // a cargo that "builds" a binary naming the commit and home it was given, and counts its builds
        let cargo = dir.join("cargo");
        fs::write(&cargo, format!("#!/bin/sh\necho x >> '{}/builds'\nmkdir -p \"$CARGO_TARGET_DIR/release\"\n\
            printf 'new %s %s' \"$RINGO_COMMIT\" \"$RINGO_BUILD_HOME\" > \"$CARGO_TARGET_DIR/release/ringo\"\n", dir.display())).unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let update = |commit: &str| update_from(commit, &home, &install, &exe, cargo.as_os_str());
        let builds = || fs::read_to_string(dir.join("builds")).map(|t| t.lines().count()).unwrap_or(0);

        let said = format!("  Ringo updated to {} (1 new change) · /restart to apply", &b[..7]);
        assert_eq!(update(&a).as_deref(), Some(said.as_str()));
        assert_eq!(fs::read_to_string(&exe).unwrap(), format!("new {b} {}", install.join(&b).display()),
                   "the new binary is where this one was, and its prompts are its own version's");
        assert_eq!(fs::read_to_string(install.join(&b).join("a.txt")).unwrap(), "two");
        assert_eq!(update(&a).as_deref(), Some(said.as_str()), "said again (the shell shows a line once)");
        assert_eq!(builds(), 1, "a version is built once");
        assert_eq!(update(&b), None, "up to date");
        assert_eq!(fs::read_to_string(home.join("a.txt")).unwrap(), "one", "the owner's checkout is never written");

        fs::write(home.join("a.txt"), "mine").unwrap();
        assert_eq!(update(&a), None, "a checkout with changes of its own is the owner's to update");
        let c = commit(&home, "mine");
        assert_eq!(update(&c), None, "a build ahead of main is the owner's too");
        assert_eq!(update(&a), None, "and one whose checkout moved on since it was built");
        let _ = fs::remove_dir_all(&dir);
    }

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
        let (codex, claude) = (Agent::from_name("codex").unwrap(), Agent::from_name("claude").unwrap());
        let (feed, update) = at(codex, "/opt/homebrew/Caskroom/codex/0.165.0/bin/codex");
        assert_eq!(feed, "https://formulae.brew.sh/api/cask/codex.json");
        assert_eq!((update.command.as_str(), update.unattended), ("brew upgrade --cask codex", true));
        let (feed, update) = at(claude, "/opt/homebrew/Caskroom/claude-code/2.1.314/claude");
        assert_eq!(feed, "https://formulae.brew.sh/api/cask/claude-code.json");
        assert_eq!(update.command, "brew upgrade --cask claude-code");
        assert_eq!(at(codex, "/opt/homebrew/Cellar/codex/0.165.0/bin/codex").1.command, "brew upgrade codex");
        assert_eq!(at(codex, "/usr/local/lib/node_modules/@openai/codex/bin/codex.js").1.command,
                   "npm install -g @openai/codex@latest");
        let shim = at(codex, r"C:\Users\u\AppData\Roaming\npm\codex.cmd");
        assert_eq!(shim.1.command, "npm install -g @openai/codex@latest");
        assert_eq!(at(claude, "/Users/u/.local/share/claude/versions/2.1.314").1.command, "claude update");
        let native = at(codex, "/Users/u/.local/bin/codex").1;
        assert!(native.command.contains("chatgpt.com/codex/install") && !native.unattended);
        assert!(source(Agent::from_name("grok").unwrap(), Path::new("/usr/local/bin/grok")).is_none());
        assert!(source(Agent::from_name("agy").unwrap(), Path::new("/opt/homebrew/bin/agy")).is_none());
        // A path segment that is not a Homebrew name never reaches a shell command.
        let odd = at(codex, "/opt/homebrew/Caskroom/codex;rm -rf ~/1/bin/codex").1;
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
    fn the_version_is_asked_again_only_when_the_program_changed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ringo-stamp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (bin, calls) = (dir.join("codex"), dir.join("calls"));
        let write = |version: &str| {
            fs::write(&bin, format!("#!/bin/sh\necho x >> '{}'\necho 'codex-cli {version}'\n", calls.display())).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        };
        let probes = || fs::read_to_string(&calls).map(|t| t.lines().count()).unwrap_or(0);
        fs::create_dir_all(ringo_cli::state_root()).unwrap(); // the probe's output goes there, as the shell has made it
        let mut memo = load(&dir.join("none.json"));
        write("0.165.0");
        assert_eq!(version_of(&mut memo, "codex", &bin).as_deref(), Some("0.165.0"));
        assert_eq!(version_of(&mut memo, "codex", &bin).as_deref(), Some("0.165.0"));
        assert_eq!(probes(), 1, "an unchanged program is not asked again");
        std::thread::sleep(Duration::from_millis(20));
        write("0.166.0"); // installed over it: a new modification time
        assert_eq!(version_of(&mut memo, "codex", &bin).as_deref(), Some("0.166.0"));
        assert_eq!(probes(), 2);
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

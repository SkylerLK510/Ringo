//! `ringo`: with no arguments on a terminal, the shell; `review`, `design` and `work` run in this binary's native
//! runtime unless a release decides otherwise (then, and for the other commands, Ringo's Python harness `ringo.py`
//! runs).

mod connect;
mod doctor;
mod highlight;
mod mascot_art;
mod setup;
mod theme;
mod tui;
mod updates;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
ringo — your coding agents, together in one terminal.

Interactive shell:
  ringo                     open the shell (on a terminal); type /help there
  ringo --resume | -r       resume a previous conversation in this folder

Subscriptions, access & updates:
  ringo setup               connect, re-sign in, or remove coding agents (claude, codex, agy, grok)
  ringo connect             manage connected services (GitHub, Git, ClickUp...) and web access (yes/no)
  ringo doctor | status     check connected subscriptions, billing hygiene, Python/git & limits (--json, --limits)
  ringo update              check and apply updates for Ringo, connected agents & pipelines (--check, --json, --auto on|off)

Pipelines:
  ringo review|design|work  run a Ringo pipeline natively (with release state, ringo.py selects the runtime)
  ringo research ...        run Ringo's source-verified research pipeline (Python runtime)
  ringo wait|release ...    wait on a traced run, or stage/activate/rollback local releases
  ringo tools [--repo PATH] [--state DIR]
                            one read-only task/evidence JSON request on stdin (no model call or mutation)

Options:
  -h, --help                show this help message
  -V, --version             show Ringo's version and commit

Examples:
  ringo                     start an interactive session in the current project
  ringo doctor --limits     verify Claude/Codex subscription logins and live quota windows
  ringo update --check      check for Ringo and agent CLI updates without modifying anything

Prompts and plans come from RINGO_HOME (default: the checkout ringo was built from).
";

/// Known top-level subcommands, used for fuzzy "did you mean" suggestions on typos.
const KNOWN_COMMANDS: &[&str] = &[
    "setup", "connect", "doctor", "status", "update", "review", "design", "work", "research", "wait", "release", "tools", "version",
];

/// Levenshtein edit distance between two ASCII/UTF-8 strings (case-insensitive).
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Closest candidate within `max_dist` edits, if one exists.
pub(crate) fn closest_match<'a>(typed: &str, candidates: &[&'a str], max_dist: usize) -> Option<&'a str> {
    if typed.is_empty() {
        return None;
    }
    candidates
        .iter()
        .copied()
        .map(|c| (c, edit_distance(typed, c)))
        .filter(|(_, d)| *d > 0 && *d <= max_dist)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // Shell agents can request bounded reviews/designs; pipeline models cannot launch more pipelines.
    let command = args.first().and_then(|a| a.to_str());
    let allowed = command == Some("ask-server") || command == Some("tools") ||
        (std::env::var_os("RINGO_PIPELINE_CHILD").is_none() && matches!(command, Some("review" | "design" | "wait")));
    if std::env::var_os("RINGO_CHILD").is_some() && !allowed {
        eprintln!("ringo: refusing recursive execution (RINGO_CHILD); shell agents may use review, design or wait");
        return ExitCode::from(2);
    }
    match args.first().and_then(|a| a.to_str()) {
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => tui::run(tui::Start::New),
        Some("--resume" | "-r") if args.len() == 1 && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            tui::run(tui::Start::Pick)
        }
        // what /restart hands the next shell
        Some("--resume") if args.len() == 2 => tui::run(tui::Start::Restarted(args[1].clone().into())),
        None => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Some("-h" | "--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("setup") => {
            setup::run();
            ExitCode::SUCCESS
        }
        Some("connect") => connect::run_cli(&args[1..]),
        Some("doctor" | "status") => doctor::run_cli(&args[1..]),
        Some("update") => updates::run_cli(&args[1..]),
        Some("-V" | "--version" | "version") => {
            match &ringo_cli::COMMIT[..ringo_cli::COMMIT.len().min(7)] {
                "" => println!("ringo {}", env!("CARGO_PKG_VERSION")),
                commit => println!("ringo {} ({commit})", env!("CARGO_PKG_VERSION")),
            }
            ExitCode::SUCCESS
        }
        Some("wait") => {
            let rest: Vec<String> = args[1..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
            ExitCode::from(ringo_cli::rt::wait::main(&rest) as u8)
        }
        // `ringo ask-server DIR PEER...`: serves one turn's asks of other agents; an agent's program runs it, not a person
        Some("ask-server") => {
            let rest: Vec<String> = args[1..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
            let Some((dir, peers)) = rest.split_first() else {
                eprintln!("usage: ringo ask-server DIR PEER...");
                return ExitCode::from(2);
            };
            match ringo_cli::serve_asks(std::path::Path::new(dir), peers, std::io::stdin().lock(), std::io::stdout().lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            }
        }
        Some("review" | "design" | "work") => pipeline(args),
        Some("tools") => tools(&args[1..]),
        Some("research" | "release") => harness(args),
        Some(other) if !other.starts_with('-') => {
            if let Some(hint) = closest_match(other, KNOWN_COMMANDS, 2) {
                eprintln!("ringo: unknown command '{other}'. Did you mean '{hint}'?\nRun `ringo --help` for usage.");
                ExitCode::from(2)
            } else {
                harness(args)
            }
        }
        _ => harness(args),
    }
}

/// `ringo tools [--repo PATH] [--state DIR]`: one read-only gateway request on stdin, one JSON response on stdout.
/// This is a transport for agents, not a pipeline: it starts no model, shell or nested Ringo run and mutates no task.
/// A structured refusal still exits 0; a host reads the response's `ok`, as the protocol says.
fn tools(args: &[OsString]) -> ExitCode {
    use ringo_cli::gateway::{Gateway, MAX_REQUEST_BYTES};
    use std::io::Read;

    let (mut repo, mut state) = (None, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--repo") => match rest.next() {
                Some(value) => repo = Some(PathBuf::from(value)),
                None => return tools_usage("--repo needs a path"),
            },
            Some("--state") => match rest.next() {
                Some(value) => state = Some(PathBuf::from(value)),
                None => return tools_usage("--state needs a path"),
            },
            Some("-h" | "--help") => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => return tools_usage(&format!("unknown argument {other:?}")),
        }
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return tools_usage(&format!("the current folder cannot be read: {e}")),
    };
    let (path, git) = task_binding(repo.as_deref().unwrap_or(&cwd));
    let state = state.unwrap_or_else(ringo_cli::state_root);
    let binding = ringo_cli::task::RepositoryBinding { path, git };
    let gateway = match Gateway::new(&state, &binding) {
        Ok(gateway) => gateway,
        Err(why) => {
            eprintln!("ringo tools: {why}");
            return ExitCode::from(2);
        }
    };
    let mut bytes = Vec::new();
    // Bound the read itself: an oversized request is refused by the gateway, never buffered without limit.
    if let Err(e) = std::io::stdin().lock().take(MAX_REQUEST_BYTES as u64 + 1).read_to_end(&mut bytes) {
        eprintln!("ringo tools: the request could not be read: {e}");
        return ExitCode::from(2);
    }
    match serde_json::to_string(&gateway.dispatch_bytes(&bytes)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ringo tools: the response could not be written: {e}");
            ExitCode::from(2)
        }
    }
}

fn tools_usage(why: &str) -> ExitCode {
    eprintln!("ringo tools: {why}\n");
    eprint!("{USAGE}");
    ExitCode::from(2)
}

/// The repository a shell session in `cwd` would bind tasks to: the enclosing git project, unless that project holds
/// the owner's home folder (a dotfiles checkout), else the folder itself.
fn task_binding(cwd: &Path) -> (PathBuf, bool) {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from).unwrap_or_default();
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    let top = Command::new("git").arg("-C").arg(cwd).args(["rev-parse", "--show-toplevel"]).output().ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
        .filter(|top| home.as_os_str().is_empty() || !home.starts_with(top))
        .map(|top| std::fs::canonicalize(&top).unwrap_or(top));
    match top {
        Some(top) => (top, true),
        None => (std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()), false),
    }
}

/// `ringo review|design|work ...`: the native runtime when the release gate selects it, else ringo.py.
fn pipeline(args: Vec<OsString>) -> ExitCode {
    use ringo_cli::rt::Selection;
    match ringo_cli::rt::select(&ringo_cli::ringo_home()) {
        Ok(Selection::Native(identity)) => {
            let argv: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            let code = if argv[0] == "work" {
                // ringo.py work runs workflow.main(argv[1:])
                ringo_cli::rt::workflow::main(&argv[1..], &identity)
            } else {
                ringo_cli::rt::harness::main(&argv, identity)
            };
            ExitCode::from(code.clamp(0, 255) as u8)
        }
        Ok(Selection::Python) => harness(args),
        Err(e) => {
            // what ringo.py prints for a release state it cannot use
            eprintln!("ringo release: {}", e.msg);
            ExitCode::from(2)
        }
    }
}

/// Runs `python3 RINGO_HOME/ringo.py ARGS` in place of this process (Unix), or waits for it (Windows).
fn harness(args: Vec<OsString>) -> ExitCode {
    let script = ringo_cli::ringo_home().join("ringo.py");
    if !script.is_file() {
        eprintln!("ringo: {} not found; set RINGO_HOME to your Ringo checkout", script.display());
        return ExitCode::from(2);
    }
    let mut cmd = Command::new(ringo_cli::python());
    cmd.arg(&script).args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("ringo: {}: {err}", ringo_cli::python().to_string_lossy());
        ExitCode::from(2)
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(2) as u8),
        Err(err) => {
            eprintln!("ringo: {}: {err}", ringo_cli::python().to_string_lossy());
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_command_matching_finds_close_typos_and_rejects_unrelated_words() {
        assert_eq!(edit_distance("update", "updtae"), 2);
        assert_eq!(edit_distance("doctor", "doctr"), 1);
        let cmds = ["doctor", "status", "update", "setup", "resume", "version"];
        assert_eq!(closest_match("updtae", &cmds, 2), Some("update"));
        assert_eq!(closest_match("doctr", &cmds, 2), Some("doctor"));
        assert_eq!(closest_match("frobnicate", &cmds, 2), None);
    }
}

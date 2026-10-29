//! `ringo`: with no arguments on a terminal, the shell; with arguments, Ringo's Python harness (`ringo.py`)
//! unchanged, so release pinning and the measured pipelines stay exactly as they are.

mod setup;
mod tui;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::{Command, ExitCode};

const USAGE: &str = "\
ringo: Ringo's shell and harness.

  ringo                     open the shell (on a terminal); type /help there
  ringo setup               add a coding agent (codex, claude, grok) and sign in to it
  ringo review|design|work|wait|release ...
                            run a Ringo pipeline: the same as python3 ringo.py ...

The pipelines run from RINGO_HOME (default: the checkout ringo was built from).
";

fn main() -> ExitCode {
    if std::env::var_os("RINGO_CHILD").is_some() {
        eprintln!("ringo: refusing to start inside an agent that ringo launched (RINGO_CHILD is set)");
        return ExitCode::from(2);
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => tui::run(),
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
        Some("-V" | "--version") => {
            println!("ringo {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => harness(args),
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

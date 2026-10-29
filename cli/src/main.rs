//! `ringo`: with no arguments on a terminal, the shell; `review`, `design` and `work` run in this binary's native
//! runtime unless a release decides otherwise (then, and for the other commands, Ringo's Python harness `ringo.py`
//! runs).

mod setup;
mod theme;
mod tui;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::{Command, ExitCode};

const USAGE: &str = "\
ringo: Ringo's shell and harness.

  ringo                     open the shell (on a terminal); type /help there
  ringo setup               add a coding agent (codex, claude, grok) and sign in to it
  ringo review|design|work ...
                            run a Ringo pipeline natively: the same requests as python3 ringo.py ...
                            (with release state, ringo.py decides which runtime runs it)
  ringo wait|release ...    the same as python3 ringo.py wait|release ...

Prompts and plans come from RINGO_HOME (default: the checkout ringo was built from).
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
        Some("wait") => {
            let rest: Vec<String> = args[1..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
            ExitCode::from(ringo_cli::rt::wait::main(&rest) as u8)
        }
        Some("review" | "design" | "work") => pipeline(args),
        _ => harness(args),
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

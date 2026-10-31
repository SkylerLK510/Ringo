//! First-run setup: the owner types the command they start each agent with; Ringo checks it is installed and
//! signed in, runs the agent's own sign-in when it is not, and asks whether to add another. A name Ringo has no entry
//! for is never run to find out what it is: no program by that name means it is not an agent; a program Ringo does
//! not know can be added by the owner, with the command that runs one prompt.

use crate::theme::Theme;
use ringo_cli::{Agent, SignIn};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

fn ask(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok()?;
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn yes(prompt: &str) -> bool {
    ask(prompt).is_some_and(|a| a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes"))
}

/// Checks one agent, signing it in if needed; true when it is ready.
fn connect(agent: Agent, t: &Theme) -> bool {
    let name = agent.name();
    let ok = t.paint(t.ok, "✓");
    let program = &PathBuf::from(&agent.spec().program);
    if !ringo_cli::installed(program) {
        println!("  {name} ({}) is {}. Install it with:\n    {}\n  then type {name} again.",
                 agent.spec().company, t.paint(t.warn, "not installed"), agent.spec().install);
        return false;
    }
    for attempt in 0..2 {
        match ringo_cli::sign_in(agent, program) {
            SignIn::Yes(how) => {
                println!("  {name}: installed {ok}  signed in {ok} ({how})");
                return true;
            }
            SignIn::Unknown => {
                println!("  {name}: installed {ok}  Ringo cannot read its sign-in status.");
                if yes(&format!("  Open {name} now to sign in (sign in in the browser, then exit {name} to come back)? [y/n] ")) {
                    let _ = Command::new(program).status();
                }
                return true;
            }
            SignIn::No(why) if attempt == 0 => {
                println!("  {name}: installed {ok}  {} ({why}).", t.paint(t.warn, "not signed in"));
                println!("  Starting `{name} {}`: finish signing in, then you come back here.", agent.spec().login.join(" "));
                let _ = Command::new(program).args(&agent.spec().login).status();
            }
            SignIn::No(why) => {
                println!("  {name} is {} ({why}); type {name} to try again.", t.paint(t.fail, "still not signed in"));
            }
        }
    }
    false
}

/// A name Ringo has no entry for. Not a program on PATH: not an agent. A program: the owner may add it as a write-only
/// agent, giving the command that runs one prompt without questions. Never runs the program to find out.
fn unknown(typed: &str, answer: &str) -> Option<Agent> {
    if !ringo_cli::valid_name(typed) || !ringo_cli::installed(&PathBuf::from(typed)) {
        println!("  `{answer}` is not an agent: nothing called {answer} is installed.");
        return None;
    }
    println!("  `{typed}` is a program, but not an agent Ringo knows.");
    if !yes("  If it is an AI agent, add it now? [y/n] ") {
        return None;
    }
    println!("  The command that runs one prompt and prints the answer, with {{prompt}} where the prompt goes and");
    println!("  {{cwd}} for the folder, e.g. `{typed} -p {{prompt}}` (without {{prompt}}, the prompt goes on stdin).");
    let line = ask("  command> ")?;
    let mut words = line.split_whitespace();
    if words.next() != Some(typed) {
        println!("  The command has to start with `{typed}`; type {typed} again to retry.");
        return None;
    }
    let args: Vec<&str> = words.collect();
    let entry = serde_json::json!({"name": typed, "adapter": "plain", "args": args});
    match ringo_cli::add_custom_agent(&ringo_cli::state_root(), entry) {
        Ok(agent) => {
            println!("  Added {typed}: @{typed}! runs it in its own worktree (no read-only turns: Ringo cannot \
                      verify one).");
            println!("  Change it in {}.", ringo_cli::state_root().join("agents.json").display());
            Some(agent)
        }
        Err(e) => {
            println!("  Could not add {typed}: {e}");
            None
        }
    }
}

/// Adds agents to the owner's setup. Returns the agents now set up.
pub fn run() -> Vec<Agent> {
    let t = Theme::load();
    let mut agents = ringo_cli::configured_agents().unwrap_or_default();
    if agents.is_empty() {
        println!("{}", t.paint(t.title.patch(t.running), "Welcome to Ringo."));
        println!("Ringo routes your work to the coding agents you already use, and runs its own cheap pipelines with them.");
    } else {
        let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
        println!("Agents set up: {}.", names.join(", "));
    }
    let known: Vec<&str> = Agent::all().into_iter().map(|a| a.name()).collect();
    println!("Type the command you start an agent with, as you would in a terminal ({}).", known.join(", "));
    while let Some(answer) = ask("agent> ") {
        if answer.is_empty() {
            if agents.is_empty() { continue } else { break }
        }
        let typed = answer.to_lowercase();
        if let Some(agent) = Agent::from_name(&typed).or_else(|| unknown(&typed, &answer))
            && connect(agent, &t) && !agents.contains(&agent) {
            agents.push(agent);
            if let Err(e) = ringo_cli::save_agents(&agents) {
                println!("  could not save the setup: {e}");
            }
        }
        if !agents.is_empty() && !yes("Would you like to add another agent [y/n] ") {
            break;
        }
    }
    let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
    if !names.is_empty() {
        println!("Set up: {}. Next time just type ringo.", names.join(", "));
    }
    agents
}

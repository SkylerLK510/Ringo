//! First-run setup: the owner types the command they start each agent with; Ringo checks it is installed and
//! signed in, runs the agent's own sign-in when it is not, and asks whether to add another.

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
    let program = &PathBuf::from(name);
    if !ringo_cli::installed(program) {
        println!("  {name} ({}) is {}. Install it with:\n    {}\n  then type {name} again.",
                 ringo_cli::company(agent), t.paint(t.warn, "not installed"), ringo_cli::install_hint(agent));
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
                println!("  Starting `{name} {}`: finish signing in, then you come back here.", ringo_cli::login_args(agent).join(" "));
                let _ = Command::new(program).args(ringo_cli::login_args(agent)).status();
            }
            SignIn::No(why) => {
                println!("  {name} is {} ({why}); type {name} to try again.", t.paint(t.fail, "still not signed in"));
            }
        }
    }
    false
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
    println!("Type the command you start an agent with, as you would in a terminal (codex, claude or grok).");
    while let Some(answer) = ask("agent> ") {
        if answer.is_empty() {
            if agents.is_empty() { continue } else { break }
        }
        let typed = answer.to_lowercase();
        match Agent::from_name(&typed) {
            None => println!("  Ringo does not drive `{answer}` yet; it knows codex, claude and grok."),
            Some(agent) => {
                if connect(agent, &t) && !agents.contains(&agent) {
                    agents.push(agent);
                    if let Err(e) = ringo_cli::save_agents(&agents) {
                        println!("  could not save the setup: {e}");
                    }
                }
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

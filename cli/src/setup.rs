//! Setup. The first time, Ringo looks for the agents it knows on PATH, lists what it found and how to install the
//! rest, and on Enter signs in to the ones found (each agent's own sign-in, when it is not signed in yet). Later
//! (`/setup`), or when the owner types names instead, they type the command they start each agent with; Ringo checks
//! it is installed and signed in, and asks whether to add another. A name Ringo has no entry for is never run to find
//! out what it is: no program by that name means it is not an agent; a program Ringo does not know can be added by
//! the owner, with the command that runs one prompt.

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
                    let _ = Command::new(program).args(&agent.spec().login).status(); // its own sign-in, if it has one
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

/// A command line split into words the way a shell splits a simple one: quotes ('…' or "…") group words, a backslash
/// outside single quotes keeps the next character as it is. No variables, globs or pipes: Ringo runs no shell.
fn words(line: &str) -> Result<Vec<String>, String> {
    let (mut out, mut word, mut quote, mut started) = (Vec::new(), String::new(), None, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.push(c),
            (_, '\\') => word.push(chars.next().ok_or("the command ends with a backslash")?),
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if started || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                started = false;
                continue;
            }
            (None, c) => word.push(c),
        }
        started = true;
    }
    if quote.is_some() {
        return Err("a quote in the command is not closed".into());
    }
    if started || !word.is_empty() {
        out.push(word);
    }
    Ok(out)
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
    let words = match words(&line) {
        Ok(words) => words,
        Err(e) => {
            println!("  {e}; type {typed} again to retry.");
            return None;
        }
    };
    if words.first().map(String::as_str) != Some(typed) {
        println!("  The command has to start with `{typed}`; type {typed} again to retry.");
        return None;
    }
    let args = &words[1..];
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

/// The first start: the agents Ringo knows that are installed here, signed in on Enter. None when none is installed;
/// empty when the owner would rather type names.
fn found(t: &Theme) -> Option<Vec<Agent>> {
    println!("{}", t.paint(t.title.patch(t.running), "Welcome to Ringo."));
    println!("Ringo works with the AI coding agents already on this computer, on your own subscription logins.\n");
    let known: Vec<Agent> = Agent::all().into_iter().filter(|a| !a.spec().custom).collect();
    let here: Vec<Agent> = known.iter().copied().filter(|a| ringo_cli::installed(&PathBuf::from(&a.spec().program))).collect();
    for agent in &known {
        let spec = agent.spec();
        if here.contains(agent) {
            println!("  {} {:<7} {}", t.paint(t.ok, "✓"), agent.name(), spec.company);
        } else {
            println!("  · {:<7} {} is not installed; to add it: {}", agent.name(), spec.company, spec.install);
        }
    }
    if here.is_empty() {
        println!("\nNone is installed yet. Install one with the command after its name (Claude Code is the one Ringo \
                  uses for code), then type ringo again.");
        return None;
    }
    let names: Vec<&str> = here.iter().map(|a| a.name()).collect();
    let answer = ask(&format!("\nPress Enter to use {} (Ringo signs you in where needed), or type n to choose: ",
                              names.join(" and "))).unwrap_or_default();
    if !answer.is_empty() {
        return Some(Vec::new());
    }
    let mut agents = Vec::new();
    for agent in here {
        if connect(agent, t) {
            agents.push(agent);
        }
    }
    if let Err(e) = ringo_cli::save_agents(&agents) {
        println!("  could not save the setup: {e}");
    }
    Some(agents)
}

/// Adds agents to the owner's setup. Returns the agents now set up.
pub fn run() -> Vec<Agent> {
    let t = Theme::load();
    let mut agents = ringo_cli::configured_agents().unwrap_or_default();
    if agents.is_empty() {
        agents = match found(&t) {
            Some(agents) => agents,
            None => return Vec::new(),
        };
        if !agents.is_empty() {
            let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
            println!("Ready: {}. Next time just type ringo; /setup adds another agent.", names.join(", "));
            return agents;
        }
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

#[cfg(test)]
mod tests {
    #[test]
    fn splits_a_command_like_a_shell() {
        let w = |l: &str| super::words(l).unwrap();
        assert_eq!(w("mytool --prompt \"{prompt}\""), ["mytool", "--prompt", "{prompt}"]);
        assert_eq!(w("mytool --model 'My Model' -p {prompt}"), ["mytool", "--model", "My Model", "-p", "{prompt}"]);
        assert_eq!(w(r#"a "it's" b\ c ''"#), ["a", "it's", "b c", ""]);
        assert!(super::words("a 'open").is_err());
    }
}

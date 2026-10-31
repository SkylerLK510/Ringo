//! Live check (one small turn each): read turns cannot create files.
//! `cargo run --example readonly -- REPO [AGENT...]` (default: claude codex).
use ringo_cli::{Agent, Session};
use std::time::Duration;

fn main() {
    let repo = std::path::PathBuf::from(std::env::args().nth(1).expect("a scratch git repository"));
    let mut s = Session::new(&repo).unwrap();
    let mut names: Vec<String> = std::env::args().skip(2).collect();
    if names.is_empty() {
        names = vec!["claude".into(), "codex".into()];
    }
    for agent in names.iter().map(|n| Agent::from_name(n).expect("an agent Ringo knows")) {
        let file = format!("{}-was-here.txt", agent.name());
        let id = s.launch_agent(agent, false, &format!("Create a file named {file} containing hi, then say what happened.")).unwrap();
        while s.job(id).unwrap().running() {
            s.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let j = s.job(id).unwrap();
        println!("{}: {:?} tokens={} file created: {} reply: {}", agent.name(), j.status, j.tokens.total(),
                 repo.join(&file).exists(), ringo_cli::parse::short(&j.reply(), 140));
    }
}

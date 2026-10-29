//! Live check (two small turns): read turns cannot create files. `cargo run --example readonly -- REPO`.
use ringo_cli::{Agent, Session};
use std::time::Duration;

fn main() {
    let repo = std::path::PathBuf::from(std::env::args().nth(1).expect("a scratch git repository"));
    let mut s = Session::new(&repo).unwrap();
    for agent in [Agent::Claude, Agent::Codex] {
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

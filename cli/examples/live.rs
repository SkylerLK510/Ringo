//! Live check against the real agent CLIs (spends a few small turns each):
//! `cargo run --example live -- REPO [AGENT...]` (default: codex claude).
use ringo_cli::{Agent, Session};
use std::time::Duration;

fn main() {
    let repo = std::env::args().nth(1).expect("a scratch git repository");
    let mut s = Session::new(std::path::Path::new(&repo)).unwrap();
    let run = |s: &mut Session, agent, write, prompt: &str| {
        let id = s.launch_agent(agent, write, prompt).unwrap();
        while s.job(id).unwrap().running() {
            s.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let j = s.job(id).unwrap();
        println!("#{id} {}{} {:?} {:.1}s tokens={} subs={} reply={:?}", agent.name(), if write { "!" } else { "" },
                 j.status, j.age().as_secs_f32(), j.tokens.total(), j.subs.len(), ringo_cli::parse::short(&j.reply(), 60));
        id
    };
    let mut names: Vec<String> = std::env::args().skip(2).collect();
    if names.is_empty() {
        names = vec!["codex".into(), "claude".into()];
    }
    for name in names {
        let agent = Agent::from_name(&name).expect("an agent Ringo knows");
        run(&mut s, agent, false, "Reply with the single word OK.");
        run(&mut s, agent, false, "What single word did you just reply with? Answer with that word only.");
        let w = run(&mut s, agent, true,
                    &format!("Create a file named {name}.txt containing the single line hi. Do nothing else."));
        println!("diff #{w}:\n{}", s.diff(w).unwrap_or_else(|e| e));
    }
    println!("state: {}", s.dir.display());
}

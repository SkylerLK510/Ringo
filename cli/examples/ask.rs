//! Live check of one agent asking another in the middle of its turn (spends two small turns):
//! `cargo run --example ask -- REPO RINGO FILE [ASKER]` makes a specialist `reader` on Claude from the instructions
//! in FILE, for this process only, then has ASKER (default claude) ask it one thing through the ask tool, served by
//! the built RINGO binary, and prints both turns.
use ringo_cli::{Agent, Session, Spec};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [repo, ringo, file, rest @ ..] = &args[..] else { return eprintln!("usage: ask REPO RINGO FILE [ASKER]") };
    let asker = Agent::from_name(rest.first().filter(|a| *a != "--").map_or("claude", String::as_str)).expect("an agent Ringo knows");
    let entry = serde_json::json!({"name": "reader", "adapter": "claude", "program": "claude", "read": true, "instructions": file});
    let reader = Agent::register(Spec::from_json(&entry, true).unwrap()).unwrap();
    let mut s = Session::new(std::path::Path::new(repo)).unwrap();
    s.mode = ringo_cli::Mode::Read;
    s.peers = vec![asker, reader];
    s.asker = ringo.into();
    // `-- PROMPT`: another message than the ask (a trivial one shows what the tool costs a turn that does not use it)
    let prompt = match rest.iter().position(|a| a == "--") {
        Some(at) => rest[at + 1..].join(" "),
        None => "Use the ask_agent tool to ask reader what this project's package name is. Then tell me, in one line, \
                 what reader said.".to_string(),
    };
    let id = s.launch_agent(asker, false, &prompt).unwrap();
    while s.jobs.iter().any(|j| j.running()) {
        s.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    for job in &s.jobs {
        let who = match job.kind { ringo_cli::Kind::Agent { agent, .. } => agent.name(), _ => "ringo" };
        println!("#{} {who}{}: {:?} {:.1}s tokens={} asked={} reply: {}", job.id, if job.id == id { "" } else { " (asked)" }, job.status,
                 job.age().as_secs_f32(), job.tokens.total(), job.asked, ringo_cli::parse::short(&job.reply(), 300));
        for line in job.log.iter().filter(|l| l.contains("ask")) {
            println!("    log: {}", ringo_cli::parse::short(line, 200));
        }
    }
}

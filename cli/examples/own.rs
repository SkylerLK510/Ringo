//! Live check of an agent of the owner's own against the real agent CLI (spends one small turn):
//! `cargo run --example own -- REPO BASE FILE [skills] [-- PROMPT]` makes BASE (claude or codex) a specialist from
//! the instructions in FILE, for this process only, runs one turn in a scratch git repository (read mode; ask mode
//! with `skills`, which read mode leaves out) and prints how it ended, what it used and the start of its reply.
use ringo_cli::{Agent, Session, Spec};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (args, prompt) = match args.iter().position(|a| a == "--") {
        Some(at) => (&args[..at], args[at + 1..].join(" ")),
        None => (&args[..], "In one sentence: who are you, and what do you know best?".to_string()),
    };
    let [repo, base, file, rest @ ..] = args else { return eprintln!("usage: own REPO BASE FILE [skills] [-- PROMPT]") };
    let base = Agent::from_name(base).expect("an agent Ringo knows");
    let adapter = if base.spec().adapter == ringo_cli::Adapter::Codex { "codex" } else { "claude" };
    let skills = rest.iter().any(|w| w == "skills");
    let entry = serde_json::json!({"name": "own-check", "adapter": adapter, "program": base.spec().program, "read": true,
                                   "instructions": file, "skills": skills});
    let own = Agent::register(Spec::from_json(&entry, true).unwrap()).unwrap();
    let size = ringo_cli::instructions(own.spec()).unwrap().map_or(0, |text| text.len());
    let mut s = Session::new(std::path::Path::new(repo)).unwrap();
    s.mode = if skills { ringo_cli::Mode::Ask } else { ringo_cli::Mode::Read };
    let id = s.launch_agent(own, false, &prompt).unwrap();
    while s.job(id).unwrap().running() {
        s.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    let job = s.job(id).unwrap();
    println!("{} with {size} bytes of instructions{}: {:?} {:.1}s tokens={} ran {:?}\nreply: {}", base.name(),
             if skills { " and skills" } else { "" }, job.status, job.age().as_secs_f32(), job.tokens.total(), job.model,
             ringo_cli::parse::short(&job.reply(), 400));
}

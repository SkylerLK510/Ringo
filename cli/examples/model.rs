//! Live check of a model choice against the real agent CLI (spends one small turn):
//! `cargo run --example model -- REPO AGENT MODEL` runs "Reply with the single word OK." on that model in a scratch
//! git repository and prints how the turn ended and the model the agent says it ran.
use ringo_cli::{Agent, Session};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [repo, agent, model] = &args[..] else { return eprintln!("usage: model REPO AGENT MODEL") };
    let agent = Agent::from_name(agent).expect("an agent Ringo knows");
    let mut s = Session::new(std::path::Path::new(repo)).unwrap();
    s.mode = ringo_cli::Mode::Read;
    s.models.insert(agent, model.clone());
    let id = s.launch_agent(agent, false, "Reply with the single word OK.").unwrap();
    while s.job(id).unwrap().running() {
        s.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    let job = s.job(id).unwrap();
    println!("{} asked for {model}: {:?} {:.1}s tokens={} ran {:?} reply={:?}", agent.name(), job.status, job.age().as_secs_f32(),
             job.tokens.total(), job.model, ringo_cli::parse::short(&job.reply(), 40));
}

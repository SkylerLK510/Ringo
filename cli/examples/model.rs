//! Live check of a model choice against the real agent CLI (spends one small turn):
//! `cargo run --example model -- REPO AGENT MODEL` runs "Reply with the single word OK." on that model in a scratch
//! git repository and prints how the turn ended and the model the agent says it ran. With `then-default` after
//! MODEL, the choice is then taken back and the same conversation takes one more turn, to show what it runs on.
use ringo_cli::{Agent, Session};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [repo, agent, model, rest @ ..] = &args[..] else { return eprintln!("usage: model REPO AGENT MODEL [then-default]") };
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
    // `then=NAME`: the same conversation goes on with another model
    if let Some(next) = rest.iter().find_map(|w| w.strip_prefix("then=")) {
        s.models.insert(agent, next.to_string());
        let id = s.launch_agent(agent, false, "Reply with the single word OK.").unwrap();
        while s.job(id).unwrap().running() {
            s.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let job = s.job(id).unwrap();
        println!("{} changed to {next}, same conversation: {:?} {:.1}s tokens={} ran {:?}", agent.name(), job.status,
                 job.age().as_secs_f32(), job.tokens.total(), job.model);
    }
    if rest.iter().any(|w| w == "then-default") {
        s.models.remove(&agent);
        let id = s.launch_agent(agent, false, "Reply with the single word OK.").unwrap();
        while s.job(id).unwrap().running() {
            s.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        let job = s.job(id).unwrap();
        println!("{} with the choice taken back, same conversation: {:?} {:.1}s tokens={} ran {:?}", agent.name(), job.status,
                 job.age().as_secs_f32(), job.tokens.total(), job.model);
    }
}

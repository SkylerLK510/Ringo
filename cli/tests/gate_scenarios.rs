//! Scripted transitions and policy sensitivity through the actual Session, using a fake app-server.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command,
          sync::atomic::{AtomicUsize, Ordering}};

const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
hello = json.loads(sys.stdin.readline())
sys.stdin.readline()
thread = json.loads(sys.stdin.readline())
print(json.dumps({'id': 1, 'result': {}}), flush=True)
path = Path(os.environ['SCENARIO_REQUESTS'])
count = len(path.read_text().splitlines()) if path.exists() else 0
tid = thread['params'].get('threadId', 'fake-'+str(count+1))
print(json.dumps({'id': 2, 'result': {'thread': {'id': tid}}}), flush=True)
turn = json.loads(sys.stdin.readline())
with path.open('a') as f: f.write(json.dumps({'thread':thread, 'turn':turn})+'\n')
def emit(method, params): print(json.dumps({'method':method, 'params':params}), flush=True)
emit('item/completed', {'threadId':tid, 'item':{'type':'agentMessage', 'text':'constraint recorded'}})
emit('thread/tokenUsage/updated', {'threadId':tid, 'tokenUsage':{'total':{'inputTokens':100,'cachedInputTokens':0,'outputTokens':2}, 'last':{'inputTokens':100,'outputTokens':2}}})
emit('turn/completed', {'threadId':tid, 'turn':{'status':'completed'}})
"#;

struct Case { root: PathBuf, program: PathBuf, agent: &'static str }
impl Case {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!("ringo-scenario-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        fs::create_dir_all(root.join("project")).unwrap();
        let program = root.join("fake-app-server");
        fs::write(&program, FAKE).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, program, agent: "codex" }
    }
    fn run(&self, actions: Value, policy: Value, child: bool) -> (i32, Value, Vec<Value>) {
        fs::write(self.root.join("scenario.json"), json!({"agent":self.agent, "actions":actions}).to_string()).unwrap();
        fs::write(self.root.join("policy.json"), policy.to_string()).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_scenariorun"));
        cmd.arg("--project").arg(self.root.join("project"))
            .arg("--scenario").arg(self.root.join("scenario.json"))
            .arg("--policy").arg(self.root.join("policy.json"))
            .arg("--program").arg(&self.program).arg("--state").arg(self.root.join("state"))
            .arg("--home").arg(self.root.join("home"))
            .env("SCENARIO_REQUESTS", self.root.join("requests"))
            .env("RINGO_NO_UPDATE_CHECK", "1").env_remove("RINGO_HOME").env_remove("CODEX_HOME");
        if child { cmd.env("RINGO_CHILD", "1"); } else { cmd.env_remove("RINGO_CHILD"); }
        let output = cmd.output().unwrap();
        let record: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e|
            panic!("{e}: {} / {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)));
        let requests = fs::read_to_string(self.root.join("requests")).unwrap_or_default()
            .lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (output.status.code().unwrap_or(-1), record, requests)
    }
}
impl Drop for Case { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }
fn message(request: &Value) -> &str { request["turn"]["params"]["input"][0]["text"].as_str().unwrap() }
fn seed(fills: u64) -> Value {
    json!({"op":"seed", "threads":[["codex","old-thread"]],
           "records":[["codex",fills,"Earlier constraint: keep the public API stable."]]})
}

#[test]
fn fresh_resume_clear_and_restart_use_real_session_state() {
    let case = Case::new();
    let (code, record, requests) = case.run(json!([
        {"op":"turn","message":"keep the public API stable"},
        {"op":"turn","message":"continue"}, {"op":"restart"},
        {"op":"turn","message":"after restart"}, {"op":"clear"},
        {"op":"turn","message":"new task"}]), json!({}), false);
    assert_eq!(code, 0, "{record}");
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0]["thread"]["method"], "thread/start");
    assert_eq!(requests[1]["thread"]["method"], "thread/resume");
    assert_eq!(requests[2]["thread"]["method"], "thread/resume");
    assert_eq!(requests[3]["thread"]["method"], "thread/start");
    assert_eq!(message(&requests[3]), "new task");
    assert_eq!(record["events"][4]["after"], json!([]));
    assert!(record["events"][2]["kept_sha256"].as_str().is_some_and(|s| s.len() == 64));
    assert_eq!(record["events"][0]["usage"]["input"], 100);
}

#[test]
fn handoff_preserves_prior_constraint_and_exact_new_message() {
    let case = Case::new();
    let (code, record, requests) = case.run(json!([seed(64001), {"op":"restart"},
        {"op":"turn","message":"Do this next.\nKeep this text exact."}]), json!({}), false);
    assert_eq!(code, 0, "{record}");
    assert_eq!(requests[0]["thread"]["method"], "thread/start");
    let sent = message(&requests[0]);
    assert!(sent.contains("keep the public API stable"), "{sent}");
    assert!(sent.ends_with("The owner's new message:\nDo this next.\nKeep this text exact."));
}

#[test]
fn threshold_boundary_and_deliberate_policy_regression_are_detectable() {
    for (fills, threshold, expected) in [(64000,64000,"thread/resume"), (64001,64000,"thread/start"),
                                        (32000,16000,"thread/start"), (32000,128000,"thread/resume")] {
        let case = Case::new();
        let (code, record, requests) = case.run(json!([seed(fills), {"op":"turn","message":"next"}]),
                                               json!({"handoff_at":threshold}), false);
        assert_eq!(code, 0, "{record}");
        assert_eq!(requests[0]["thread"]["method"], expected);
        assert_eq!(message(&requests[0]).contains("keep the public API stable"), expected == "thread/start");
    }
}

#[test]
fn explicit_policy_changes_only_instruction_text_and_clear_drops_seed() {
    let case = Case::new();
    let (code, record, requests) = case.run(json!([seed(100000), {"op":"clear"},
        {"op":"turn","message":"next"}]), json!({"instruction":"Check the result.",
        "handoff_instruction":"Keep the owner's constraints."}), false);
    assert_eq!(code, 0, "{record}");
    assert_eq!(message(&requests[0]), "Check the result.\n\nnext");
    assert_eq!(requests[0]["thread"]["params"]["sandbox"], "read-only");
}

#[test]
fn child_guard_and_invalid_scenario_refuse_before_any_provider_launch() {
    for (actions, policy, child) in [(json!([{"op":"turn","message":"next"}]), json!({}), true),
        (json!([{"op":"shell","command":"true"}]), json!({}), false),
        (json!([{"op":"turn","message":"next"}]), json!({"permissions":"all"}), false)] {
        let case = Case::new();
        let (code, _, requests) = case.run(actions, policy, child);
        assert_eq!(code, 2);
        assert!(requests.is_empty());
    }
}

#[test]
fn claude_compact_is_an_exact_control_command_even_with_an_instruction_policy() {
    let mut case = Case::new();
    case.agent = "claude";
    fs::write(&case.program, r#"#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
if '--input-format' not in sys.argv:
    print('2.1.323 (Claude Code)'); sys.exit(0)
request = json.loads(sys.stdin.readline())
Path(os.environ['SCENARIO_REQUESTS']).write_text(json.dumps(request)+'\n')
for event in [
 {'type':'system','subtype':'init','session_id':'fake-claude','apiKeySource':'none'},
 {'type':'assistant','message':{'id':'m1','content':[{'type':'text','text':'compacted'}], 'usage':{'input_tokens':10,'output_tokens':2}},'parent_tool_use_id':None},
 {'type':'result','subtype':'success','is_error':False,'result':'compacted'}]:
 print(json.dumps(event), flush=True)
"#).unwrap();
    let (code, record, requests) = case.run(json!([{"op":"compact"}]), json!({"instruction":"Check your work."}), false);
    assert_eq!(code, 0, "{record}");
    assert_eq!(requests[0]["message"]["content"], "/compact");
}

#[test]
fn termination_shuts_down_the_separate_provider_process_group() {
    let case = Case::new();
    let marker = case.root.join("ticks");
    let script = format!("#!/bin/sh\nwhile true; do echo tick >> '{}'; sleep 0.05; done\n", marker.display());
    fs::write(&case.program, script).unwrap();
    fs::write(case.root.join("scenario.json"), json!({"agent":"codex", "actions":[{"op":"turn","message":"work"}]}).to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scenariorun"))
        .arg("--project").arg(case.root.join("project"))
        .arg("--scenario").arg(case.root.join("scenario.json"))
        .arg("--program").arg(&case.program).arg("--state").arg(case.root.join("state"))
        .arg("--home").arg(case.root.join("home"))
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").env_remove("CODEX_HOME")
        .stdout(std::process::Stdio::null()).spawn().unwrap();
    let deadline = std::time::Instant::now()+std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(marker.exists(), "stand-in started");
    // Signal just this scenario controller; its handler must stop the separate provider group.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM); }
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() { let _ = child.kill(); panic!("scenario did not exit"); }
    let before = fs::read(&marker).unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(fs::read(&marker).unwrap().len(), before, "provider descendant stopped");
}

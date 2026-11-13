//! The `turnrun` measurement runner against fake agents in throwaway git repositories (Unix: the
//! fakes are shell scripts). What is pinned here: the record's accounting shape, the cap behavior
//! (a live backend is stopped mid-turn; a post-turn one is recorded as overshoot), the process
//! stop, the fixture's cleanliness requirement, the recursion guard, and a restarted conversation
//! seeded through the actual handoff path.

#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A codex that answers at once: it records its arguments (`.argv`), environment (`.env`) and turn
/// request (`.turn`), writes `made.txt`, and reports a turn's usage (raw input 10, of which 4 are
/// cached: the record must hold input 6 and cache_read 4, never 10 + 4).
const CODEX_OK: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
env > "$0.env"
read -r hello; read -r ready; read -r thread
echo '{"id":1,"result":{}}'
echo '{"id":2,"result":{"thread":{"id":"t-1"}}}'
read -r turn
printf '%s\n' "$turn" > "$0.turn"
echo "made it" > made.txt
echo '{"method":"item/completed","params":{"threadId":"t-1","item":{"type":"agentMessage","text":"all done"}}}'
echo '{"method":"thread/tokenUsage/updated","params":{"threadId":"t-1","tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":2}}}}'
echo '{"method":"turn/completed","params":{"threadId":"t-1","turn":{"status":"completed"}}}'
"#;

/// A claude whose turn works for a while, marking each pass in `$0.work` and reporting usage on
/// every message (`live`): with `units` a pass costs that many tokens, and `passes` bounds the turn.
const CLAUDE_WORKS: &str = r#"#!/bin/sh
case "$*" in *no-session-persistence*) exit 0 ;; *--input-format*) ;; *) echo '2.1.318 (Claude Code)'; exit 0 ;; esac
head -n 1 > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s-r","apiKeySource":"none"}'
i=0
while [ "$i" -lt 400 ]; do
  printf '{"type":"assistant","message":{"id":"m%s","content":[{"type":"text","text":"working"}],"usage":{"input_tokens":%s,"output_tokens":10}},"parent_tool_use_id":null}\n' "$i" "$TURNRUN_UNITS"
  echo tick >> "$0.work"
  sleep 0.05
  i=$((i+1))
done
echo '{"type":"result","subtype":"success","is_error":false,"result":"built"}'
"#;

fn root() -> PathBuf {
    std::env::temp_dir().join(format!("ringo-turnrun-{}", std::process::id()))
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git").arg("-C").arg(dir).args(args).status().map(|s| s.success()).unwrap_or(false)
}

/// A one-commit repository: the fixture a run refuses to leave dirty.
fn repo(name: &str) -> PathBuf {
    let dir = root().join(name);
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
    dir
}

fn fake(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    let script = if script.starts_with("#!") { script.to_string() } else { format!("#!/bin/sh\n{script}") };
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// One run of the runner: its exit code, stdout record, and the record file when asked for.
fn turnrun(project: &Path, program: &Path, args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(project).arg("--agent").arg("codex").arg("--program").arg(program)
        .arg("--message").arg("do the thing")
        .args(args)
        .env("HOME", root().join("home"))
        .env("RINGO_NO_UPDATE_CHECK", "1")
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").env_remove("CODEX_HOME")
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let record: Value = stdout.lines().rev().find_map(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| panic!("no record on stdout: {stdout}"));
    (out.status.code().unwrap_or(-1), record)
}

fn codex_turn(codex: &Path) -> String {
    let turn: Value = serde_json::from_str(&fs::read_to_string(codex.with_extension("turn")).unwrap_or_default())
        .unwrap_or(Value::Null);
    turn["params"]["input"][0]["text"].as_str().unwrap_or_default().to_string()
}

/// A turn that stopped must end its agent's process: the marker file stops growing.
fn marker_stops(work: &Path, within: Duration) -> bool {
    let read = || fs::read_to_string(work).unwrap_or_default().lines().count();
    let (before, deadline) = (read(), Instant::now() + within);
    let mut stable = 0;
    while Instant::now() < deadline {
        if read() > before {
            return false; // still growing
        }
        stable += 1;
        if stable > 20 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
fn records_a_finished_turns_accounting_and_change() {
    let r = repo("ok");
    let codex = fake(&root(), "codex-ok", CODEX_OK);
    fs::create_dir_all(root().join("home")).unwrap();
    let record_path = root().join("records/ok.json");
    let (code, rec) = turnrun(&r, &codex, &["--record", record_path.to_str().unwrap(), "--source-id", "src-ok"]);
    assert_eq!(code, 0);
    assert_eq!(rec["outcome"], "done");
    assert_eq!(rec["usage"]["input"], 6, "raw input 10 minus its 4 cached tokens");
    assert_eq!(rec["usage"]["cache_read"], 4);
    assert_eq!(rec["usage"]["output"], 2);
    assert_eq!(rec["usage"]["cache_write"], 0);
    assert_eq!(rec["usage_total"], 12);
    assert_eq!(rec["usage"]["counted"], true);
    assert_eq!(rec["agent"]["cap_detection"], "post_turn", "codex reports only at a turn's end");
    assert_eq!(rec["changed"], serde_json::json!(["made.txt"]));
    assert_eq!(rec["fixture"]["clean"], true);
    assert_eq!(rec["source_id"], "src-ok");
    assert_eq!(rec["budget"]["tokens"], Value::Null);
    assert!(rec["elapsed_s"].as_f64().unwrap() < 60.0);
    assert!(rec["runner"]["binary_sha256"].as_str().is_some_and(|h| h.len() == 64));
    let job_dir = PathBuf::from(rec["job_dir"].as_str().unwrap());
    assert!(job_dir.join("reply.md").exists());
    // The record file is the same evidence, written whole.
    let written: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(written["outcome"], "done");
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "made it\n");
    assert!(git(&r, &["status", "--porcelain"]));
    assert_eq!(codex_turn(&codex), "do the thing");
    // No API key reaches the agent, and it cannot start Ringo again.
    let env = fs::read_to_string(codex.with_extension("env")).unwrap();
    assert!(!env.contains("API_KEY"));
    assert!(env.contains("RINGO_CHILD=1"));
    let actual_home = env.lines().find_map(|l| l.strip_prefix("HOME=")).unwrap();
    assert_ne!(Path::new(actual_home), root().join("home"), "the inherited home is replaced");
    assert!(Path::new(actual_home).is_dir());
    assert!(env.lines().any(|l| l == format!("CLAUDE_CONFIG_DIR={actual_home}/.claude")));
}

#[test]
fn a_finished_turn_without_counts_keeps_its_total_unknown() {
    let r = repo("unknown-usage");
    let script = CODEX_OK.lines().filter(|line| !line.contains("thread/tokenUsage/updated"))
        .collect::<Vec<_>>().join("\n") + "\n";
    let codex = fake(&root(), "codex-unknown", &script);
    let (code, rec) = turnrun(&r, &codex, &[]);
    assert_eq!(code, 0);
    assert_eq!(rec["usage"]["counted"], false);
    assert_eq!(rec["usage_total"], Value::Null);
}

#[test]
fn refuses_a_dirty_fixture_the_recursion_and_bad_usage() {
    let r = repo("dirty");
    let codex = fake(&root(), "codex-dirty", CODEX_OK);
    fs::write(r.join("a.txt"), "changed\n").unwrap();
    let (code, rec) = turnrun(&r, &codex, &[]);
    assert_eq!((code, rec["outcome"].as_str().unwrap()), (2, "refused"));
    assert_eq!(rec["reason"], "the project's tree is not clean");
    assert!(!codex.with_extension("turn").exists(), "nothing ran");

    fs::write(r.join("a.txt"), "one\n").unwrap();
    git(&r, &["add", "."]);
    git(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "back"]);
    // A runner started by an agent Ringo launched refuses to start agents of its own.
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("codex").arg("--program").arg(&codex)
        .arg("--message").arg("x").env("RINGO_CHILD", "1").env_remove("RINGO_HOME").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let rec: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rec["reason"], "RINGO_CHILD is set");

    let (code, rec) = turnrun(&r, &codex, &["--budget-tokens", "words"]);
    assert_eq!((code, rec["reason"].as_str().unwrap()), (2, "--budget-tokens needs a number"));

    // Without --program the run would start the installed agent: a live session, refused.
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("codex").arg("--message").arg("x")
        .env("HOME", root().join("home")).env_remove("RINGO_CHILD").env_remove("RINGO_HOME").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let rec: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(rec["reason"].as_str().unwrap().starts_with("--program is required"), "{rec}");

    // A program that is an agent's own command (by name, or as PATH finds it) is a live session.
    let named = root().join("live");
    fs::create_dir_all(&named).unwrap();
    let live = fake(&named, "claude", CODEX_OK);
    let (code, rec) = turnrun(&r, &live, &[]);
    assert_eq!((code, rec["outcome"].as_str().unwrap()), (2, "refused"));
    assert!(rec["reason"].as_str().unwrap().contains("installed claude command"), "{rec}");
    let alias = root().join("looks-offline");
    std::os::unix::fs::symlink(&live, &alias).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("codex").arg("--program").arg(&alias).arg("--message").arg("x")
        .env("PATH", std::env::join_paths(std::iter::once(named.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()))).unwrap())
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let rec: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(rec["reason"].as_str().unwrap().contains("installed claude command"), "{rec}");

    // A seed the shell cannot go on from would run an unseeded turn under a seeded label: refused.
    let bad = root().join("not-a-conversation.json");
    fs::write(&bad, "not json").unwrap();
    let (code, rec) = turnrun(&r, &codex, &["--resume-json", bad.to_str().unwrap()]);
    assert_eq!((code, rec["outcome"].as_str().unwrap()), (2, "refused"));
    assert!(!codex.with_extension("turn").exists(), "nothing ran");
}

/// A live backend is stopped at its cap mid-turn: the record says cap_breach, and the fake's marker
/// file stops growing, so the stop reached the process group.
#[test]
fn stops_a_live_turn_at_its_token_cap() {
    let r = repo("cap");
    let claude = fake(&root(), "claude-cap", &format!("export TURNRUN_UNITS=1000\n{CLAUDE_WORKS}"));
    let work = root().join("claude-cap.work");
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("claude").arg("--program").arg(&claude)
        .arg("--message").arg("do the thing").arg("--budget-tokens").arg("3000")
        .env("HOME", root().join("home")).env("RINGO_NO_UPDATE_CHECK", "1")
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").env_remove("CODEX_HOME")
        .output().unwrap();
    let rec: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(rec["outcome"], "cap_breach");
    assert_eq!(rec["agent"]["cap_detection"], "live");
    assert!(rec["stopped_by"]["tokens"].as_u64().unwrap() > 3000, "the count that showed the breach is recorded");
    assert!(rec["usage"]["counted"].as_bool().unwrap(), "the stopped turn's messages' counts are kept");
    assert!(marker_stops(&work, Duration::from_secs(5)), "the stopped turn's process was ended");
}

/// The wall cap stops a turn whose usage never arrives: with no count, usage stays unknown, not
/// zero, and the marker still stops.
#[test]
fn stops_at_the_wall_cap() {
    let r = repo("wall");
    let claude = fake(&root(), "claude-slow", &format!("export TURNRUN_UNITS=1\n{CLAUDE_WORKS}"));
    let work = root().join("claude-slow.work");
    let out = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("claude").arg("--program").arg(&claude)
        .arg("--message").arg("do the thing").arg("--budget-seconds").arg("1")
        .env("HOME", root().join("home")).env("RINGO_NO_UPDATE_CHECK", "1")
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").env_remove("CODEX_HOME")
        .output().unwrap();
    let rec: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(rec["outcome"], "timeout");
    assert!(rec["stopped_by"]["seconds"].as_u64().unwrap() >= 1);
    assert!(marker_stops(&work, Duration::from_secs(5)), "the stopped turn's process was ended");
}

/// A codex whose usage arrives with its completion, in one write, after a pause: the runner sees a
/// finished turn past its cap, not a stoppable one.
const CODEX_OVER: &str = r#"#!/bin/sh
read -r hello; read -r ready; read -r thread
echo '{"id":1,"result":{}}'
echo '{"id":2,"result":{"thread":{"id":"t-1"}}}'
read -r turn
echo "made it" > made.txt
echo '{"method":"item/completed","params":{"threadId":"t-1","item":{"type":"agentMessage","text":"all done"}}}'
sleep 0.3
printf '%s\n%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"t-1","tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":4,"outputTokens":2}}}}' '{"method":"turn/completed","params":{"threadId":"t-1","turn":{"status":"completed"}}}'
"#;

/// A post-turn backend reports its usage a moment before it finishes, so the cap may stop the turn
/// (outcome `cap_breach`) or see it only at the end (`done` with `cap_overshoot`). Which one depends
/// on timing, and the runner must not pretend otherwise: what is pinned here is that neither
/// outcome passes silently — a finished turn past its cap always carries `cap_overshoot`.
#[test]
fn a_post_turn_turn_past_its_cap_is_never_recorded_as_a_quiet_pass() {
    fs::create_dir_all(root()).unwrap();
    let codex = fake(&root(), "codex-over", CODEX_OVER);
    for i in 0..3 {
        let r = repo(&format!("over-{i}")); // a fresh fixture: the last run's files would dirty it
        let (code, rec) = turnrun(&r, &codex, &["--budget-tokens", "5"]);
        match rec["outcome"].as_str().unwrap() {
            "done" => {
                assert_eq!(code, 0);
                assert_eq!(rec["cap_overshoot"]["tokens"], 12);
                assert_eq!(rec["cap_overshoot"]["cap"], 5);
            }
            "cap_breach" => {
                assert_eq!(code, 3);
                assert_eq!(rec["stopped_by"]["cap"], 5);
                assert!(rec["stopped_by"]["tokens"].as_u64().unwrap() > 5);
            }
            other => panic!("a turn past its cap was recorded as {other}"),
        }
    }
}

/// A kept conversation handed to the runner goes through the actual restart path: the seeded turns
/// before the restart reach the agent's message, once.
#[test]
fn seeds_a_restarted_conversation_through_the_handoff() {
    let r = repo("resume");
    let codex = fake(&root(), "codex-resume", CODEX_OK);
    let kept = root().join("resume.json");
    fs::write(&kept, serde_json::json!({
        "threads": [["codex", "t-old"]],
        "told": [["codex", "The owner asked for a retry with backoff, and the turn was cut short."]],
        "records": [],
        "usage": [["codex", "t-old", [100, 0, 0, 20]]],
        "separate": false, "reached": [], "conversation": null, "about": "test", "saved": 1
    }).to_string()).unwrap();
    let (code, rec) = turnrun(&r, &codex, &["--resume-json", kept.to_str().unwrap()]);
    assert_eq!((code, rec["outcome"].as_str().unwrap()), (0, "done"));
    assert_eq!(rec["resume"]["path"], kept.to_str().unwrap());
    assert_eq!(rec["resume"]["sha256"].as_str().map(str::len), Some(64), "the seed is pinned by its hash");
    let sent = codex_turn(&codex);
    assert!(sent.contains("Before Ringo restarted"), "the seeded record travels as the restart handoff: {sent}");
    assert!(sent.contains("The owner asked for a retry with backoff"), "the seeded text is in the message: {sent}");
    assert!(sent.ends_with("The owner's new message:\ndo the thing"), "the scripted message follows it: {sent}");
}

#[test]
fn termination_cleans_up_the_sessions_separate_agent_group() {
    let r = repo("terminate");
    let claude = fake(&root(), "claude-terminate", &format!("export TURNRUN_UNITS=1\n{CLAUDE_WORKS}"));
    let work = root().join("claude-terminate.work");
    let mut child = Command::new(env!("CARGO_BIN_EXE_turnrun"))
        .arg("--project").arg(&r).arg("--agent").arg("claude").arg("--program").arg(&claude)
        .args(["--message", "keep working"])
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME")
        .stdout(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !work.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(work.exists(), "the fake agent started");
    // SAFETY: this is the runner process this test just spawned.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM); }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the runner did not finish shutdown");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker_stops(&work, Duration::from_secs(3)), "the separate agent group ended");
}

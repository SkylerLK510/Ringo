//! The conductor against fake agents in throwaway git repositories (Unix: the fakes are shell scripts).
#![cfg(unix)]

use ringo_cli::{Agent, Session, Status};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

const CODEX_OK: &str = r#"#!/bin/sh
echo "$@" >> "$0.args"
env > "$0.env"
cat > "$0.stdin"
case "$*" in *workspace-write*) echo made > made.txt
  echo "{\"type\":\"item.started\",\"item\":{\"type\":\"file_change\",\"changes\":[{\"path\":\"$PWD/made.txt\",\"kind\":\"add\"}]}}" ;; esac
echo '{"type":"thread.started","thread_id":"t-1"}'
echo '{"type":"item.started","item":{"type":"command_execution","command":"ls"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"all done"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,"output_tokens":2}}'
"#;

fn root() -> PathBuf {
    static ONCE: Once = Once::new();
    let root = std::env::temp_dir().join(format!("ringo-cli-test-{}", std::process::id()));
    ONCE.call_once(|| {
        fs::create_dir_all(&root).unwrap();
        // SAFETY: set once, before any test reads it.
        unsafe { std::env::set_var("RINGO_RELEASE_HOME", root.join("state")) };
    });
    root
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
}

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
    let path = dir.join(format!("fake-{name}"));
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn wait(s: &mut Session, id: usize) -> Status {
    let deadline = Instant::now() + Duration::from_secs(20);
    while s.job(id).unwrap().running() {
        assert!(Instant::now() < deadline, "job {id} did not finish");
        s.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    s.job(id).unwrap().status.clone()
}

fn session(repo: &Path, agent: Agent, program: &Path) -> Session {
    let mut s = Session::new(repo).unwrap();
    s.programs.insert(agent, program.to_path_buf());
    s
}

#[test]
fn read_turns_continue_one_conversation_one_at_a_time() {
    let r = repo("read");
    let codex = fake(&root(), "codex-read", &CODEX_OK.replace("cat >", "sleep 0.3; cat >"));
    let mut s = session(&r, Agent::Codex, &codex);
    assert!(!s.dir.starts_with(&r), "state is outside the repository");
    assert_eq!(fs::metadata(&s.dir).unwrap().permissions().mode() & 0o777, 0o700);
    let first = s.launch_agent(Agent::Codex, false, "what is a.txt?").unwrap();
    assert!(s.launch_agent(Agent::Codex, false, "again").unwrap_err().contains("busy"));
    assert_eq!(wait(&mut s, first), Status::Done);
    let job = s.job(first).unwrap();
    assert_eq!(job.reply(), "all done");
    assert_eq!(job.tokens.total(), 12);
    assert_eq!(fs::read_to_string(codex.with_extension("stdin")).unwrap(), "what is a.txt?");
    let second = s.launch_agent(Agent::Codex, false, "and then?").unwrap();
    assert_eq!(wait(&mut s, second), Status::Done);
    let args = fs::read_to_string(format!("{}.args", codex.display())).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert!(lines[0].contains("-s read-only") && lines[0].contains("forced_login_method"));
    assert!(lines[1].starts_with("exec resume t-1") && lines[1].contains("sandbox_mode=\"read-only\""));
    assert!(fs::read_to_string(r.join("a.txt")).unwrap() == "one\n" && !r.join("made.txt").exists());
}

#[test]
fn agents_get_no_api_keys_and_cannot_start_ringo() {
    let r = repo("env");
    let codex = fake(&root(), "codex-env", CODEX_OK);
    let mut s = session(&r, Agent::Codex, &codex);
    // SAFETY: only this test's child reads these.
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "x");
    }
    let id = s.launch_agent(Agent::Codex, false, "hi").unwrap();
    wait(&mut s, id);
    let env = fs::read_to_string(format!("{}.env", codex.display())).unwrap();
    assert!(!env.contains("OPENAI_API_KEY") && !env.contains("ANTHROPIC_AUTH_TOKEN"));
    assert!(env.lines().any(|l| l == "RINGO_CHILD=1"));
    let ringo = Command::new(env!("CARGO_BIN_EXE_ringo")).arg("--help").env("RINGO_CHILD", "1").output().unwrap();
    assert_eq!(ringo.status.code(), Some(2));
    let python = Command::new("python3").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../ringo.py"))
        .arg("--help").env("RINGO_CHILD", "1").output().unwrap();
    assert_eq!(python.status.code(), Some(2), "ringo.py refuses too");
}

#[test]
fn write_jobs_stay_in_their_worktree_until_applied() {
    let r = repo("write");
    let codex = fake(&root(), "codex-write", CODEX_OK);
    let mut s = session(&r, Agent::Codex, &codex);
    let (one, two) = (s.launch_agent(Agent::Codex, true, "make it").unwrap(),
                      s.launch_agent(Agent::Codex, true, "make it too").unwrap());
    assert_eq!((wait(&mut s, one), wait(&mut s, two)), (Status::Done, Status::Done));
    assert!(!r.join("made.txt").exists(), "nothing lands without /apply");
    assert_eq!(s.overlaps(one), vec![(two, vec!["made.txt".to_string()])], "both jobs edit made.txt");
    assert!(s.diff(one).unwrap().contains("made.txt"));
    assert!(s.expand("check {#1} please").unwrap().contains("--- change made by job #1 ---"));
    fs::write(r.join("a.txt"), "dirty\n").unwrap();
    assert!(s.apply(one).unwrap_err().contains("uncommitted"));
    git(&r, &["checkout", "--", "a.txt"]);
    s.apply(one).unwrap();
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "made\n");
    let wt = s.job(one).unwrap().worktree.clone().unwrap();
    s.drop_worktree(one).unwrap();
    assert!(!wt.exists());
}

#[test]
fn kill_stops_the_whole_process_group() {
    let r = repo("kill");
    let slow = fake(&root(), "codex-slow", "#!/bin/sh\nsleep 30 &\necho $! > \"$0.child\"\nsleep 30\n");
    let mut s = session(&r, Agent::Codex, &slow);
    let id = s.launch_agent(Agent::Codex, false, "wait").unwrap();
    let child_file = PathBuf::from(format!("{}.child", slow.display()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !child_file.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(s.expand("{#1}").unwrap_err().contains("not finished"));
    s.kill(id).unwrap();
    assert_eq!(wait(&mut s, id), Status::Cancelled);
    let child: i32 = fs::read_to_string(&child_file).unwrap().trim().parse().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(child, 0) }, -1, "the agent's own child is gone too");
}

#[test]
fn malformed_output_and_api_key_logins_fail_closed() {
    let r = repo("fail");
    let garbage = fake(&root(), "codex-garbage", "#!/bin/sh\ncat >/dev/null\necho 'not json'\n");
    let mut s = session(&r, Agent::Codex, &garbage);
    let id = s.launch_agent(Agent::Codex, false, "x").unwrap();
    assert!(matches!(wait(&mut s, id), Status::Failed(_)));
    let keyed = fake(&root(), "claude-keyed", "#!/bin/sh\ncat >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}'\nsleep 30\n");
    s.programs.insert(Agent::Claude, keyed);
    let id = s.launch_agent(Agent::Claude, false, "x").unwrap();
    let started = Instant::now();
    match wait(&mut s, id) {
        Status::Failed(reason) => assert!(reason.contains("API key")),
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10), "the turn was stopped, not left to run");
}

#[test]
fn limits_are_refused_clearly() {
    let r = repo("limits");
    let slow = fake(&root(), "codex-limit", "#!/bin/sh\nsleep 30\n");
    let mut s = session(&r, Agent::Codex, &slow);
    s.max_jobs = 1;
    let id = s.launch_agent(Agent::Codex, true, "x").unwrap();
    assert!(s.launch_agent(Agent::Codex, true, "y").unwrap_err().contains("limit 1"));
    assert!(s.launch_agent(Agent::Grok, false, "z").is_err());
    s.kill(id).unwrap();
    wait(&mut s, id);
}

#[test]
fn arguments_pass_through_to_ringo_py() {
    let out = Command::new(env!("CARGO_BIN_EXE_ringo")).args(["wait", "/nonexistent-ringo-run", "--timeout", "0"])
        .output().unwrap();
    assert_eq!(out.status.code(), Some(3)); // ringo.py wait: not started
    assert!(String::from_utf8_lossy(&out.stdout).contains("not_started"));
    let help = Command::new(env!("CARGO_BIN_EXE_ringo")).stdin(std::process::Stdio::null()).output().unwrap();
    assert_eq!(help.status.code(), Some(2), "no arguments and no terminal: usage, not the shell");
}

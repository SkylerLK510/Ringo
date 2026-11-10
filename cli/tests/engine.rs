//! The conductor against fake agents in throwaway git repositories (Unix: the fakes are shell scripts).
#![cfg(unix)]

use ringo_cli::{kept, Agent, Decision, Kept, Mode, Session, Status};

fn agent(name: &str) -> Agent {
    Agent::from_name(name).unwrap()
}
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

/// A fake `codex app-server`: records its arguments (`.argv`, one a line; `.args`, one line a run), environment,
/// thread request (`.thread`, one line a run) and turn request (`.turn`), runs `act` (shell, with $thread and $turn
/// set) and prints `events`. Thread "t-1".
fn codex_script(act: &str, events: &str) -> String {
    format!(r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
echo "$@" >> "$0.args"
env > "$0.env"
read -r hello; read -r ready; read -r thread
printf '%s\n' "$thread" >> "$0.thread"
echo '{{"id":1,"result":{{}}}}'
echo '{{"id":2,"result":{{"thread":{{"id":"t-1"}}}}}}'
read -r turn
printf '%s\n' "$turn" > "$0.turn"
{act}
{events}
"#)
}

/// A turn's reply, its usage so far (input, cached, output) and its end.
fn codex_done(reply: &str, usage: (u64, u64, u64)) -> String {
    format!(r#"echo '{{"method":"item/started","params":{{"threadId":"t-1","item":{{"type":"commandExecution","command":"ls"}}}}}}'
echo '{{"method":"item/completed","params":{{"threadId":"t-1","item":{{"type":"agentMessage","text":"{reply}"}}}}}}'
echo '{{"method":"thread/tokenUsage/updated","params":{{"threadId":"t-1","tokenUsage":{{"total":{{"inputTokens":{},"cachedInputTokens":{},"outputTokens":{}}}}}}}}}'
echo '{{"method":"turn/completed","params":{{"threadId":"t-1","turn":{{"status":"completed"}}}}}}'"#, usage.0, usage.1, usage.2)
}

/// Edits when its sandbox lets it.
const CODEX_EDITS: &str = r#"case "$thread" in *workspace-write*) echo made > made.txt; printf 'y\nx\n\n' > b.txt; printf '\000\001\002binary' > c.bin
  echo "{\"method\":\"item/started\",\"params\":{\"threadId\":\"t-1\",\"item\":{\"type\":\"fileChange\",\"changes\":[{\"path\":\"$PWD/made.txt\"}]}}}" ;; esac"#;

fn codex_ok() -> String {
    codex_script(CODEX_EDITS, &codex_done("all done", (10, 4, 2)))
}

/// What the fake codex was last asked to do, in its turn request.
fn turn_text(codex: &Path) -> String {
    let turn: serde_json::Value = serde_json::from_str(&fs::read_to_string(format!("{}.turn", codex.display())).unwrap()).unwrap();
    turn["params"]["input"][0]["text"].as_str().unwrap().to_string()
}

fn threads(codex: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(format!("{}.thread", codex.display())).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap()).collect()
}

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
fn turns_in_the_folder_continue_one_conversation_one_at_a_time_and_undo() {
    let r = repo("read");
    // A resumed thread reports its usage so far, as the real Codex does.
    let events = format!("case \"$thread\" in *thread/resume*)\n{}\n;; *)\n{}\n;; esac",
                         codex_done("all done", (20, 8, 4)), codex_done("all done", (10, 4, 2)));
    let codex = fake(&root(), "codex-read", &codex_script(&format!("sleep 0.3\n{CODEX_EDITS}"), &events));
    let mut s = session(&r, agent("codex"), &codex);
    assert!(!s.dir.starts_with(&r), "state is outside the repository");
    assert_eq!(fs::metadata(&s.dir).unwrap().permissions().mode() & 0o777, 0o700);
    let first = s.launch_agent(agent("codex"), false, "what is a.txt?").unwrap();
    assert!(s.launch_agent(agent("codex"), false, "again").unwrap_err().contains("busy"));
    assert_eq!(wait(&mut s, first), Status::Done);
    let job = s.job(first).unwrap();
    assert_eq!(job.reply(), "all done");
    assert_eq!(job.tokens.total(), 12);
    assert_eq!(turn_text(&codex), "what is a.txt?");
    let second = s.launch_agent(agent("codex"), false, "and then?").unwrap();
    assert_eq!(wait(&mut s, second), Status::Done);
    assert_eq!(s.job(second).unwrap().tokens.total(), 12, "only what the resumed turn added (24 so far)");
    let (args, sent) = (fs::read_to_string(format!("{}.args", codex.display())).unwrap(), threads(&codex));
    assert!(args.lines().all(|l| l.starts_with("app-server") && l.contains("forced_login_method")));
    assert_eq!((sent[0]["method"].as_str(), sent[0]["params"]["sandbox"].as_str()), (Some("thread/start"), Some("workspace-write")),
               "ask mode edits here");
    assert_eq!((sent[1]["method"].as_str(), sent[1]["params"]["threadId"].as_str(), sent[1]["params"]["sandbox"].as_str()),
               (Some("thread/resume"), Some("t-1"), Some("workspace-write")));
    // The first turn's edits landed in the folder and are known; the second wrote the same again, so changed nothing.
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "made\n");
    assert_eq!(s.job(first).unwrap().changed, ["b.txt", "c.bin", "made.txt"]);
    assert!(s.job(second).unwrap().changed.is_empty());
    let status = Command::new("git").arg("-C").arg(&r).args(["status", "--porcelain"]).output().unwrap().stdout;
    assert_eq!(String::from_utf8_lossy(&status), "?? b.txt\n?? c.bin\n?? made.txt\n", "nothing staged, nothing stashed");
    assert!(s.undo().unwrap().contains("undid #1"));
    assert!(!r.join("made.txt").exists() && !r.join("c.bin").exists() && fs::read_to_string(r.join("a.txt")).unwrap() == "one\n");
    assert!(s.undo().unwrap_err().contains("nothing to undo"));
    // A file the owner edited after the turn: /undo refuses and touches nothing.
    let third = s.launch_agent(agent("codex"), false, "once more").unwrap();
    assert_eq!(wait(&mut s, third), Status::Done);
    fs::write(r.join("made.txt"), "mine\n").unwrap();
    assert!(s.undo().unwrap_err().contains("made.txt changed after #3"));
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "mine\n");
    assert!(r.join("c.bin").exists());
    // Read mode: the same agent only looks.
    s.mode = Mode::Read;
    let looked = s.launch_agent(agent("codex"), false, "just look").unwrap();
    assert_eq!(wait(&mut s, looked), Status::Done);
    assert_eq!(threads(&codex).last().unwrap()["params"]["sandbox"], "read-only");
    assert!(s.job(looked).unwrap().before.is_none(), "a turn that cannot edit keeps no before");
}

#[test]
fn undo_refuses_after_any_later_edit_and_while_a_turn_runs() {
    let r = repo("undo");
    let lines: String = (1..=12).map(|i| format!("line {i}\n")).collect();
    fs::write(r.join("long.txt"), &lines).unwrap();
    git(&r, &["add", "."]);
    git(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "long"]);
    // An agent that changes the first line, and waits first when asked to.
    let codex = fake(&root(), "codex-undo", &codex_script("case \"$turn\" in *slow*) sleep 1 ;; esac
{ echo changed; tail -n +2 long.txt; } > t && mv t long.txt", &codex_done("done", (1, 0, 1))));
    let mut s = session(&r, agent("codex"), &codex);
    let id = s.launch_agent(agent("codex"), false, "change it").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    assert_eq!(s.job(id).unwrap().changed, ["long.txt"]);
    // The owner's own line at the end, far from the turn's: git apply would take the turn's back anyway.
    fs::write(r.join("long.txt"), format!("changed\n{}mine\n", &lines["line 1\n".len()..])).unwrap();
    assert!(s.undo().unwrap_err().contains("long.txt changed after #1"));
    assert!(fs::read_to_string(r.join("long.txt")).unwrap().ends_with("mine\n"), "nothing was undone");
    fs::write(r.join("long.txt"), format!("changed\n{}", &lines["line 1\n".len()..])).unwrap();
    // While a turn works in the folder, neither /undo nor /apply touches it.
    let slow = s.launch_agent(agent("codex"), false, "slow").unwrap();
    assert!(s.undo().unwrap_err().contains(&format!("#{slow} is working in this folder")));
    assert_eq!(wait(&mut s, slow), Status::Done);
    assert!(s.job(slow).unwrap().changed.is_empty(), "the first line was already changed");
    assert!(s.undo().unwrap().contains("undid #1"));
    assert_eq!(fs::read_to_string(r.join("long.txt")).unwrap(), lines);
}

#[test]
fn agents_get_no_api_keys_and_cannot_start_ringo() {
    let r = repo("env");
    let codex = fake(&root(), "codex-env", &codex_ok());
    let mut s = session(&r, agent("codex"), &codex);
    // SAFETY: only this test's child reads these.
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "x");
    }
    let id = s.launch_agent(agent("codex"), false, "hi").unwrap();
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
    fs::write(r.join("b.txt"), "x\n\n").unwrap(); // ends in a blank line: patch context that trimming would break
    git(&r, &["add", "."]);
    git(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "b"]);
    let codex = fake(&root(), "codex-write", &codex_ok());
    let mut s = session(&r, agent("codex"), &codex);
    let (one, two) = (s.launch_agent(agent("codex"), true, "make it").unwrap(),
                      s.launch_agent(agent("codex"), true, "make it too").unwrap());
    assert_eq!((wait(&mut s, one), wait(&mut s, two)), (Status::Done, Status::Done));
    assert!(!r.join("made.txt").exists(), "nothing lands without /apply");
    assert_eq!(s.overlaps(one), vec![(two, vec!["made.txt".to_string()])], "both jobs edit made.txt");
    assert!(s.diff(one).unwrap().contains("made.txt"));
    let handoff = s.expand("check {#1} please").unwrap();
    assert!(handoff.contains("--- change made by job #1 ---") && handoff.contains("made.txt"));
    assert!(handoff.contains("Binary files") && !handoff.contains("GIT binary patch"),
            "a binary file is named, not sent as base85: {handoff}");
    assert_eq!(s.expand("render {# note #} and {#x} here").unwrap(), "render {# note #} and {#x} here",
               "only {{#N}} is a handoff");
    // The copies began as the folder was, so a change applies onto it with the owner's own edit kept; a clash with
    // that edit leaves everything as it was.
    fs::write(r.join("a.txt"), "dirty\n").unwrap();
    fs::write(r.join("made.txt"), "mine\n").unwrap();
    assert!(s.apply(one).unwrap_err().contains("your files changed where #1 edits them"));
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "mine\n");
    fs::remove_file(r.join("made.txt")).unwrap();
    assert!(s.apply(one).unwrap().contains("made.txt"));
    assert_eq!(fs::read_to_string(r.join("a.txt")).unwrap(), "dirty\n");
    assert_eq!(fs::read_to_string(r.join("made.txt")).unwrap(), "made\n");
    assert_eq!(fs::read_to_string(r.join("b.txt")).unwrap(), "y\nx\n\n");
    assert_eq!(fs::read(r.join("c.bin")).unwrap(), b"\x00\x01\x02binary", "/apply still carries binary files");
    let wt = s.job(one).unwrap().worktree.clone().unwrap();
    s.drop_worktree(one).unwrap();
    assert!(!wt.exists());
}

#[test]
fn kill_stops_the_whole_process_group() {
    let r = repo("kill");
    let slow = fake(&root(), "codex-slow", "#!/bin/sh\nsleep 30 &\necho $! > \"$0.child\"\nsleep 30\n");
    let mut s = session(&r, agent("codex"), &slow);
    let id = s.launch_agent(agent("codex"), false, "wait").unwrap();
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
    let garbage = fake(&root(), "codex-garbage", "#!/bin/sh\nread -r hello\necho 'not json'\n");
    let mut s = session(&r, agent("codex"), &garbage);
    let id = s.launch_agent(agent("codex"), false, "x").unwrap();
    assert!(matches!(wait(&mut s, id), Status::Failed(_)));
    let keyed = fake(&root(), "claude-keyed", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}'\nsleep 30\n");
    s.programs.insert(agent("claude"), keyed);
    let id = s.launch_agent(agent("claude"), false, "x").unwrap();
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
    let mut s = session(&r, agent("codex"), &slow);
    s.max_jobs = 1;
    let id = s.launch_agent(agent("codex"), true, "x").unwrap();
    assert!(s.launch_agent(agent("codex"), true, "y").unwrap_err().contains("limit 1"));
    assert!(s.launch_agent(agent("grok"), false, "z").is_err());
    s.kill(id).unwrap();
    wait(&mut s, id);
}

#[test]
fn no_job_starts_while_an_agent_update_holds_the_session() {
    let r = repo("hold");
    let quick = fake(&root(), "codex-hold", "#!/bin/sh\nexit 0\n");
    let mut s = session(&r, agent("codex"), &quick);
    s.hold = Some("updating codex".into());
    let err = s.launch_agent(agent("codex"), false, "x").unwrap_err();
    assert!(err.contains("updating codex"), "{err}");
    assert!(s.ask("why?").unwrap_err().contains("updating codex"));
    assert!(s.jobs.is_empty());
    s.hold = None;
    let id = s.launch_agent(agent("codex"), false, "x").unwrap();
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

#[test]
fn state_inside_the_repository_is_refused() {
    // a repository under the crate folder (the tests' working folder), so a relative state root can point into it
    let r = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("inside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&r);
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q"]);
    let relative = Path::new("target").join(r.file_name().unwrap()).join("private-state");
    assert!(Session::with_state(&r, &relative).err().unwrap().contains("inside the repository"));
    let link = root().join("inside-link");
    let _ = std::os::unix::fs::symlink(r.join("state"), &link);
    assert!(Session::with_state(&r, &link).err().unwrap().contains("symlink"));
    let outside = root().join("outside-state");
    fs::create_dir_all(&outside).unwrap();
    let _ = std::os::unix::fs::symlink(&r, outside.join("shell")); // the shell folder itself points into the repo
    assert!(Session::with_state(&r, &outside).err().unwrap().contains("inside the repository"));
    fs::remove_dir_all(&r).unwrap();
}

#[test]
fn a_repository_holding_the_home_folder_is_not_a_project() {
    // A dotfiles repository at the home folder: the session works in the current folder, keeps its private state
    // under the home folder (git-ignored there), answers questions, and refuses worktree edits with a clear reason.
    let home = repo("dotfiles-home");
    let docs = home.join("Documents");
    fs::create_dir_all(&docs).unwrap();
    let state = home.join(".local/share/ringo");
    let mut s = Session::with_state_home(&docs, &state, &home).unwrap();
    assert_eq!((s.repo.clone(), s.git), (docs.canonicalize().unwrap(), false));
    assert!(fs::read_to_string(state.join(".gitignore")).unwrap().contains('*'));
    let status = Command::new("git").arg("-C").arg(&home).args(["status", "--porcelain", "--untracked-files=all"])
        .output().unwrap();
    assert!(!String::from_utf8_lossy(&status.stdout).contains(".local"), "the home repository sees the private state");
    let refused = s.launch_agent(agent("codex"), true, "edit it").unwrap_err();
    assert!(refused.contains("needs a git project"), "{refused}");
    assert!(s.review_repo("HEAD~1", "HEAD").unwrap_err().contains("git project"));
    // a project inside the home folder is still a project, and its own state rule still holds
    let project = repo("project-under-home");
    let s = Session::with_state_home(&project, &state, &home).unwrap();
    assert!(s.git);
    // the home folder itself gets no undo store: hashing all of it at every turn would be far too slow
    let s = Session::with_state_home(&home, &state, &home).unwrap();
    assert!(!s.undoable() && s.no_undo.as_deref().unwrap().contains("home folder"));
}

#[test]
fn turns_outside_git_undo_from_ringos_own_store() {
    // A school folder inside a dotfiles repository at the home folder, as on the owner's machine: the session is
    // not a project, yet each turn can be undone, from a store under Ringo's state that the folder never sees.
    let home = repo("school-home");
    let class = home.join("CSC138");
    fs::create_dir_all(class.join("src")).unwrap();
    fs::write(class.join("src/Main.java"), "class Main {}\n").unwrap();
    fs::write(class.join("b.txt"), "x\n\n").unwrap();
    let state = home.join(".local/share/ringo");
    let codex = fake(&root(), "codex-school", &codex_ok());
    let mut s = Session::with_state_home(&class, &state, &home).unwrap();
    s.programs.insert(agent("codex"), codex.clone());
    assert!(!s.git && s.undoable() && s.no_undo.is_none());
    let id = s.launch_agent(agent("codex"), false, "add a file").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    assert_eq!(s.job(id).unwrap().changed, ["b.txt", "c.bin", "made.txt"]);
    assert!(!class.join(".git").exists(), "the folder gets no repository of its own");
    let home_status = Command::new("git").arg("-C").arg(&home).args(["status", "--porcelain"]).output().unwrap().stdout;
    assert!(!String::from_utf8_lossy(&home_status).contains(".local"), "the home repository never sees the store");
    assert!(s.expand("check {#1}").unwrap().contains("made.txt"), "a handoff carries the turn's change");
    // a review reads the turn's change from the store: its before and after, committed there and not in the home
    // repository the folder sits in
    let review = fake(&root(), "review-school", "#!/bin/sh\necho \"$@\" > \"$0.args\"\n\
        while [ $# -gt 0 ]; do [ \"$1\" = --out ] && out=$2; shift; done\necho 'no defects' > \"$out\"\n");
    s.pipeline = Some(vec![review.clone().into()]);
    let r = s.review_job(id, "claude").unwrap();
    assert_eq!(wait(&mut s, r), Status::Done);
    let args = fs::read_to_string(review.with_extension("args")).unwrap();
    let arg = |name: &str| args.split_whitespace().skip_while(|a| *a != name).nth(1).unwrap().to_string();
    let store = PathBuf::from(arg("--repo"));
    assert!(store.starts_with(state.canonicalize().unwrap()) && arg("--backend") == "claude", "{args}");
    let names = Command::new("git").arg("-C").arg(&store).args(["diff", "--name-only", &arg("--base"), &arg("--head")])
        .output().unwrap();
    assert_eq!(String::from_utf8_lossy(&names.stdout).lines().collect::<Vec<_>>(), ["b.txt", "c.bin", "made.txt"]);
    let in_home = Command::new("git").arg("-C").arg(&home).args(["cat-file", "-e", &arg("--head")]).status().unwrap();
    assert!(!in_home.success(), "the home repository gets no commit");
    // the owner edits a changed file: nothing is undone; put back, the turn is undone exactly
    fs::write(class.join("made.txt"), "mine\n").unwrap();
    assert!(s.undo().unwrap_err().contains("made.txt changed after #1"));
    fs::write(class.join("made.txt"), "made\n").unwrap();
    assert!(s.undo().unwrap().contains("undid #1"));
    assert!(!class.join("made.txt").exists() && !class.join("c.bin").exists());
    assert_eq!(fs::read_to_string(class.join("b.txt")).unwrap(), "x\n\n");
    assert_eq!(fs::read_to_string(class.join("src/Main.java")).unwrap(), "class Main {}\n");
    // a second session in the same folder reuses the store
    let again = Session::with_state_home(&class, &state, &home).unwrap();
    assert!(again.undoable());
    assert_eq!(fs::read_dir(state.join("undo")).unwrap().count(), 1);
}

/// A stand-in ringo.py in a home folder of its own: it records its argv and the material it was given (seen-N.json),
/// then answers "answer N" through its trace and --out.
fn ask_home(name: &str) -> PathBuf {
    let home = root().join(name);
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("ringo.py"), "import json, sys, pathlib\n\
        a = sys.argv[1:]\n\
        val = lambda k: a[a.index(k) + 1]\n\
        material = pathlib.Path(val('--input')).read_text()\n\
        n = len(list(pathlib.Path(sys.argv[0]).parent.glob('seen-*.json'))) + 1\n\
        pathlib.Path(sys.argv[0]).with_name(f'seen-{n}.json').write_text(json.dumps({'argv': a, 'material': material}))\n\
        t = pathlib.Path(val('--trace')); t.mkdir(parents=True, exist_ok=True)\n\
        (t / 'trace.jsonl').write_text(json.dumps({'step': 'answer', 'backend': 'b', 'status': 'started'}) + '\\n' +\n\
            json.dumps({'step': 'answer', 'status': 'ok', 'usage': {'input': 900, 'cache_write': 0, 'cache_read': 0, 'output': 40}}) + '\\n')\n\
        pathlib.Path(val('--out')).write_text(f'answer {n}\\n')\n").unwrap();
    home
}

#[test]
fn a_plain_question_after_clear_carries_no_earlier_answers() {
    let r = repo("ask-clear");
    let home = ask_home("fake-ask-clear-home");
    let mut s = Session::new(&r).unwrap();
    s.ringo_home = home.clone();
    let first = s.ask("what is a dotfiles repository?").unwrap();
    assert_eq!(wait(&mut s, first), Status::Done);
    s.forget(); // /clear
    let second = s.ask("something else entirely").unwrap();
    assert_eq!(wait(&mut s, second), Status::Done);
    let seen: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join("seen-2.json")).unwrap()).unwrap();
    let material = seen["material"].as_str().unwrap();
    assert!(!material.contains("dotfiles") && !material.contains("answer 1"), "the question before /clear went along: {material}");
    assert!(material.ends_with("Question: something else entirely\n"), "{material}");
    // and the conversation since /clear still carries its own
    let third = s.ask("and then?").unwrap();
    assert_eq!(wait(&mut s, third), Status::Done);
    let seen: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join("seen-3.json")).unwrap()).unwrap();
    assert!(seen["material"].as_str().unwrap().contains("Question: something else entirely\n\nAnswer:\nanswer 2"));
}

#[test]
fn a_plain_question_is_one_tool_free_call_that_remembers_the_last_answers() {
    let r = repo("ask");
    let home = ask_home("fake-ask-home");
    let mut s = Session::new(&r).unwrap();
    s.ringo_home = home.clone();
    let first = s.ask("what is a dotfiles repository?").unwrap();
    assert_eq!(wait(&mut s, first), Status::Done);
    let second = s.ask("and why does it matter here?").unwrap();
    assert_eq!(wait(&mut s, second), Status::Done);
    let seen: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join("seen-2.json")).unwrap()).unwrap();
    let argv: Vec<&str> = seen["argv"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(&argv[..3], &["design", "--pipeline", home.join("plans/ask.json").to_str().unwrap()]);
    let material = seen["material"].as_str().unwrap();
    assert!(material.contains("Question: what is a dotfiles repository?\n\nAnswer:\nanswer 1"), "{material}");
    assert!(material.ends_with("Question: and why does it matter here?\n"), "{material}");
    assert_eq!(s.jobs[second - 1].tokens.total(), 940);
    assert!(s.jobs[second - 1].log.iter().any(|l| l == "answer 2"));
}

#[test]
fn killing_a_pipeline_stops_its_model_calls_too() {
    // Ringo's model calls run in sessions of their own; the interrupt lets Python's cleanup end them.
    let r = repo("pipeline");
    let home = root().join("fake-ringo-home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("ringo.py"), "import subprocess, sys, time\n\
        p = subprocess.Popen(['sleep', '30'], start_new_session=True)\n\
        open(sys.argv[0] + '.child', 'w').write(str(p.pid))\n\
        try:\n    time.sleep(30)\nfinally:\n    p.kill()\n").unwrap();
    let mut s = Session::new(&r).unwrap();
    s.ringo_home = home.clone();
    let id = s.launch_ringo("fake review", vec!["review".into()], root().join("no-trace")).unwrap();
    let child_file = home.join("ringo.py.child");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !child_file.exists() || fs::read_to_string(&child_file).unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    s.kill(id).unwrap();
    assert_eq!(wait(&mut s, id), Status::Cancelled);
    let child: i32 = fs::read_to_string(&child_file).unwrap().trim().parse().unwrap();
    // A killed call stays a zombie until the system reaps it, which a busy machine can take a while to do.
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(Instant::now() < deadline, "the model call in its own session is gone");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Every argument and the stdin each built-in adapter sends, pinned: the registry must reproduce them exactly.
#[test]
fn agent_commands_are_pinned() {
    let r = repo("golden");
    fs::write(r.join("AGENTS.md"), "rules\n").unwrap();
    git(&r, &["add", "."]);
    git(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "rules"]);
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n";
    let codex = fake(&root(), "codex-golden", &codex_script("", &codex_done("done", (1, 0, 1))));
    let claude = fake(&root(), "claude-golden", &format!("#!/bin/sh\n{RECORD}head -n 1 > \"$0.stdin\"\n\
        echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\",\"apiKeySource\":\"none\"}}'\n\
        echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
    let grok = fake(&root(), "grok-golden", &format!("#!/bin/sh\n{RECORD}echo done\n"));
    let mut s = session(&r, agent("codex"), &codex);
    s.codex_home = root().join("codex-home-golden"); // none: the owner's own Codex instructions add nothing
    s.separate = true; // each agent on its own, so no turn here is told another's (see the handoff tests)
    s.programs.insert(agent("claude"), claude.clone());
    s.programs.insert(agent("grok"), grok.clone());
    let repo = s.repo.to_string_lossy().into_owned();
    let mut turn = |agent: Agent, write: bool, program: &Path| -> (Vec<String>, String) {
        let id = s.launch_agent(agent, write, "the task").unwrap();
        assert_eq!(wait(&mut s, id), Status::Done);
        let job = s.job(id).unwrap();
        assert_eq!(job.unchanged, write, "the fakes change nothing");
        let wt = job.worktree.clone().map(|w| w.to_string_lossy().into_owned());
        let argv = fs::read_to_string(format!("{}.argv", program.display())).unwrap_or_default();
        let stdin = fs::read_to_string(format!("{}.stdin", program.display())).unwrap_or_default();
        let _ = fs::remove_file(format!("{}.stdin", program.display()));
        let argv = argv.lines().map(|a| match &wt { Some(w) => a.replace(w.as_str(), "WT"), None => a.to_string() })
            .map(|a| a.replace(&repo, "REPO")).collect();
        (argv, stdin)
    };
    // Codex cannot ask, so the ask tool, its mode text and analytics go; plugins and apps only for a connector task.
    // The thread's settings and the task go in as JSON-RPC requests.
    let argv = ["app-server", "-c", "sandbox_workspace_write.network_access=false", "-c", "forced_login_method=\"chatgpt\"",
                "-c", "model_provider=\"openai\"", "-c", "features.default_mode_request_user_input=false",
                "-c", "include_collaboration_mode_instructions=false", "-c", "analytics.enabled=false",
                "-c", "features.plugins=false", "-c", "features.apps=false", "-c", "features.image_generation=false",
                "-c", "features.goals=false", "-c", "model_auto_compact_token_limit=200000"].map(String::from).to_vec();
    let mut codex_turn = |write: bool| -> (Vec<String>, String, String) {
        let (argv, _) = turn(agent("codex"), write, &codex);
        let thread = threads(&codex).pop().unwrap().to_string();
        let ask = fs::read_to_string(format!("{}.turn", codex.display())).unwrap();
        (argv, thread.replace(&repo, "REPO"), ask.trim_end().to_string())
    };
    let ask = r#"{"id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"the task"}],"threadId":"t-1"}}"#;
    assert_eq!(codex_turn(false), (argv.clone(),
        r#"{"id":2,"method":"thread/start","params":{"cwd":"REPO","sandbox":"workspace-write","approvalPolicy":"never"}}"#.into(), ask.into()));
    assert_eq!(codex_turn(false), (argv.clone(),
        r#"{"id":2,"method":"thread/resume","params":{"cwd":"REPO","sandbox":"workspace-write","approvalPolicy":"never","threadId":"t-1","excludeTurns":true}}"#.into(),
        ask.into()), "a resumed turn in ask mode says the network is off, whatever an earlier turn had");
    let (copy_argv, copy_thread, copy_ask) = codex_turn(true);
    assert_eq!((copy_argv, copy_ask), (argv.clone(), ask.into()));
    assert!(copy_thread.contains(r#""method":"thread/start""#) && copy_thread.contains(r#""sandbox":"workspace-write""#)
            && !copy_thread.contains("REPO\""), "a copy starts fresh in its own worktree: {copy_thread}");
    let hello = fs::read_to_string(format!("{}.args", codex.display())).unwrap();
    assert!(!hello.contains("exec"), "{hello}");
    let want = |head: &[&str], rest: &[&str]| -> Vec<String> { head.iter().chain(rest).map(|a| a.to_string()).collect() };
    // Ask mode (the default): edits go through, every permission prompt comes to Ringo, the always-ask rules ride
    // along; the prompt is one stream-json message on stdin, which stays open for the answers.
    let head = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages", "--replay-user-messages", "--disable-slash-commands", "--safe-mode",
                "--permission-prompt-tool", "stdio", "--permission-mode"];
    let rules = serde_json::json!({"permissions": {"ask": ringo_cli::ALWAYS_ASK},
                                   "attribution": {"commit": "", "pr": ""}}).to_string();
    let deny = |extra: &[&'static str]| -> Vec<&'static str> {
        ["--disallowed-tools"].into_iter().chain(extra.iter().copied()).chain(ringo_cli::OUT_OF_TURN_TOOLS)
            .chain(["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"]).collect()
    };
    let here = [&["acceptEdits"][..], &deny(&[]), &["--settings", &rules]].concat();
    let message = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"the task\"}}\n".to_string();
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&here[..], &["--append-system-prompt-file", "REPO/AGENTS.md"]].concat()), message.clone()));
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&here[..], &["--resume", "s-1", "--append-system-prompt-file", "REPO/AGENTS.md"]].concat()),
                message.clone()));
    assert_eq!(turn(agent("claude"), true, &claude),
               (want(&head, &[&["acceptEdits"][..], &deny(&[]), &["--settings", &rules, "--append-system-prompt-file",
                                                                  "WT/AGENTS.md"]].concat()),
                message.clone()));
    assert_eq!(turn(agent("grok"), true, &grok),
               (want(&["-p", "the task", "--output-format", "plain", "--no-auto-update", "--always-approve", "--cwd", "WT"], &[]),
                String::new()));
}

/// A Codex turn is told it is already in Ringo when the owner's own Codex instructions send Codex to Ringo (one small
/// task otherwise read Ringo's guide and started a review inside the turn), and only then; a Claude /compact turn
/// keeps Claude Code's commands, which every other turn goes without.
#[test]
fn codex_is_told_it_is_in_ringo_and_compact_keeps_claudes_commands() {
    let r = repo("in-ringo");
    let codex = fake(&root(), "codex-in-ringo", &codex_script("", &codex_done("done", (1, 0, 1))));
    let claude = fake(&root(), "claude-in-ringo", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n\
        head -n 1 > /dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-i\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n");
    let mut s = session(&r, agent("codex"), &codex);
    s.programs.insert(agent("claude"), claude.clone());
    let home = root().join("codex-home-in-ringo");
    fs::create_dir_all(&home).unwrap();
    s.codex_home = home.clone();
    let told = |s: &mut Session| -> Option<String> {
        let id = s.launch_agent(agent("codex"), false, "the task").unwrap();
        assert_eq!(wait(s, id), Status::Done);
        threads(&codex).pop().unwrap()["params"]["developerInstructions"].as_str().map(String::from)
    };
    assert_eq!(told(&mut s), None, "no instructions of the owner's: nothing is added");
    fs::write(home.join("AGENTS.md"), "Use RINGO as the default harness.\n").unwrap();
    let said = told(&mut s).expect("a resumed turn is told too");
    assert!(said.contains("do not read Ringo's guides or run Ringo inside it") && !said.contains("review"), "{said}");
    assert_eq!(threads(&codex).last().unwrap()["method"], "thread/resume");
    fs::write(home.join("AGENTS.override.md"), " \n").unwrap();
    assert!(told(&mut s).is_some(), "an override with nothing in it is passed over");
    fs::write(home.join("AGENTS.override.md"), "Be brief.\n").unwrap();
    assert_eq!(told(&mut s), None, "Codex reads the override in its place");
    fs::remove_file(home.join("AGENTS.override.md")).unwrap();
    fs::write(home.join("config.toml"), "model = \"m\"\ndeveloper_instructions = \"Mine.\"\n").unwrap();
    assert_eq!(told(&mut s), None, "the owner's own developer instructions are not replaced");
    let argv = |s: &mut Session, task: &str| -> String {
        let id = s.launch_agent(agent("claude"), false, task).unwrap();
        assert_eq!(wait(s, id), Status::Done);
        fs::read_to_string(format!("{}.argv", claude.display())).unwrap()
    };
    assert!(argv(&mut s, "the task").lines().any(|a| a == "--disable-slash-commands"));
    assert!(!argv(&mut s, "/compact").lines().any(|a| a == "--disable-slash-commands"), "/compact is a command");
}

/// Every Claude turn runs with a 200k auto-compact window, so a long conversation is compacted by Claude itself
/// instead of growing toward the 1M window; the owner's own rules file, when there is one, goes in ahead of the
/// project's instructions.
#[test]
fn claude_turns_compact_at_200k_and_carry_the_owners_rules() {
    let r = repo("compact-rules");
    fs::write(r.join("AGENTS.md"), "Project rule.\n").unwrap();
    let claude = fake(&root(), "claude-rules", "#!/bin/sh\nprintf '%s' \"$CLAUDE_CODE_AUTO_COMPACT_WINDOW\" > \"$0.window\"\n\
        rm -f \"$0.appended\"\nwhile [ $# -gt 0 ]; do [ \"$1\" = --append-system-prompt-file ] && cat \"$2\" > \"$0.appended\"; shift; done\n\
        head -n 1 > /dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-r\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n");
    let mut s = Session::with_state(&r, &root().join("state-compact-rules")).unwrap();
    s.programs.insert(agent("claude"), claude.clone());
    let run = |s: &mut Session| {
        let id = s.launch_agent(agent("claude"), false, "the task").unwrap();
        assert_eq!(wait(s, id), Status::Done);
        (fs::read_to_string(claude.with_extension("window")).unwrap(),
         fs::read_to_string(claude.with_extension("appended")).unwrap_or_default())
    };
    // an owner who sets the window keeps it
    let window = std::env::var("CLAUDE_CODE_AUTO_COMPACT_WINDOW").unwrap_or_else(|_| "200000".into());
    assert_eq!(run(&mut s), (window, "Project rule.\n".to_string()), "no rules file: as before");
    fs::write(s.rules_path(), "My rule.\n\n").unwrap();
    assert_eq!(run(&mut s).1, "My rule.\n\nProject rule.\n", "the owner's rules first, then the project's");
    fs::remove_file(r.join("AGENTS.md")).unwrap();
    assert_eq!(run(&mut s).1, "My rule.\n\n", "rules alone");
    fs::write(s.rules_path(), "  \n").unwrap();
    assert_eq!(run(&mut s).1, "", "an empty rules file adds nothing");
    // files that are not UTF-8 go in as they are, and the turn runs
    fs::write(s.rules_path(), b"R\xe9gle.\n").unwrap();
    fs::write(r.join("CLAUDE.md"), b"Projet \xe9t\xe9.\n").unwrap();
    let id = s.launch_agent(agent("claude"), false, "the task").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done, "the turn runs");
    assert_eq!(fs::read(claude.with_extension("appended")).unwrap(), b"R\xe9gle.\n\nProjet \xe9t\xe9.\n");
}

/// agy (verified with 1.4.12): the prompt on stdin as one stream-json message, stream-json out, session-only deny
/// rules (read turns cannot write files; no turn runs commands, MCP tools or browser actions), resume by
/// conversation id, where agy reports the conversation's usage so far and only the turn's own is counted. The prompt
/// is never an argument, so a leading `-` or a quote cannot change the command line.
#[test]
fn agy_turns_and_their_permissions() {
    let r = repo("agy");
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\ncat > \"$0.stdin\"\n";
    let agy = fake(&root(), "agy", &format!("#!/bin/sh\n{RECORD}\
        echo '{{\"event\":\"init\",\"conversation_id\":\"c-1\",\"init\":{{\"cwd\":\"x\"}}}}'\n\
        if [ -e \"$0.fail\" ]; then rm \"$0.fail\"; echo '{{\"event\":\"result\",\"result\":{{\"status\":\"ERROR\",\"response\":\"\",\"error\":\"invalid model selection\",\"usage\":{{}}}}}}'; exit 0; fi\n\
        case \" $* \" in *' --conversation '*) k=$(($(cat \"$0.k\") + 1)); echo $k > \"$0.k\" ;; *) k=1; [ -e \"$0.k\" ] || echo 1 > \"$0.k\" ;; esac\n\
        echo \"{{\\\"event\\\":\\\"result\\\",\\\"result\\\":{{\\\"status\\\":\\\"SUCCESS\\\",\\\"response\\\":\\\"done\\\",\\\"usage\\\":{{\\\"input_tokens\\\":$((7*k)),\\\"output_tokens\\\":$((2*k))}}}}}}\"\n"));
    let mut s = session(&r, agent("agy"), &agy);
    let turn = |s: &mut Session, write: bool| -> Vec<String> {
        let id = s.launch_agent(agent("agy"), write, r#"-x the "task""#).unwrap();
        assert_eq!(wait(s, id), Status::Done);
        assert_eq!(s.job(id).unwrap().tokens.total(), 9);
        assert_eq!(fs::read_to_string(format!("{}.stdin", agy.display())).unwrap(),
                   "{\"event\":\"user\",\"message\":{\"content\":\"-x the \\\"task\\\"\"}}\n");
        fs::read_to_string(format!("{}.argv", agy.display())).unwrap().lines().map(str::to_string).collect()
    };
    let read = r#"{"permissions":{"deny":["write_file(*)","command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]}}"#;
    let head = ["--input-format", "stream-json", "--output-format", "stream-json", "--settings"];
    let write = r#"{"permissions":{"deny":["command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]}}"#;
    assert_eq!(turn(&mut s, false), [&head[..], &[write, "--mode", "accept-edits"]].concat(), "ask mode edits here");
    assert_eq!(turn(&mut s, false), [&head[..], &[write, "--mode", "accept-edits", "--conversation", "c-1"]].concat());
    assert_eq!(turn(&mut s, true), [&head[..], &[write, "--mode", "accept-edits"]].concat());
    s.mode = Mode::Read;
    assert_eq!(turn(&mut s, false), [&head[..], &[read, "--conversation", "c-1"]].concat(), "read mode cannot write files");
    s.mode = Mode::Ask;
    // A failed resumed turn reports no usage; the running total it leaves is still the last one reported.
    fs::write(format!("{}.fail", agy.display()), "").unwrap();
    let failed = s.launch_agent(agent("agy"), false, "again").unwrap();
    // the reason is in `error`, the response empty: it is what the owner and the other agents hear
    assert!(matches!(wait(&mut s, failed), Status::Failed(why) if why == "agy ended with ERROR: invalid model selection"));
    assert_eq!(s.job(failed).unwrap().tokens.total(), 0);
    let next = s.launch_agent(agent("agy"), false, "and now?").unwrap();
    assert_eq!(wait(&mut s, next), Status::Done);
    assert_eq!(s.job(next).unwrap().tokens.total(), 9, "36 so far, 27 at the last report");
}

/// An agent the owner adds: kept in agents.json beside the set-up list, run with its own command line (the prompt
/// on stdin when the line has no {prompt}), write turns only.
#[test]
fn an_agent_the_owner_adds() {
    let state = root().join("custom-state");
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("agents.json"), r#"{"agents":["codex"],"custom":[{"name":"bad name!"}]}"#).unwrap();
    assert_eq!(ringo_cli::load_custom_agents(&state).len(), 1, "an entry that cannot run is reported, not loaded");
    assert!(ringo_cli::add_custom_agent(&state, serde_json::json!({"name": "codex"})).is_err(), "a built-in name is taken");
    assert!(ringo_cli::add_custom_agent(&state, serde_json::json!({"name": "toolr", "read": true})).is_err(),
            "a plain agent cannot claim a read-only mode");
    let added = ringo_cli::add_custom_agent(&state, serde_json::json!({"name": "mytool", "args": ["run", "--in", "{cwd}"]})).unwrap();
    assert_eq!(Agent::from_name("mytool"), Some(added));
    let file: serde_json::Value = serde_json::from_str(&fs::read_to_string(state.join("agents.json")).unwrap()).unwrap();
    assert_eq!(file["agents"], serde_json::json!(["codex"]), "the set-up list is kept");
    assert_eq!(file["custom"][1]["name"], "mytool");
    assert!(ringo_cli::load_custom_agents(&state).len() == 1, "loading again changes nothing but the bad entry");

    let r = repo("custom");
    let tool = fake(&root(), "mytool", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n\
        cat > \"$0.stdin\"\necho done\n");
    let mut s = session(&r, added, &tool);
    assert!(s.launch_agent(added, false, "x").unwrap_err().contains("only in its own copy"));
    let id = s.launch_agent(added, true, "fix {cwd} please").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    let job = s.job(id).unwrap();
    assert!(job.unchanged, "a write job that changed nothing says so");
    let wt = job.worktree.clone().expect("and keeps its worktree, for /drop: an ignored file is not in the diff");
    assert_eq!(s.diff(id), Ok(String::new()));
    let argv = fs::read_to_string(format!("{}.argv", tool.display())).unwrap();
    assert_eq!(argv.lines().collect::<Vec<_>>(), ["run", "--in", &*wt.to_string_lossy()]);
    assert_eq!(fs::read_to_string(format!("{}.stdin", tool.display())).unwrap(), "fix {cwd} please");
}

/// Claude's permission requests, with Claude Code 2.1.316's shape (`control_request` / `can_use_tool` with
/// suggested rules): two requests with the same rule, then one Ringo does not serve, then the result.
const CLAUDE_ASKS: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
read -r line
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s-p","apiKeySource":"none"}'
printf '%s\n' '{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"gh pr create --fill\nrm -rf build"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"gh pr create:*"}],"behavior":"allow","destination":"localSettings"}]}}'
read -r a; printf '%s\n' "$a" > "$0.answers"
printf '%s\n' '{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"gh pr create --draft"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"gh pr create:*"}],"behavior":"allow"}]}}'
read -r a; printf '%s\n' "$a" >> "$0.answers"
printf '%s\n' '{"type":"control_request","request_id":"r3","request":{"subtype":"hook_callback","callback_id":"h"}}'
read -r a; printf '%s\n' "$a" >> "$0.answers"
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"opened"}'
if read -r a; then echo "stdin was left open" >> "$0.answers"; fi
"#;

fn wait_for_ask(s: &mut Session) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        s.pump();
        if let Some((id, _)) = s.pending() {
            return id;
        }
        assert!(Instant::now() < deadline, "no permission request arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn answers(program: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(format!("{}.answers", program.display())).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not an answer: {l}"))).collect()
}

#[test]
fn ask_mode_sends_each_permission_request_to_the_owner() {
    let r = repo("permissions");
    let claude = fake(&root(), "claude-asks", CLAUDE_ASKS);
    let mut s = session(&r, agent("claude"), &claude);
    assert_eq!(s.mode, Mode::Ask);
    let id = s.launch_agent(agent("claude"), true, "open a PR with the fix").unwrap();
    assert_eq!(wait_for_ask(&mut s), id);
    let (_, p) = s.pending().unwrap();
    assert_eq!(p.what, "Bash gh pr create --fill\nrm -rf build", "every line of the command is shown");
    assert_eq!(p.rule, "Bash(gh pr create:*)");
    assert_eq!(s.job(id).unwrap().activity, "waiting for you: Bash gh pr create --fill");
    let log: Vec<&String> = s.job(id).unwrap().log.iter().collect();
    assert!(log.windows(2).any(|w| w[0] == "asks before it acts: Bash gh pr create --fill" && w[1] == "rm -rf build"),
            "the whole request is in the job's log: {log:?}");
    assert!(s.decide(id, Decision::Session).unwrap().starts_with("you allowed for this session"));
    assert_eq!(wait(&mut s, id), Status::Done);
    let a = answers(&claude);
    assert_eq!(a.len(), 3, "the result closed stdin: {a:?}");
    assert_eq!((a[0]["response"]["request_id"].as_str(), a[0]["response"]["response"]["behavior"].as_str()),
               (Some("r1"), Some("allow")));
    assert_eq!(a[0]["response"]["response"]["updatedInput"]["command"], "gh pr create --fill\nrm -rf build");
    assert_eq!((a[1]["response"]["request_id"].as_str(), a[1]["response"]["response"]["behavior"].as_str()),
               (Some("r2"), Some("allow")), "the same rule, allowed for the session, is not asked again");
    assert_eq!((a[2]["response"]["request_id"].as_str(), a[2]["response"]["subtype"].as_str()), (Some("r3"), Some("error")));
    let log = fs::read_to_string(s.dir.join("actions.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
    assert!(log.lines().nth(1).unwrap().contains("\"answer\":\"allowed (this session)\""));
    // A later turn starts with the rule allowed, so its same requests never reach the owner.
    let next = s.launch_agent(agent("claude"), true, "and another").unwrap();
    assert_eq!(wait(&mut s, next), Status::Done);
    let argv = fs::read_to_string(format!("{}.argv", claude.display())).unwrap();
    assert!(argv.contains("--allowedTools\nBash(gh pr create:*)\n"), "{argv}");
    // Declined, the same rule is asked again; nothing is allowed for the session.
    let mut s = session(&r, agent("claude"), &claude);
    let id = s.launch_agent(agent("claude"), true, "open a PR").unwrap();
    wait_for_ask(&mut s);
    assert!(s.decide(id, Decision::Deny).unwrap().starts_with("you declined"));
    wait_for_ask(&mut s);
    s.decide(id, Decision::Once).unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    let a = answers(&claude);
    assert_eq!(a[0]["response"]["response"]["behavior"], "deny");
    assert_eq!(a[1]["response"]["response"]["behavior"], "allow");
    assert!(s.allowed.is_empty() && s.decide(id, Decision::Once).is_err());
}

/// A follow-up to a turn that called a connector keeps the connectors although it names none ("and the overdue
/// ones?" after a ClickUp question); the turn after one that did not goes back to running without them.
#[test]
fn a_follow_up_keeps_the_connectors_the_turn_before_used() {
    let r = repo("connector-follow-up");
    let claude = fake(&root(), "claude-follow-up", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n\
        line=$(head -n 1)\nprintf '%s' \"$line\" > \"$0.stdin\"\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-f\",\"apiKeySource\":\"none\"}'\n\
        case \"$line\" in *ClickUp*) echo '{\"type\":\"assistant\",\"message\":{\"id\":\"a1\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"mcp__claude_ai_ClickUp__clickup_filter_tasks\",\"input\":{}}]},\"parent_tool_use_id\":null}';; esac\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n");
    let mut s = session(&r, agent("claude"), &claude);
    let turn = |s: &mut Session, write: bool, task: &str| -> bool {
        let id = s.launch_agent(agent("claude"), write, task).unwrap();
        assert_eq!(wait(s, id), Status::Done);
        let narrow = fs::read_to_string(format!("{}.stdin", claude.display())).unwrap().contains("(Ringo: this reaches a connector");
        let safe = fs::read_to_string(format!("{}.argv", claude.display())).unwrap().lines().any(|a| a == "--safe-mode");
        assert_eq!(narrow, !safe, "a turn with the connectors is asked to keep them narrow, and only such a turn");
        safe
    };
    let safe = |s: &mut Session, task: &str| turn(s, false, task);
    assert!(!safe(&mut s, "what ClickUp tasks are mine?"), "it names a connector");
    // after a quit or a crash, `ringo --resume` goes on with them too
    let kept = s.keep(None).unwrap();
    let mut next = session(&r, agent("claude"), &claude);
    next.go_on(&kept).unwrap();
    assert!(!safe(&mut next, "and the overdue ones?"), "the turn before the resume called one");
    let mut cleared = session(&r, agent("claude"), &claude);
    cleared.go_on(&kept).unwrap();
    cleared.forget();
    assert!(safe(&mut cleared, "and the overdue ones?"), "/clear: a new conversation, with no turn before");
    assert!(turn(&mut s, true, "fix the parser") && turn(&mut s, true, "fix the parser"),
            "an @claude! turn starts fresh in its own copy: it continues no conversation");
    assert!(!safe(&mut s, "and the overdue ones?"), "the conversation's turn before called one");
    assert!(safe(&mut s, "now fix the parser"), "the turn before had them but called none");
    assert!(safe(&mut s, "and its tests"));
}

#[test]
fn modes_change_what_each_agent_may_do() {
    let r = repo("modes");
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\nenv > \"$0.env\"\n";
    let claude = fake(&root(), "claude-modes", &format!("#!/bin/sh\n{RECORD}head -n 1 >/dev/null\n\
        echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-m\",\"apiKeySource\":\"none\"}}'\n\
        echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
    let codex = fake(&root(), "codex-modes", &codex_script("", &codex_done("done", (1, 0, 1))));
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    let read = |p: &Path, ext: &str| fs::read_to_string(format!("{}.{ext}", p.display())).unwrap();
    let has = |argv: &str, run: &[&str]| argv.lines().collect::<Vec<_>>().windows(run.len()).any(|w| w == run);
    let turn = |s: &mut Session, a: &str, write: bool, task: &str| {
        let id = s.launch_agent(agent(a), write, task).unwrap();
        assert_eq!(wait(s, id), Status::Done);
    };

    s.mode = Mode::Auto;
    turn(&mut s, "claude", true, "fix the retry loop");
    let argv = read(&claude, "argv");
    assert!(has(&argv, &["--safe-mode", "--permission-prompt-tool", "stdio", "--permission-mode", "auto"]), "{argv}");
    let denied: Vec<&str> = argv.lines().skip_while(|a| *a != "--disallowed-tools").skip(1)
        .take_while(|a| !a.starts_with("--")).collect();
    assert!(!denied.contains(&"Bash") && !denied.contains(&"Edit") && denied.contains(&"AskUserQuestion")
            && denied.contains(&"Workflow") && argv.contains("--settings"),
            "a write turn edits and runs commands; only tools Ringo does not serve or that act outside the turn are \
             denied, and the always-ask rules ride along: {argv}");
    turn(&mut s, "codex", true, "fix the retry loop");
    let argv = read(&codex, "argv");
    assert!(has(&argv, &["app-server", "-c", "sandbox_workspace_write.network_access=true"]), "{argv}");
    let thread = threads(&codex).pop().unwrap();
    assert_eq!((thread["params"]["sandbox"].as_str(), thread["params"]["approvalPolicy"].as_str(),
                thread["params"]["approvalsReviewer"].as_str()), (Some("workspace-write"), Some("on-request"), Some("auto_review")),
               "auto: Codex's own reviewer answers its approval requests, as --approve-for-me did");
    assert!(argv.contains("features.plugins=false"), "no connector named: no plugins: {argv}");
    turn(&mut s, "codex", true, "summarise my unread Gmail");
    let argv = read(&codex, "argv");
    assert!(!argv.contains("features.plugins=false") && argv.contains("analytics.enabled=false"), "{argv}");

    // A task naming a connector gets the owner's connectors, without their CLAUDE.md, memory, skills or main agent.
    turn(&mut s, "claude", false, "what is in my Jira sprint?");
    let (argv, env) = (read(&claude, "argv"), read(&claude, "env"));
    assert!(has(&argv, &["--agent", "claude", "--disable-slash-commands"]) && !argv.contains("--safe-mode"), "{argv}");
    assert!(!has(&argv, &["--disallowed-tools", "Bash"]) && argv.contains("--disallowed-tools"),
            "a turn in the owner's folder edits and runs commands as its mode allows: {argv}");
    for var in ["CLAUDE_CODE_DISABLE_CLAUDE_MDS=1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY=1", "RINGO_CHILD=1"] {
        assert!(env.lines().any(|l| l == var), "{var}");
    }
    assert!(ringo_cli::names_connector("check the ClickUp board") && ringo_cli::names_connector("open my Google Drive"));
    assert!(!ringo_cli::names_connector("fix the drive train module") && !ringo_cli::names_connector("refactor the email_parser"),
            "words inside identifiers and other senses do not count");

    s.mode = Mode::Read;
    assert!(s.launch_agent(agent("claude"), true, "fix it").unwrap_err().contains("read mode"));
    turn(&mut s, "claude", false, "what is in my Jira sprint?");
    let argv = read(&claude, "argv");
    assert!(has(&argv, &["--safe-mode", "--permission-prompts", "none", "--permission-mode", "default"]), "{argv}");
    assert!(has(&argv, &["--disallowed-tools", "Bash", "Edit", "Write", "NotebookEdit"]) && !argv.contains("--settings"));
    assert!(!argv.contains("AskUserQuestion"), "read mode has no permission host, so Claude adds no host tools");
    assert_eq!((Mode::Ask.next(), Mode::Auto.next(), Mode::Read.next()), (Mode::Auto, Mode::Read, Mode::Ask));
}

#[test]
fn a_task_typed_as_a_plain_line_comes_back_routed() {
    // The ask call answers a task with one `ROUTE:` line; the shell starts it, and later asks do not carry it.
    let r = repo("routed");
    let home = root().join("fake-route-home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("ringo.py"), "import json, sys, pathlib\n\
        a = sys.argv[1:]\n\
        val = lambda k: a[a.index(k) + 1]\n\
        material = pathlib.Path(val('--input')).read_text()\n\
        n = len(list(pathlib.Path(sys.argv[0]).parent.glob('seen-*.json'))) + 1\n\
        pathlib.Path(sys.argv[0]).with_name(f'seen-{n}.json').write_text(json.dumps({'material': material}))\n\
        t = pathlib.Path(val('--trace')); t.mkdir(parents=True, exist_ok=True)\n\
        (t / 'trace.jsonl').write_text(json.dumps({'step': 'answer', 'status': 'ok', 'usage': {'input': 900, 'output': 5}}) + '\\n')\n\
        pathlib.Path(val('--out')).write_text('ROUTE: Edit.\\nIt changes client.rs.\\n' if n == 1 else 'an answer\\n')\n").unwrap();
    let mut s = Session::new(&r).unwrap();
    s.ringo_home = home.clone();
    let task = s.ask("fix the retry loop in client.rs").unwrap();
    assert_eq!(wait(&mut s, task), Status::Done);
    assert_eq!(s.routed.pop_front(), Some((task, "edit".to_string())));
    let question = s.ask("why do retries back off?").unwrap();
    assert_eq!(wait(&mut s, question), Status::Done);
    assert!(s.routed.is_empty());
    let seen: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join("seen-2.json")).unwrap()).unwrap();
    assert_eq!(seen["material"], "Question: why do retries back off?\n", "a routed line is not an earlier answer");
}

#[test]
fn always_ask_actions_reach_the_owner_every_time() {
    let r = repo("always-ask");
    let sudo = CLAUDE_ASKS.replace("gh pr create --fill\\nrm -rf build", "make && sudo -n true")
        .replace("gh pr create --draft", "make && sudo -n true").replace("gh pr create:*", "make && sudo -n true");
    let claude = fake(&root(), "claude-sudo", &sudo);
    let mut s = session(&r, agent("claude"), &claude);
    let id = s.launch_agent(agent("claude"), true, "build it").unwrap();
    wait_for_ask(&mut s);
    let (_, p) = s.pending().unwrap();
    assert!(s.always_asks(p), "`sudo` after `&&` is still sudo");
    assert_eq!(s.decide(id, Decision::Session).unwrap(), "you allowed: Bash make && sudo -n true", "allowed once only");
    assert!(s.allowed.is_empty());
    wait_for_ask(&mut s); // the same request asks again
    s.decide(id, Decision::Deny).unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    let a = answers(&claude);
    assert_eq!((a[0]["response"]["response"]["behavior"].as_str(), a[1]["response"]["response"]["behavior"].as_str()),
               (Some("allow"), Some("deny")));

    let ask = |tool: &str, input: serde_json::Value| ringo_cli::parse::Permission {
        id: "r".into(), tool: tool.into(), input, what: String::new(), rule: String::new() };
    let covers = |rule: &str, p: &ringo_cli::parse::Permission| ringo_cli::rule_covers(rule, p);
    let bash = |c: &str| ask("Bash", serde_json::json!({"command": c}));
    assert!(covers("Bash(sudo:*)", &bash("sudo rm x")) && covers("Bash(sudo:*)", &bash("ls; sudo rm x")));
    assert!(covers("Bash(sudo:*)", &bash("echo $(sudo cat f)")) && covers("Bash(sudo:*)", &bash("ls\nsudo rm x")));
    assert!(!covers("Bash(sudo:*)", &bash("pseudocode.py")) && !covers("Bash(dd:*)", &bash("git add .")));
    assert!(covers("Bash(git push --force:*)", &bash("git push --force origin main")));
    assert!(!covers("Bash(sudo:*)", &ask("Edit", serde_json::json!({"file_path": "/x/sudo"}))));
    let home = std::env::var("HOME").unwrap();
    assert!(covers("Edit(~/.claude/**)", &ask("Edit", serde_json::json!({"file_path": format!("{home}/.claude/settings.json")}))));
    assert!(!covers("Edit(~/.claude/**)", &ask("Edit", serde_json::json!({"file_path": format!("{home}/.claudex/a")}))));
    assert!(covers("WebFetch", &ask("WebFetch", serde_json::json!({}))) && covers("Bash(make)", &bash("make")));
}

#[test]
fn quitting_waits_only_until_the_jobs_are_gone() {
    let r = repo("shutdown");
    let slow = fake(&root(), "codex-shutdown", "#!/bin/sh\nsleep 30\n");
    let mut s = session(&r, agent("codex"), &slow);
    s.launch_agent(agent("codex"), true, "x").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    s.shutdown();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}: TERM ends the fake at once", started.elapsed());
}

/// A Claude turn's tokens count up from its messages' usage; the turn's own count replaces them at the end, and a
/// result without one (an error result has none) keeps them.
#[test]
fn a_claude_turn_keeps_its_running_count_until_its_own() {
    let r = repo("live-tokens");
    let init = r#"{"type":"system","subtype":"init","session_id":"s-live","apiKeySource":"none"}"#;
    let message = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":90,"output_tokens":5}},"parent_tool_use_id":null}"#;
    let without = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"stopped"}"#;
    let with = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","usage":{"input_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":90,"output_tokens":7}}"#;
    for (n, (result, want)) in [(without, 105), (with, 117)].into_iter().enumerate() {
        let claude = fake(&root(), &format!("claude-live-{n}"),
                          &format!("#!/bin/sh\nhead -n 1 > /dev/null\necho '{init}'\necho '{message}'\necho '{result}'\n"));
        let mut s = session(&r, agent("claude"), &claude);
        let id = s.launch_agent(agent("claude"), false, "x").unwrap();
        wait(&mut s, id);
        assert_eq!(s.job(id).unwrap().tokens_so_far(), want, "{result}");
        assert_eq!(s.tokens(), want);
    }
}

#[test]
fn tiers_set_each_agents_effort_and_every_job_leaves_a_usage_record() {
    let r = repo("tiers");
    let state = root().join("tiers-state");
    let _ = fs::remove_dir_all(&state);
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n";
    let claude = fake(&root(), "claude-tiers", &format!("#!/bin/sh\n{RECORD}head -n 1 >/dev/null\n\
        echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-t\",\"apiKeySource\":\"none\"}}'\n\
        echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}'\n"));
    let codex = fake(&root(), "codex-tiers", &codex_script("", &codex_done("done", (10, 4, 2))));
    let agy = fake(&root(), "agy-tiers", &format!("#!/bin/sh\n{RECORD}cat >/dev/null\n\
        echo '{{\"event\":\"init\",\"conversation_id\":\"c-t\",\"init\":{{\"cwd\":\"x\"}}}}'\n\
        echo '{{\"event\":\"result\",\"result\":{{\"status\":\"SUCCESS\",\"response\":\"done\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}'\n"));
    let mut s = Session::with_state(&r, &state).unwrap();
    for (name, program) in [("claude", &claude), ("codex", &codex), ("agy", &agy)] {
        s.programs.insert(agent(name), program.clone());
    }
    let argv = |p: &Path| fs::read_to_string(format!("{}.argv", p.display())).unwrap();
    let has = |argv: &str, run: &[&str]| argv.lines().collect::<Vec<_>>().windows(run.len()).any(|w| w == run);
    let effort = |s: &mut Session, tier: ringo_cli::Tier| -> (String, String, String) {
        s.tier = tier;
        for name in ["claude", "codex", "agy"] {
            s.forget(); // a fresh conversation each time, so every turn reads alike
            let id = s.launch_agent(agent(name), false, "fix it").unwrap();
            assert_eq!(wait(s, id), Status::Done, "{name}");
        }
        (argv(&claude), argv(&codex), argv(&agy))
    };
    // standard: each agent's own default, as before tiers
    let (c, x, a) = effort(&mut s, ringo_cli::Tier::Standard);
    assert!(!c.contains("--effort") && !x.contains("model_reasoning_effort") && !a.contains("--effort"), "{c}\n{x}\n{a}");
    let (c, x, a) = effort(&mut s, ringo_cli::Tier::Quick);
    assert!(has(&c, &["--effort", "low"]) && has(&x, &["-c", "model_reasoning_effort=\"low\""]) && has(&a, &["--effort", "low"]),
            "{c}\n{x}\n{a}");
    for tier in [ringo_cli::Tier::Deep, ringo_cli::Tier::Ultra] {
        let (c, x, a) = effort(&mut s, tier);
        // agy's default model has no xhigh (agy 1.4.12): it gets its highest, high
        assert!(has(&c, &["--effort", "xhigh"]) && has(&x, &["-c", "model_reasoning_effort=\"xhigh\""]) && has(&a, &["--effort", "high"]),
                "{c}\n{x}\n{a}");
    }
    // every job that used tokens is in the record /usage totals. Claude: 7 a turn. Codex and agy count what a turn
    // added to its thread's total, and these fakes answer with one thread and one total every time, so only their
    // first turn adds any (12 and 2), and the jobs that added none leave no line.
    let (day, first) = s.used_since(86_400);
    let total = |name: &str| day.get(name).map(|t| t.total()).unwrap_or(0);
    assert_eq!((total("claude"), total("codex"), total("agy")), (28, 12, 2));
    assert!(first.is_some());
    assert_eq!(fs::read_to_string(state.join("usage.jsonl")).unwrap().lines().count(), 6);
}

#[test]
fn a_turn_stopped_by_quitting_is_still_in_the_usage_record() {
    let r = repo("quit-usage");
    let state = root().join("quit-usage-state");
    let _ = fs::remove_dir_all(&state);
    // a Claude turn that reports its first message's usage, then works on
    let claude = fake(&root(), "claude-quit", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-q\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":6}},\"parent_tool_use_id\":null}'\n\
        sleep 30\n");
    let mut s = Session::with_state(&r, &state).unwrap();
    s.programs.insert(agent("claude"), claude);
    let id = s.launch_agent(agent("claude"), false, "work").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.job(id).unwrap().live.total() == 0 {
        assert!(Instant::now() < deadline);
        s.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    s.shutdown();
    assert_eq!(s.job(id).unwrap().status, Status::Cancelled);
    assert_eq!(s.used_since(86_400).0.get("claude").map(|t| t.total()), Some(11));
}

/// A fake claude: records its arguments and its message, says "done", and reports a 70k window when the message says
/// ends with GROW, 300 with HUGE (10 tokens otherwise; a record of an earlier one does not count); with BASH it runs a
/// command; with WRITE-O it writes o.txt and says so as its Write tool would.
const CLAUDE_TALKS: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
head -n 1 > "$0.stdin"
echo '{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}'
case "$(cat "$0.stdin")" in *WRITE-O*) echo o > o.txt
  echo '{"type":"assistant","message":{"id":"w1","content":[{"type":"tool_use","id":"tu1","name":"Write","input":{"file_path":"'"$PWD"'/o.txt","content":"o"}}]},"parent_tool_use_id":null}' ;; esac
case "$(cat "$0.stdin")" in *'BASH"}}') echo '{"type":"assistant","message":{"id":"b1","content":[{"type":"tool_use","id":"tu2","name":"Bash","input":{"command":"true"}}]},"parent_tool_use_id":null}' ;; esac
case "$(cat "$0.stdin")" in *'GROW"}}') big=70000 ;; *'HUGE"}}') big=300 ;; *) big=10 ;; esac
echo '{"type":"assistant","message":{"id":"m1","content":[],"usage":{"input_tokens":'$big',"output_tokens":5}},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done"}'
"#;

/// A claude that takes a second line on stdin while it works (`Session::steer`), as Claude Code 2.1.316 does: with its
/// next tool result, or, when the message arrived during its final answer (LATE), in a run of its own after its first
/// result. It writes the line it took to $0.steer and ends when its stdin closes.
const CLAUDE_STEERED: &str = r#"#!/bin/sh
read first
echo '{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}'
echo '{"type":"user","isReplay":true,"message":{"role":"user","content":"x"},"parent_tool_use_id":null}'
replay='{"type":"user","isReplay":true,"message":{"role":"user","content":"y"},"parent_tool_use_id":null}'
case "$first" in *QUIT*)
  sleep 1
  echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5}},"parent_tool_use_id":null}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":"done"}'
  exit 0 ;;
*LATE*)
  echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"first"}],"usage":{"input_tokens":10,"output_tokens":5}},"parent_tool_use_id":null}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":"first","model_usage_since_last_result":{"m":{"inputTokens":10,"outputTokens":5}}}'
  read second; printf '%s\n' "$second" > "$0.steer"; echo "$replay"
  echo '{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"second"}],"usage":{"input_tokens":20,"output_tokens":7}},"parent_tool_use_id":null}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":"second","model_usage_since_last_result":{"m":{"inputTokens":20,"outputTokens":7}}}' ;;
*)
  echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 1"}}],"usage":{"input_tokens":10,"output_tokens":5}},"parent_tool_use_id":null}'
  read second; printf '%s\n' "$second" > "$0.steer"
  echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"","is_error":false}]},"parent_tool_use_id":null}'
  echo "$replay"
  echo '{"type":"result","subtype":"success","is_error":false,"result":"both","model_usage_since_last_result":{"m":{"inputTokens":30,"outputTokens":12}}}' ;;
esac
cat > /dev/null
"#;

#[test]
fn a_line_sent_while_claude_works_goes_into_its_turn() {
    let r = repo("steer");
    let claude = fake(&root(), "claude-steer", CLAUDE_STEERED);
    let mut s = session(&r, agent("claude"), &claude);
    for (task, reply) in [("fix it", "both"), ("fix it LATE", "second")] {
        let id = s.launch_agent(agent("claude"), false, task).unwrap();
        assert!(!s.steer(id, "also check my clickup tasks"), "a turn started without the connectors cannot use them");
        assert!(s.steer(id, "and the docs"));
        // stdin closes only once the line is taken in and answered: the fake ends when it does
        assert_eq!(wait(&mut s, id), Status::Done, "{task}");
        let j = s.job(id).unwrap();
        assert_eq!(j.reply().trim(), reply, "{task}: the last answer, which took the line in");
        assert_eq!((j.tokens.input, j.tokens.output), (30, 12), "{task}: every run counted once");
        let heard: serde_json::Value = serde_json::from_str(&fs::read_to_string(format!("{}.steer", claude.display())).unwrap()).unwrap();
        assert_eq!(heard["message"]["content"], "and the docs");
        assert!(j.title_full.ends_with("fix it\n\nthen, while it worked: and the docs") || j.title_full.ends_with("LATE\n\nthen, while it worked: and the docs"),
                "the owner's words, for the record: {}", j.title_full);
        assert!(!s.steer(id, "too late"), "an ended turn takes no more: the line waits for the next");
    }
    assert!(s.returned.is_empty());
    // a turn that ends without taking the line in gives it back, for the next turn, and it is not in this one's record
    let id = s.launch_agent(agent("claude"), false, "fix it QUIT").unwrap();
    assert!(s.steer(id, "and the tests"));
    assert_eq!(wait(&mut s, id), Status::Done);
    assert_eq!(s.returned, [(id, agent("claude"), "and the tests".to_string())]);
    assert_eq!(s.job(id).unwrap().title_full, "fix it QUIT");
}

/// What the fake claude was last sent, and whether it resumed a conversation.
fn claude_heard(claude: &Path) -> (String, bool) {
    let line: serde_json::Value = serde_json::from_str(&fs::read_to_string(format!("{}.stdin", claude.display())).unwrap()).unwrap();
    let argv = fs::read_to_string(format!("{}.argv", claude.display())).unwrap();
    (line["message"]["content"].as_str().unwrap().to_string(), argv.lines().any(|a| a == "--resume"))
}

fn said(s: &mut Session, who: &str, text: &str) -> usize {
    let id = s.launch_agent(agent(who), false, text).unwrap();
    assert_eq!(wait(s, id), Status::Done);
    id
}

#[test]
fn a_restart_drops_nothing_from_the_shared_conversation() {
    let r = repo("restart");
    let (codex, claude) = (fake(&root(), "codex-restart", &codex_ok()), fake(&root(), "claude-restart", CLAUDE_TALKS));
    let open = || {
        let mut s = session(&r, agent("claude"), &claude);
        s.programs.insert(agent("codex"), codex.clone());
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let mut s = open();
    said(&mut s, "claude", "fix it");
    said(&mut s, "codex", "take it from here");
    // /restart: the next shell gets each conversation and what each agent had yet to hear, as this one would tell it
    let (threads, told, records) = (s.threads(), s.carry(), s.carry_record());
    let mut next = open();
    next.resume(&threads, &told, &records);
    said(&mut next, "claude", "and now");
    let (heard, resumed) = claude_heard(&claude);
    assert!(resumed && heard.starts_with("Before Ringo restarted, other turns were taken"), "{heard}");
    assert!(heard.contains("#2 · the owner to codex: take it from here\ncodex: all done\n") && !heard.contains("#1 ·"),
            "only what it had not heard: {heard}");
    assert!(heard.ends_with("The owner's new message:\nand now"), "{heard}");
    said(&mut next, "claude", "again");
    assert_eq!(claude_heard(&claude), ("again".into(), true), "told once");
}

#[test]
fn a_long_conversation_starts_fresh_after_a_restart_and_keeps_the_turns_before_it() {
    let r = repo("restart-long");
    let (codex, claude) = (fake(&root(), "codex-restart-long", &codex_ok()), fake(&root(), "claude-restart-long", CLAUDE_TALKS));
    let open = || {
        let mut s = session(&r, agent("claude"), &claude);
        s.programs.insert(agent("codex"), codex.clone());
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let mut s = open();
    let long = format!("fix it {}", "x".repeat(5000)); // longer than the record keeps inline
    said(&mut s, "claude", &long);
    said(&mut s, "claude", "GROW"); // its window passes 64k
    said(&mut s, "codex", "take it from here"); // which claude has yet to hear
    let (threads, told, records) = (s.threads(), s.carry(), s.carry_record());
    let mut next = open();
    next.resume(&threads, &told, &records);
    // The first turn after the restart starts fresh, as it would have without one: the 70k conversation is not
    // re-read at every call.
    let id = said(&mut next, "claude", "then");
    let (heard, resumed) = claude_heard(&claude);
    assert!(!resumed && heard.starts_with("Ringo's record of this conversation so far, which you start fresh from"), "{heard}");
    assert!(["#1 · the owner to claude: fix it", "#2 · the owner to claude: GROW", "#3 · the owner to codex: take it from here"]
        .iter().all(|t| heard.contains(t)), "{heard}");
    assert!(!heard.contains("Before Ringo restarted") && heard.matches("#3 · the owner to codex").count() == 1,
            "the turn it had not heard is told once, in the record: {heard}");
    assert!(heard.ends_with("The owner's new message:\nthen"), "{heard}");
    let log = &next.job(id).unwrap().log;
    assert!(log.iter().any(|l| l.starts_with("handoff: claude starts fresh from Ringo's record of the turns before the restart")),
            "{log:?}");
    // the record inline is cut; the one it names is whole, and this shell's whole record names it too
    assert!(!heard.contains(&long) && heard.contains("(cut; the whole is in the record)"), "{heard}");
    let uncut = heard.split("(these turns uncut: ").nth(1).and_then(|r| r.split(')').next()).expect(&heard);
    assert!(fs::read_to_string(uncut).unwrap().contains(&long), "{uncut}");
    let whole = fs::read_to_string(next.job(id).unwrap().dir.join("record.md")).unwrap();
    assert!(whole.contains(&format!("(these turns uncut: {uncut})")), "{whole}");
    said(&mut next, "claude", "after");
    assert_eq!(claude_heard(&claude), ("after".into(), true), "the new conversation goes on");
    // A later fresh start still holds the turns before the restart, then the ones since.
    said(&mut next, "claude", "GROW");
    said(&mut next, "claude", "later");
    let (heard, resumed) = claude_heard(&claude);
    let since = heard.find("(Ringo restarted here; the turns since are numbered from #1 again)").expect(&heard);
    assert!(!resumed && heard[..since].contains("#3 · the owner to codex: take it from here"), "{heard}");
    assert!(heard[since..].contains("#1 · the owner to claude: then") && heard[since..].contains("#3 · the owner to claude: GROW"),
            "{heard}");
    // /clear: a new conversation hears nothing from before the restart either
    next.forget();
    said(&mut next, "codex", "new");
    assert_eq!(turn_text(&codex), "new");
}

#[test]
fn a_fresh_start_after_an_older_shells_restart_still_hears_the_turns_before_it() {
    let r = repo("restart-older");
    let (codex, claude) = (fake(&root(), "codex-restart-older", &codex_ok()), fake(&root(), "claude-restart-older", CLAUDE_TALKS));
    let open = || {
        let mut s = session(&r, agent("claude"), &claude);
        s.programs.insert(agent("codex"), codex.clone());
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let mut s = open();
    said(&mut s, "claude", "fix it"); // codex has no conversation yet
    // a shell from before records were handed on: conversations and what each agent had yet to hear, no records
    let (threads, told) = (s.threads(), s.carry());
    let mut next = open();
    next.resume(&threads, &told, &[]);
    said(&mut next, "claude", "and now");
    said(&mut next, "codex", "your turn");
    let heard = turn_text(&codex);
    let since = heard.find("(Ringo restarted here; the turns since are numbered from #1 again)").expect(&heard);
    assert!(heard.starts_with("Ringo's record of this conversation so far") && heard[..since].contains("the owner to claude: fix it"),
            "{heard}");
    assert!(heard[since..].contains("#1 · the owner to claude: and now") && !heard.contains("Before Ringo restarted"), "{heard}");
    // /clear right after such a restart: the new conversation hears none of what codex had yet to be told
    let mut cleared = open();
    cleared.resume(&threads, &told, &[]);
    cleared.forget();
    said(&mut cleared, "codex", "new");
    assert_eq!(turn_text(&codex), "new");
}

#[test]
fn a_conversation_kept_after_each_turn_goes_on_after_a_crash() {
    let r = repo("crash");
    let (codex, claude) = (fake(&root(), "codex-crash", &codex_ok()), fake(&root(), "claude-crash", CLAUDE_TALKS));
    let open = || {
        let mut s = session(&r, agent("claude"), &claude);
        s.programs.insert(agent("codex"), codex.clone());
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let state = root().join("state");
    let mut s = open();
    said(&mut s, "claude", "fix it");
    let path = s.keep(None).unwrap(); // the shell keeps it after every turn
    said(&mut s, "codex", "take it from here");
    assert_eq!(s.keep(None).unwrap(), path, "one file per shell, replaced whole");
    let (dir, folder) = (s.dir.clone(), s.repo.clone());
    drop(s); // a crash: nothing more is written
    // `ringo --resume` lists it for this folder alone, by the owner's latest message
    let listed = kept(&state, &folder);
    let ours: Vec<&Kept> = listed.iter().filter(|k| k.path == path).collect();
    assert_eq!(ours.len(), 1, "{listed:?}");
    assert_eq!(ours[0].about, "take it from here");
    assert!(!kept(&state, &root().join("elsewhere")).iter().any(|k| k.path == path));
    let mut next = open();
    assert!(next.go_on(&path).is_some());
    said(&mut next, "claude", "and now");
    let (heard, resumed) = claude_heard(&claude);
    assert!(resumed && heard.contains("#2 · the owner to codex: take it from here") && !heard.contains("#1 ·"), "{heard}");
    // once the new shell keeps its own, the crashed one is no longer listed: one conversation, one entry
    let own = next.keep(None).unwrap();
    let listed = kept(&state, &folder);
    assert!(listed.iter().any(|k| k.path == own && k.about == "and now") && !listed.iter().any(|k| k.path == path), "{listed:?}");
    assert!(dir.join("resumed.json").exists());
    // newest first, so Enter takes the latest: an older conversation of the same folder comes after
    let older = state.join("shell").join("0-older");
    fs::create_dir_all(&older).unwrap();
    fs::write(older.join("resume.json"), serde_json::json!({"folder": folder, "saved": 1, "about": "older"}).to_string()).unwrap();
    let mine: Vec<String> = kept(&state, &folder).into_iter().map(|k| k.about).filter(|a| a == "older" || a == "and now").collect();
    assert_eq!(mine, ["and now", "older"]);
    fs::remove_dir_all(&older).unwrap();
    // a shell that goes on and keeps before a turn of its own (a /restart right away) is still listed by the latest
    let mut again = open();
    again.go_on(&own).unwrap();
    let theirs = again.keep(None).unwrap();
    assert!(kept(&state, &folder).iter().any(|k| k.path == theirs && k.about == "and now"));
    // /separate goes on as /separate: neither agent is then told what the other does
    let mut alone = open();
    alone.separate = true;
    said(&mut alone, "claude", "alone");
    let mut after = open();
    after.go_on(&alone.keep(None).unwrap()).unwrap();
    assert!(after.separate && !again.separate);
}

#[test]
fn agents_share_one_conversation_and_a_long_one_starts_fresh_from_ringos_record() {
    let r = repo("handoff");
    let (codex, claude) = (fake(&root(), "codex-handoff", &codex_ok()), fake(&root(), "claude-handoff", CLAUDE_TALKS));
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    said(&mut s, "claude", "fix it");
    assert_eq!(claude_heard(&claude), ("fix it".into(), false), "nothing to tell yet");
    // Codex's first turn hears the conversation so far, from Ringo's own record: no model call writes it.
    said(&mut s, "codex", "take it from here");
    let told = turn_text(&codex);
    assert!(told.starts_with("Ringo's record of this conversation so far, which you start fresh from"), "{told}");
    assert!(told.contains("#1 · the owner to claude: fix it\nclaude: done\n"), "{told}");
    assert!(told.ends_with("The owner's new message:\ntake it from here"), "{told}");
    // Claude goes on with its own conversation and hears only what happened since its last turn: the words, the
    // reply, the files and the change itself.
    said(&mut s, "claude", "and now");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.starts_with("While you were away, other agents took turns"), "{told}");
    assert!(told.contains("#2 · the owner to codex: take it from here\ncodex: all done\n") && !told.contains("#1 ·"), "{told}");
    assert!(told.contains("changed: b.txt, c.bin, made.txt") && told.contains("+made"), "{told}");
    let id = said(&mut s, "claude", "GROW");
    assert_eq!(claude_heard(&claude), ("GROW".into(), true), "nothing new since its own turn");
    // Its window passed 64k: the next turn starts fresh from the record of the whole conversation.
    let id2 = said(&mut s, "claude", "then");
    let (told, resumed) = claude_heard(&claude);
    assert!(!resumed && told.starts_with("Ringo's record of this conversation so far, which you start fresh from"), "{told}");
    assert!(["#1 · ", "#2 · ", "#3 · ", &format!("#{id} · the owner to claude: GROW")].iter().all(|t| told.contains(t)), "{told}");
    assert!(told.ends_with("The owner's new message:\nthen"));
    assert!(s.job(id2).unwrap().log.iter().any(|l| l.starts_with("handoff: claude starts fresh")));
    let whole = fs::read_to_string(s.job(id2).unwrap().dir.join("record.md")).unwrap();
    assert!(whole.contains("#2 · the owner to codex: take it from here") && whole.contains("+made"), "the record uncut");
    said(&mut s, "claude", "after");
    assert_eq!(claude_heard(&claude), ("after".into(), true), "the new conversation goes on as any other");
    // Past the threshold, but the record would not be half the window: claude resumes, still told what codex did.
    said(&mut s, "claude", "HUGE");
    s.handoff_at = 200; // past it, but the whole record is more than half of 305 tokens
    said(&mut s, "codex", "meanwhile");
    let id = said(&mut s, "claude", "and so");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.starts_with("While you were away") && told.contains("the owner to codex: meanwhile"), "{told}");
    assert!(s.job(id).unwrap().log.iter().any(|l| l.starts_with("told claude what #")));
    // /clear: a new conversation, which hears nothing of the old one.
    s.forget();
    said(&mut s, "codex", "new");
    assert_eq!(turn_text(&codex), "new");
}

#[test]
fn together_agents_work_side_by_side_and_each_hears_every_turn_it_missed() {
    // a slow turn edits again after its wait, once the quick one beside it has ended
    const LATE: &str = r#"case "$turn" in *SLOW*) sleep 1; echo late > late.txt
  echo "{\"method\":\"item/started\",\"params\":{\"threadId\":\"t-1\",\"item\":{\"type\":\"fileChange\",\"changes\":[{\"path\":\"$PWD/late.txt\"}]}}}" ;; esac"#;
    let r = repo("side");
    let codex = fake(&root(), "codex-side", &codex_script(&format!("{CODEX_EDITS}\n{LATE}"), &codex_done("all done", (10, 4, 2))));
    let claude = fake(&root(), "claude-side", CLAUDE_TALKS);
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    let slow = s.launch_agent(agent("codex"), false, "SLOW").unwrap();
    assert!(s.launch_agent(agent("codex"), false, "again").unwrap_err().contains(&format!("#{slow}")), "each agent one turn at a time");
    let deadline = Instant::now() + Duration::from_secs(20);
    while s.job(slow).unwrap().touched.is_empty() {
        assert!(Instant::now() < deadline, "codex said no edit");
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    // Claude starts while Codex works, in the same conversation: told what is still at work, and ends first.
    let quick = s.launch_agent(agent("claude"), false, "WRITE-O").unwrap();
    assert_eq!(wait(&mut s, quick), Status::Done);
    assert!(s.job(slow).unwrap().running(), "codex still works");
    let (told, resumed) = claude_heard(&claude);
    assert!(!resumed && told.starts_with("Still at work in this folder as you start"), "{told}");
    assert!(told.contains(&format!("#{slow} codex, asked: \"SLOW\"; edited so far: made.txt\n")), "{told}");
    assert!(told.ends_with("The owner's new message:\nWRITE-O"), "{told}");
    assert!(s.job(quick).unwrap().log.iter().any(|l| l == "told claude which turns are still at work here"));
    assert_eq!(wait(&mut s, slow), Status::Done);
    // an agent with no conversation yet hears the whole change up to the turn that ended last, which is not the last
    // listed: Codex's late file came after Claude's quick turn ended
    let agy_owed = s.carry().into_iter().find(|(a, _)| a == "agy").unwrap().1;
    assert!(agy_owed.contains("+late") && agy_owed.contains("+o"), "{agy_owed}");
    // Codex goes on with its conversation and hears Claude's turn, which ended while it worked: its file is Claude's
    // own (Codex said it edited another and had run no command yet).
    said(&mut s, "codex", "next");
    let told = turn_text(&codex);
    assert!(told.starts_with("While you were away") && told.contains(&format!("#{quick} · the owner to claude: WRITE-O")), "{told}");
    assert!(told.contains("changed: o.txt") && !told.contains(&format!("#{slow} ·")), "{told}");
    // Claude hears both of Codex's turns: the one that ended after its own began, which a place in the lane would
    // miss, and the one since; never its own, and nothing twice.
    let now = said(&mut s, "claude", "and now");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.contains(&format!("#{slow} · the owner to codex: SLOW")) && told.contains("the owner to codex: next"), "{told}");
    assert!(!told.contains(&format!("#{quick} ·")) && !told.contains("Still at work"), "{told}");
    said(&mut s, "claude", "again");
    assert_eq!(claude_heard(&claude), ("again".into(), true), "each turn told once");
    // /restart hands on the same: claude has heard every turn; codex has yet to hear claude's last two
    let carry = s.carry();
    assert!(!carry.iter().any(|(a, _)| a == "claude"), "{carry:?}");
    let codex_owed = &carry.iter().find(|(a, _)| a == "codex").unwrap().1;
    assert!(codex_owed.starts_with(&format!("#{now} · the owner to claude: and now")) && !codex_owed.contains(&format!("#{quick} ·")),
            "{codex_owed}");
    // a turn started in /separate stays its agent's own after /together: it is not named to the others
    s.separate = true;
    let alone = s.launch_agent(agent("codex"), false, "SLOW alone").unwrap();
    s.separate = false;
    said(&mut s, "claude", "meanwhile");
    assert_eq!(claude_heard(&claude), ("meanwhile".into(), true), "nothing to tell, and no separate task named");
    assert_eq!(wait(&mut s, alone), Status::Done);
}

#[test]
fn a_turn_that_cannot_start_leaves_its_handoff_for_the_next() {
    let r = repo("no-start");
    let (codex, claude) = (fake(&root(), "codex-no-start", &codex_ok()), fake(&root(), "claude-no-start", CLAUDE_TALKS));
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    said(&mut s, "claude", "fix it");
    let theirs = said(&mut s, "codex", "take it from here");
    // Claude's program is there but cannot run: the turn does not start, so it was told nothing
    let broken = root().join("claude-not-executable");
    fs::write(&broken, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o644)).unwrap();
    s.programs.insert(agent("claude"), broken);
    assert!(s.launch_agent(agent("claude"), false, "and now").is_err());
    s.programs.insert(agent("claude"), claude.clone());
    said(&mut s, "claude", "and now");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.contains(&format!("#{theirs} · the owner to codex: take it from here")), "{told}");
}

#[test]
fn the_record_names_the_files_a_turn_edited_where_git_cannot_see_them() {
    // a folder with a git project inside it gets no /undo, so no snapshot says what a turn changed
    let dir = root().join("no-undo");
    fs::create_dir_all(dir.join("inner/.git")).unwrap();
    let state = root().join("no-undo-state");
    let (codex, claude) = (fake(&root(), "codex-no-undo", &codex_ok()), fake(&root(), "claude-no-undo", CLAUDE_TALKS));
    let mut s = Session::with_state_home(&dir, &state, &root()).unwrap();
    assert!(!s.undoable());
    s.programs.insert(agent("claude"), claude.clone());
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    said(&mut s, "claude", "WRITE-O");
    said(&mut s, "codex", "take it from here");
    let told = turn_text(&codex);
    assert!(told.contains("#1 · the owner to claude: WRITE-O\nclaude: done\n") && told.contains("\nedited (as it reported): o.txt\n")
            && !told.contains("changed:"), "{told}");
}

#[test]
fn codex_compacts_where_claude_does_unless_the_owner_set_it() {
    let r = repo("codex-compact");
    let codex = fake(&root(), "codex-compact", &codex_ok());
    let mut s = session(&r, agent("codex"), &codex);
    let home = root().join("codex-home-compact");
    fs::create_dir_all(&home).unwrap();
    s.codex_home = home.clone();
    let args = |s: &mut Session| {
        s.forget();
        said(s, "codex", "go");
        fs::read_to_string(codex.with_extension("argv")).unwrap()
    };
    assert!(args(&mut s).lines().any(|l| l == "model_auto_compact_token_limit=200000"));
    fs::write(home.join("config.toml"), "model = \"x\"\nmodel_auto_compact_token_limit = 350000\n").unwrap();
    assert!(!args(&mut s).contains("model_auto_compact_token_limit"), "the owner's own limit stands");
}

#[test]
fn separate_agents_work_at_once_and_undo_keeps_their_work_apart() {
    let r = repo("separate");
    let codex = fake(&root(), "codex-separate", &codex_script(&format!("case \"$turn\" in *SLOW*) sleep 1 ;; esac\n{CODEX_EDITS}"),
                                                             &codex_done("all done", (10, 4, 2))));
    let claude = fake(&root(), "claude-separate", CLAUDE_TALKS);
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    s.separate = true;
    let slow = s.launch_agent(agent("codex"), false, "SLOW").unwrap();
    let other = s.launch_agent(agent("claude"), false, "WRITE-O").unwrap();
    assert!(s.launch_agent(agent("codex"), false, "again").unwrap_err().contains(&format!("#{slow}")), "each agent one turn at a time");
    assert_eq!(wait(&mut s, other), Status::Done);
    assert!(s.job(slow).unwrap().running(), "claude finished while codex still works");
    assert_eq!(claude_heard(&claude), ("WRITE-O".into(), false), "neither is told what the other does");
    assert_eq!(wait(&mut s, slow), Status::Done);
    assert_eq!(turn_text(&codex), "SLOW");
    // Each keeps the files it said it edited; what neither said it edited (codex's b.txt and c.bin, written by a
    // command) stays out of /undo, said in its log.
    assert_eq!(s.job(other).unwrap().changed, ["o.txt"]);
    assert_eq!(s.job(slow).unwrap().changed, ["made.txt"]);
    assert!(s.job(slow).unwrap().log.iter().any(|l| l == &format!("b.txt, c.bin, o.txt changed while #{other} worked here too, so /undo leaves them")));
    assert!(s.undo().unwrap().contains(&format!("undid #{other}")));
    assert!(!r.join("o.txt").exists() && r.join("made.txt").exists());
    assert!(s.undo().unwrap().contains(&format!("undid #{slow}")));
    assert!(!r.join("made.txt").exists() && r.join("b.txt").exists(), "only its own file went back");
    // A command the other turn ran could have written any file without saying so: the turn keeps none for /undo.
    let slow = s.launch_agent(agent("codex"), false, "SLOW again").unwrap();
    let other = s.launch_agent(agent("claude"), false, "BASH").unwrap();
    assert_eq!((wait(&mut s, other), wait(&mut s, slow)), (Status::Done, Status::Done));
    assert!(s.job(slow).unwrap().changed.is_empty(), "made.txt came back, but #{other} ran a command meanwhile");
    assert!(s.job(slow).unwrap().log.iter().any(|l| l == &format!("made.txt changed while #{other} ran commands here too, so /undo leaves it")));
    // Back together, the next turn hears nothing of what the others did on their own.
    s.separate = false;
    said(&mut s, "codex", "together");
    assert_eq!(turn_text(&codex), "together");
}

#[test]
fn a_conversation_that_goes_on_after_a_restart_counts_only_what_each_turn_added() {
    let r = repo("usage-restart");
    // A resumed thread reports its usage so far, as the real Codex does: 12 after the first turn, 24 after the second.
    let events = format!("case \"$thread\" in *thread/resume*)\n{}\n;; *)\n{}\n;; esac",
                         codex_done("all done", (20, 8, 4)), codex_done("all done", (10, 4, 2)));
    let codex = fake(&root(), "codex-usage-restart", &codex_script("", &events));
    let open = || {
        let mut s = session(&r, agent("codex"), &codex);
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let mut s = open();
    let first = said(&mut s, "codex", "what is a.txt?");
    assert_eq!(s.job(first).unwrap().tokens.total(), 12);
    let kept = s.keep(None).unwrap();
    drop(s); // a /restart, a quit or a crash: the next shell goes on from what was kept
    let mut next = open();
    next.go_on(&kept).unwrap();
    let second = said(&mut next, "codex", "and then?");
    assert_eq!(next.job(second).unwrap().tokens.total(), 12, "the 12 counted before the restart are not counted again");
    // a conversation that is no longer the agent's (a /clear, a fresh start) hands on no total
    next.forget();
    let cleared = next.keep(None).unwrap();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&fs::read(cleared).unwrap()).unwrap()["usage"], serde_json::json!([]));
}

#[test]
fn a_turn_that_cannot_start_after_a_restart_is_still_told_the_turns_before_it() {
    let r = repo("no-start-restart");
    let (codex, claude) = (fake(&root(), "codex-no-start-restart", &codex_ok()),
                           fake(&root(), "claude-no-start-restart", CLAUDE_TALKS));
    let open = || {
        let mut s = session(&r, agent("claude"), &claude);
        s.programs.insert(agent("codex"), codex.clone());
        s.codex_home = root().join("codex-home-golden");
        s
    };
    let mut s = open();
    said(&mut s, "claude", "fix it");
    said(&mut s, "codex", "take it from here");
    let kept = s.keep(None).unwrap();
    drop(s);
    let mut next = open();
    next.go_on(&kept).unwrap();
    // Claude's program is there but cannot run: the turn does not start, so it was told nothing
    let broken = root().join("claude-not-executable-restart");
    fs::write(&broken, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o644)).unwrap();
    next.programs.insert(agent("claude"), broken);
    assert!(next.launch_agent(agent("claude"), false, "and now").is_err());
    next.programs.insert(agent("claude"), claude.clone());
    said(&mut next, "claude", "and now");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.contains("#2 · the owner to codex: take it from here"), "{told}");
}

#[test]
fn what_a_restart_carries_for_an_agent_stays_bounded() {
    let r = repo("carry-cap");
    let (codex, claude) = (fake(&root(), "codex-carry-cap", &codex_ok()), fake(&root(), "claude-carry-cap", CLAUDE_TALKS));
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex);
    s.codex_home = root().join("codex-home-golden");
    // Claude has a conversation and was away for several restarts: what it is owed came to 150,000 characters
    let old: String = (0..2_500).map(|n| format!("#{n} · the owner to codex: {}\n", "x".repeat(40))).collect();
    assert!(old.len() > 150_000);
    s.resume(&[("claude".into(), "s-1".into())], &[("claude".into(), old)], &[]);
    let latest = said(&mut s, "codex", "take it from here");
    let carried = s.carry().into_iter().find(|(name, _)| name == "claude").unwrap().1;
    assert!(carried.len() <= 60_100, "the sum had no limit: {} characters", carried.len());
    assert!(carried.starts_with("(the earliest turns before the restart are cut)\n"), "{}", &carried[..80]);
    assert!(carried.contains("#2499 · the owner to codex") && !carried.contains("#0 · the owner to codex"), "the newest stay");
    assert!(carried.contains(&format!("#{latest} · the owner to codex: take it from here")), "and the turn since");
}

#[test]
fn a_turn_that_dies_while_starting_leaves_its_handoff_for_the_next() {
    let r = repo("dies-starting");
    let (codex, claude) = (fake(&root(), "codex-dies-starting", &codex_ok()), fake(&root(), "claude-dies-starting", CLAUDE_TALKS));
    let mut s = session(&r, agent("claude"), &claude);
    s.programs.insert(agent("codex"), codex.clone());
    s.codex_home = root().join("codex-home-golden");
    said(&mut s, "claude", "fix it");
    let theirs = said(&mut s, "codex", "take it from here");
    // Claude's program starts and dies before it reads a word: the turn was told nothing, though it did start
    let dies = fake(&root(), "claude-dies", "#!/bin/sh\nhead -n 1 >/dev/null\necho 'cannot reach the login service' >&2\nexit 1\n");
    s.programs.insert(agent("claude"), dies);
    let failed = s.launch_agent(agent("claude"), false, "and now").unwrap();
    assert!(matches!(wait(&mut s, failed), Status::Failed(_)));
    assert!(s.job(failed).unwrap().log.iter().any(|l| l.contains("failed before it read its message")), "{:?}", s.job(failed).unwrap().log);
    s.programs.insert(agent("claude"), claude.clone());
    said(&mut s, "claude", "and now");
    let (told, resumed) = claude_heard(&claude);
    assert!(resumed && told.contains(&format!("#{theirs} · the owner to codex: take it from here")), "{told}");
    // told once: a turn that did read its message keeps what it was told
    said(&mut s, "claude", "again");
    assert_eq!(claude_heard(&claude), ("again".into(), true));
}

#[test]
fn a_compacted_conversation_goes_on_instead_of_starting_fresh() {
    let r = repo("compacted");
    // grows past the handoff size on GROW; /compact answers with the boundary Claude Code sends and no words
    let claude = fake(&root(), "claude-compacted", r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
head -n 1 > "$0.stdin"
echo '{"type":"system","subtype":"init","session_id":"s-1","apiKeySource":"none"}'
case "$(cat "$0.stdin")" in
  *'/compact"}}') echo '{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":70005}}'
    echo '{"type":"result","subtype":"success","is_error":false,"result":""}'; exit 0 ;;
  *'GROW"}}') big=70000 ;; *) big=10 ;; esac
echo '{"type":"assistant","message":{"id":"m1","content":[],"usage":{"input_tokens":'$big',"output_tokens":5}},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done"}'
"#);
    let mut s = session(&r, agent("claude"), &claude);
    said(&mut s, "claude", "GROW");
    said(&mut s, "claude", "/compact");
    said(&mut s, "claude", "and now");
    // before, the size from before the compaction stood, so this turn started fresh and the compaction was thrown away
    assert_eq!(claude_heard(&claude), ("and now".into(), true), "the compacted conversation goes on");
    // a conversation still past the size after a turn starts fresh, as ever
    said(&mut s, "claude", "GROW");
    said(&mut s, "claude", "next");
    let (told, resumed) = claude_heard(&claude);
    assert!(!resumed && told.starts_with("Ringo's record of this conversation so far"), "{told}");
}

fn usage_rows(state: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(state.join("usage.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn the_usage_record_says_what_its_counts_are_of_and_keeps_a_turn_that_reported_none() {
    let r = repo("usage-rows");
    let state = root().join("usage-rows-state");
    let _ = fs::remove_dir_all(&state);
    let counted = fake(&root(), "claude-usage-rows", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-u\",\"apiKeySource\":\"none\",\"model\":\"claude-test-1\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"model_usage_since_last_result\":{\"claude-test-1\":{\"inputTokens\":5,\"outputTokens\":6}}}'\n");
    // a turn that ends well and reports no count anywhere
    let silent = fake(&root(), "claude-usage-silent", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-u\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}'\n");
    let mut s = Session::with_state(&r, &state).unwrap();
    s.programs.insert(agent("claude"), counted);
    s.tier = ringo_cli::Tier::Deep;
    let first = said(&mut s, "claude", "work");
    s.tier = ringo_cli::Tier::Standard;
    s.programs.insert(agent("claude"), silent);
    let second = said(&mut s, "claude", "more");
    let rows = usage_rows(&state);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!((rows[0]["agent"].as_str(), rows[0]["tier"].as_str(), rows[0]["model"].as_str()),
               (Some("claude"), Some("deep"), Some("claude-test-1")));
    assert_eq!((rows[0]["input"].as_u64(), rows[0]["output"].as_u64()), (Some(5), Some(6)));
    assert_eq!(rows[0]["job"].as_str(), s.job(first).unwrap().dir.to_str());
    assert!(rows[0].get("unknown").is_none());
    // unknown is not zero: the row is there, says so, and holds no counts
    assert_eq!((rows[1]["unknown"].as_bool(), rows[1]["tier"].as_str()), (Some(true), Some("standard")));
    assert!(rows[1].get("input").is_none() && rows[1].get("model").is_none(), "{}", rows[1]);
    assert_eq!(rows[1]["job"].as_str(), s.job(second).unwrap().dir.to_str());
    assert_eq!(s.used_since(86_400).0.get("claude").map(|t| t.total()), Some(11), "/usage sums what was counted");
}

#[test]
fn an_older_claude_that_reports_a_running_total_is_counted_once() {
    let r = repo("claude-running-total");
    let state = root().join("claude-running-total-state");
    let _ = fs::remove_dir_all(&state);
    // Claude Code before 2.1.316: modelUsage alone, the whole session's so far (100, then 150 after a second turn)
    let claude = fake(&root(), "claude-running-total", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        n=$(cat \"$0.n\" 2>/dev/null || echo 0); n=$((n + 1)); echo $n > \"$0.n\"\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-o\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"modelUsage\":{\"m\":{\"inputTokens\":'$((50 + 50 * n))',\"outputTokens\":0}}}'\n");
    let mut s = Session::with_state(&r, &state).unwrap();
    s.programs.insert(agent("claude"), claude);
    let (first, second) = (said(&mut s, "claude", "one"), said(&mut s, "claude", "two"));
    assert_eq!((s.job(first).unwrap().tokens.total(), s.job(second).unwrap().tokens.total()), (100, 50), "the first turn's 100 counted again");
    assert_eq!(s.used_since(86_400).0.get("claude").map(|t| t.total()), Some(150));
}

#[test]
fn a_pipeline_call_without_counts_is_marked_in_the_usage_record() {
    let r = repo("uncounted");
    let state = root().join("uncounted-state");
    let _ = fs::remove_dir_all(&state);
    // a stand-in ringo.py: two finished calls, the second (first run) or both (later runs) without token counts
    let home = root().join("fake-uncounted-home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("ringo.py"), "import json, sys, pathlib\n\
        a = sys.argv[1:]\n\
        val = lambda k: a[a.index(k) + 1]\n\
        here = pathlib.Path(sys.argv[0]).parent\n\
        n = len(list(here.glob('ran-*'))) + 1\n\
        (here / f'ran-{n}').write_text('')\n\
        first = {'input': 900, 'cache_write': 0, 'cache_read': 0, 'output': 40} if n == 1 else {'cost_usd': None}\n\
        t = pathlib.Path(val('--trace')); t.mkdir(parents=True, exist_ok=True)\n\
        (t / 'trace.jsonl').write_text(json.dumps({'step': 'a', 'status': 'ok', 'usage': first}) + '\\n' +\n\
            json.dumps({'step': 'b', 'status': 'ok', 'usage': None}) + '\\n')\n\
        pathlib.Path(val('--out')).write_text('answer\\n')\n").unwrap();
    let mut s = Session::with_state(&r, &state).unwrap();
    s.ringo_home = home;
    for question in ["one?", "two?"] {
        let id = s.ask(question).unwrap();
        assert_eq!(wait(&mut s, id), Status::Done);
    }
    let rows = usage_rows(&state);
    assert_eq!(rows.len(), 2, "{rows:?}");
    // counts of the call that reported them, and a mark that another call's are missing
    assert_eq!((rows[0]["input"].as_u64(), rows[0]["output"].as_u64(), rows[0]["uncounted_calls"].as_u64()), (Some(900), Some(40), Some(1)));
    assert_eq!((rows[0]["agent"].as_str(), rows[0]["command"].as_str()), (Some("ringo"), Some("ask")));
    assert!(rows[0].get("unknown").is_none());
    // no call reported any: unknown, with how many
    assert_eq!((rows[1]["unknown"].as_bool(), rows[1]["uncounted_calls"].as_u64()), (Some(true), Some(2)));
    assert!(rows[1].get("input").is_none(), "{}", rows[1]);
    assert_eq!(s.jobs[0].tokens.total(), 940);
}

#[test]
fn an_older_claude_write_job_counts_a_repeated_running_total_once() {
    let r = repo("claude-write-total");
    // one run that ends in two results (a line taken in while it worked), each with the session's total so far
    let claude = fake(&root(), "claude-write-total", "#!/bin/sh\nhead -n 1 >/dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-w\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"one\",\"modelUsage\":{\"m\":{\"inputTokens\":100,\"outputTokens\":0}}}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"modelUsage\":{\"m\":{\"inputTokens\":150,\"outputTokens\":0}}}'\n");
    let mut s = session(&r, agent("claude"), &claude);
    let id = s.launch_agent(agent("claude"), true, "write it").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    assert_eq!(s.job(id).unwrap().tokens.total(), 150, "100 then 150 is 150 in all, not 250");
}

/// /model: the owner's choice of model goes to each agent its own way, on a new conversation and on one it goes on
/// with; an agent with no choice is started as it always was. Codex's list of models is read from its own answer.
#[test]
fn each_agent_is_started_on_the_model_its_owner_chose() {
    let r = repo("models");
    let codex = fake(&root(), "codex-models", &codex_script("", &codex_done("done", (1, 0, 1))));
    let claude = fake(&root(), "claude-models", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n\
        head -n 1 > /dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-m\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n");
    let mut s = session(&r, agent("codex"), &codex);
    s.programs.insert(agent("claude"), claude.clone());
    let run = |s: &mut Session| {
        for name in ["codex", "claude"] {
            let id = s.launch_agent(agent(name), false, "the task").unwrap();
            assert_eq!(wait(s, id), Status::Done);
        }
        let thread = threads(&codex).pop().unwrap();
        let argv: Vec<String> = fs::read_to_string(format!("{}.argv", claude.display())).unwrap().lines().map(String::from).collect();
        let chosen = argv.iter().position(|a| a == "--model").map(|at| argv[at + 1].clone());
        (thread["method"].as_str().unwrap().to_string(), thread["params"]["model"].as_str().map(String::from), chosen)
    };
    assert_eq!(run(&mut s), ("thread/start".into(), None, None), "no choice: each on its own default");
    s.models.insert(agent("codex"), "gpt-test".into());
    s.models.insert(agent("claude"), "opus".into());
    assert_eq!(run(&mut s), ("thread/resume".into(), Some("gpt-test".into()), Some("opus".into())), "a conversation goes on with it");
    s.forget();
    assert_eq!(run(&mut s), ("thread/start".into(), Some("gpt-test".into()), Some("opus".into())), "and a new one starts on it");
    s.models.clear();
    assert_eq!(run(&mut s).1, None, "back to its own");

    let lister = fake(&root(), "codex-model-list", "#!/bin/sh\nread -r hello; read -r ready; read -r ask\n\
        echo '{\"id\":1,\"result\":{}}'\n\
        echo '{\"id\":2,\"result\":{\"data\":[{\"id\":\"a\",\"model\":\"m-big\",\"description\":\"the big one\",\"isDefault\":true},\
              {\"id\":\"b\",\"model\":\"m-hidden\",\"hidden\":true},{\"id\":\"m-small\",\"description\":\"small\"}]}}'\n");
    let listed: Vec<(String, String, bool)> = ringo_cli::models(agent("codex"), &lister).unwrap()
        .into_iter().map(|m| (m.name, m.what, m.default)).collect();
    assert_eq!(listed, [("m-big".to_string(), "the big one".to_string(), true), ("m-small".into(), "small".into(), false)]);
    assert!(ringo_cli::models(agent("claude"), &claude).is_err(), "Claude Code has no list to read");
    for (name, ok) in [("opus", true), ("claude-fable-5-2[1m]", true), ("gpt-6.1-sol", true), ("-p", false), ("a b", false), ("", false)] {
        assert_eq!(ringo_cli::valid_model(name), ok, "{name}");
    }
}

/// An agent of the owner's own: a file of instructions makes Claude or Codex a specialist under its own name. The
/// text goes to its every turn the agent's own way (Claude: appended to the system prompt between the owner's rules
/// and the project's file; Codex: the thread's developer instructions), without a Claude Code agent file's opening
/// block, read anew each turn. A turn that cannot have them does not start. With `skills`, Claude loads the owner's
/// skills, plugins and connectors, but still not their CLAUDE.md.
#[test]
fn an_agent_of_the_owners_own_carries_its_instructions_into_every_turn() {
    let r = repo("own-agent");
    fs::write(r.join("AGENTS.md"), "Project rule.\n").unwrap();
    let state = root().join("state-own-agent");
    let file = root().join("specialist.md");
    fs::write(&file, "---\nname: specialist\ndescription: knows the billing code\n---\n\nYou know the billing code.\n").unwrap();
    let claude = fake(&root(), "claude-own", "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\nenv > \"$0.env\"\n\
        rm -f \"$0.appended\"\nwhile [ $# -gt 0 ]; do [ \"$1\" = --append-system-prompt-file ] && cat \"$2\" > \"$0.appended\"; shift; done\n\
        head -n 1 > /dev/null\n\
        echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-o\",\"apiKeySource\":\"none\"}'\n\
        echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}'\n");
    let codex = fake(&root(), "codex-own", &codex_script("", &codex_done("done", (1, 0, 1))));
    let entry = |name: &str, adapter: &str, skills: bool| serde_json::json!({"name": name, "adapter": adapter, "read": true,
        "program": "claude", "instructions": file, "skills": skills});
    for (bad, why) in [(serde_json::json!({"name": "own-agy", "adapter": "agy", "instructions": "x.md"}), "claude or codex"),
                       (serde_json::json!({"name": "own-plain", "instructions": "x.md"}), "claude or codex"),
                       (entry("own-skilled-codex", "codex", true), "Claude Code's")] {
        let e = ringo_cli::add_custom_agent(&state, bad).unwrap_err();
        assert!(e.contains(why), "{e}");
    }
    let on_claude = ringo_cli::add_custom_agent(&state, entry("own-claude", "claude", false)).unwrap();
    let skilled = ringo_cli::add_custom_agent(&state, entry("own-skilled", "claude", true)).unwrap();
    let on_codex = ringo_cli::add_custom_agent(&state, entry("own-codex", "codex", false)).unwrap();
    let mut s = Session::with_state(&r, &state).unwrap();
    for (agent, program) in [(on_claude, &claude), (skilled, &claude), (agent("claude"), &claude), (on_codex, &codex)] {
        s.programs.insert(agent, program.clone());
    }
    let home = root().join("codex-home-own");
    fs::create_dir_all(&home).unwrap();
    s.codex_home = home.clone();
    let run = |s: &mut Session, agent: Agent| {
        let id = s.launch_agent(agent, false, "the task").unwrap();
        assert_eq!(wait(s, id), Status::Done);
        (fs::read_to_string(claude.with_extension("appended")).unwrap_or_default(),
         fs::read_to_string(format!("{}.argv", claude.display())).unwrap_or_default())
    };
    let (appended, argv) = run(&mut s, on_claude);
    assert_eq!(appended, "You know the billing code.\n\nProject rule.\n", "its instructions, then the project's; no opening block");
    assert!(argv.lines().any(|a| a == "--safe-mode"), "as lean as any Claude turn: {argv}");
    assert_eq!(run(&mut s, agent("claude")).0, "Project rule.\n", "claude itself is not the specialist");
    fs::write(s.rules_path(), "My rule.\n").unwrap();
    fs::write(&file, "You know the billing code, and the ledger.\n").unwrap();
    assert_eq!(run(&mut s, on_claude).0, "My rule.\n\nYou know the billing code, and the ledger.\n\nProject rule.\n",
               "the owner's rules first, and an edit to the file is in the next turn");
    fs::remove_file(r.join("AGENTS.md")).unwrap();
    fs::remove_file(s.rules_path()).unwrap();
    assert_eq!(run(&mut s, on_claude).0, "You know the billing code, and the ledger.", "alone");
    // with skills: the owner's customizations load, their CLAUDE.md and memory still do not
    let argv = run(&mut s, skilled).1;
    let has = |flag: &str| argv.lines().any(|a| a == flag);
    assert!(!has("--safe-mode") && !has("--disable-slash-commands") && has("--agent"), "{argv}");
    let env = fs::read_to_string(format!("{}.env", claude.display())).unwrap();
    assert!(env.contains("CLAUDE_CODE_DISABLE_CLAUDE_MDS=1") && env.contains("CLAUDE_CODE_DISABLE_AUTO_MEMORY=1"), "{env}");
    // Codex: the thread's developer instructions, with Ringo's own note after them when the owner's files call for it
    let told = |s: &mut Session| {
        let id = s.launch_agent(on_codex, false, "the task").unwrap();
        assert_eq!(wait(s, id), Status::Done);
        threads(&codex).pop().unwrap()["params"]["developerInstructions"].as_str().map(String::from).unwrap()
    };
    assert_eq!(told(&mut s), "You know the billing code, and the ledger.");
    fs::write(home.join("AGENTS.md"), "Use Ringo as the default harness.\n").unwrap();
    let both = told(&mut s);
    assert!(both.starts_with("You know the billing code, and the ledger.\n\nRingo launched this conversation"), "{both}");
    // no instructions, no turn: a turn without them would not be this agent's
    let jobs = s.jobs.len();
    fs::write(&file, "---\nname: specialist\n---\n \n").unwrap();
    assert!(s.launch_agent(on_claude, false, "the task").unwrap_err().contains("no text in it"));
    fs::remove_file(&file).unwrap();
    let e = s.launch_agent(on_codex, false, "the task").unwrap_err();
    assert!(e.contains("own-codex's instructions could not be read") && s.jobs.len() == jobs, "{e}");
    // taken away: out of the owner's file, the other entries as they were
    assert_eq!(ringo_cli::remove_custom_agent(&state, "own-claude"), Ok(true));
    assert_eq!(ringo_cli::remove_custom_agent(&state, "own-claude"), Ok(false));
    let kept: serde_json::Value = serde_json::from_slice(&fs::read(state.join("agents.json")).unwrap()).unwrap();
    let names: Vec<&str> = kept["custom"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names, ["own-skilled", "own-codex"]);
}

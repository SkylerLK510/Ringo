//! The conductor against fake agents in throwaway git repositories (Unix: the fakes are shell scripts).
#![cfg(unix)]

use ringo_cli::{Agent, Session, Status};

fn agent(name: &str) -> Agent {
    Agent::from_name(name).unwrap()
}
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
case "$*" in *workspace-write*) echo made > made.txt; printf 'y\nx\n\n' > b.txt
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
    // A resumed thread reports its usage so far, as the real Codex does.
    let so_far = r#"case "$*" in *resume*) echo '{"type":"turn.completed","usage":{"input_tokens":20,"cached_input_tokens":8,"output_tokens":4}}'; exit ;; esac
echo '{"type":"turn.completed""#;
    let codex = fake(&root(), "codex-read", &CODEX_OK.replace("cat >", "sleep 0.3; cat >")
        .replace(r#"echo '{"type":"turn.completed""#, so_far));
    let mut s = session(&r, agent("codex"), &codex);
    assert!(!s.dir.starts_with(&r), "state is outside the repository");
    assert_eq!(fs::metadata(&s.dir).unwrap().permissions().mode() & 0o777, 0o700);
    let first = s.launch_agent(agent("codex"), false, "what is a.txt?").unwrap();
    assert!(s.launch_agent(agent("codex"), false, "again").unwrap_err().contains("busy"));
    assert_eq!(wait(&mut s, first), Status::Done);
    let job = s.job(first).unwrap();
    assert_eq!(job.reply(), "all done");
    assert_eq!(job.tokens.total(), 12);
    assert_eq!(fs::read_to_string(codex.with_extension("stdin")).unwrap(), "what is a.txt?");
    let second = s.launch_agent(agent("codex"), false, "and then?").unwrap();
    assert_eq!(wait(&mut s, second), Status::Done);
    assert_eq!(s.job(second).unwrap().tokens.total(), 12, "only what the resumed turn added (24 so far)");
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
    let codex = fake(&root(), "codex-write", CODEX_OK);
    let mut s = session(&r, agent("codex"), &codex);
    let (one, two) = (s.launch_agent(agent("codex"), true, "make it").unwrap(),
                      s.launch_agent(agent("codex"), true, "make it too").unwrap());
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
    assert_eq!(fs::read_to_string(r.join("b.txt")).unwrap(), "y\nx\n\n");
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
    let garbage = fake(&root(), "codex-garbage", "#!/bin/sh\ncat >/dev/null\necho 'not json'\n");
    let mut s = session(&r, agent("codex"), &garbage);
    let id = s.launch_agent(agent("codex"), false, "x").unwrap();
    assert!(matches!(wait(&mut s, id), Status::Failed(_)));
    let keyed = fake(&root(), "claude-keyed", "#!/bin/sh\ncat >/dev/null\n\
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
}

#[test]
fn a_plain_question_is_one_tool_free_call_that_remembers_the_last_answers() {
    // A stand-in ringo.py: records its argv and the material it was given, then answers through its trace and --out.
    let r = repo("ask");
    let home = root().join("fake-ask-home");
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
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: signal 0 only checks that the process exists.
    assert_eq!(unsafe { libc::kill(child, 0) }, -1, "the model call in its own session is gone");
}

/// Every argument and the stdin each built-in adapter sends, pinned: the registry must reproduce them exactly.
#[test]
fn agent_commands_are_pinned() {
    let r = repo("golden");
    fs::write(r.join("AGENTS.md"), "rules\n").unwrap();
    git(&r, &["add", "."]);
    git(&r, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "rules"]);
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\n";
    let codex = fake(&root(), "codex-golden", &format!("#!/bin/sh\n{RECORD}cat > \"$0.stdin\"\n\
        echo '{{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}}'\n\
        echo '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"done\"}}}}'\n\
        echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n"));
    let claude = fake(&root(), "claude-golden", &format!("#!/bin/sh\n{RECORD}cat > \"$0.stdin\"\n\
        echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\",\"apiKeySource\":\"none\"}}'\n\
        echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
    let grok = fake(&root(), "grok-golden", &format!("#!/bin/sh\n{RECORD}echo done\n"));
    let mut s = session(&r, agent("codex"), &codex);
    s.programs.insert(agent("claude"), claude.clone());
    s.programs.insert(agent("grok"), grok.clone());
    let repo = s.repo.to_string_lossy().into_owned();
    let mut turn = |agent: Agent, write: bool, program: &Path| -> (Vec<String>, String) {
        let id = s.launch_agent(agent, write, "the task").unwrap();
        assert_eq!(wait(&mut s, id), Status::Done);
        let wt = s.job(id).unwrap().worktree.clone().map(|w| w.to_string_lossy().into_owned());
        let argv = fs::read_to_string(format!("{}.argv", program.display())).unwrap_or_default();
        let stdin = fs::read_to_string(format!("{}.stdin", program.display())).unwrap_or_default();
        let _ = fs::remove_file(format!("{}.stdin", program.display()));
        let argv = argv.lines().map(|a| match &wt { Some(w) => a.replace(w.as_str(), "WT"), None => a.to_string() })
            .map(|a| a.replace(&repo, "REPO")).collect();
        (argv, stdin)
    };
    let tail = ["--json", "--skip-git-repo-check", "-c", "forced_login_method=\"chatgpt\"", "-c", "model_provider=\"openai\"", "-"];
    let want = |head: &[&str], rest: &[&str]| -> Vec<String> { head.iter().chain(rest).map(|a| a.to_string()).collect() };
    assert_eq!(turn(agent("codex"), false, &codex), (want(&["exec", "-s", "read-only", "-C", "REPO"], &tail), "the task".into()));
    assert_eq!(turn(agent("codex"), false, &codex),
               (want(&["exec", "resume", "t-1", "-c", "sandbox_mode=\"read-only\""], &tail), "the task".into()));
    assert_eq!(turn(agent("codex"), true, &codex), (want(&["exec", "-s", "workspace-write", "-C", "WT"], &tail), "the task".into()));
    let head = ["-p", "--output-format", "stream-json", "--verbose", "--safe-mode", "--permission-prompts", "none", "--permission-mode"];
    let read = ["default", "--disallowed-tools", "Bash", "Edit", "Write", "NotebookEdit"];
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&read[..], &["--append-system-prompt-file", "REPO/AGENTS.md"]].concat()), "the task".into()));
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&read[..], &["--resume", "s-1", "--append-system-prompt-file", "REPO/AGENTS.md"]].concat()),
                "the task".into()));
    assert_eq!(turn(agent("claude"), true, &claude),
               (want(&head, &["acceptEdits", "--disallowed-tools", "Bash", "--append-system-prompt-file", "WT/AGENTS.md"]),
                "the task".into()));
    assert_eq!(turn(agent("grok"), true, &grok),
               (want(&["-p", "the task", "--output-format", "plain", "--no-auto-update", "--always-approve", "--cwd", "WT"], &[]),
                String::new()));
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
        case \" $* \" in *' --conversation '*) k=2 ;; *) k=1 ;; esac\n\
        echo \"{{\\\"event\\\":\\\"result\\\",\\\"result\\\":{{\\\"status\\\":\\\"SUCCESS\\\",\\\"response\\\":\\\"done\\\",\\\"usage\\\":{{\\\"input_tokens\\\":$((7*k)),\\\"output_tokens\\\":$((2*k))}}}}}}\"\n"));
    let mut s = session(&r, agent("agy"), &agy);
    let mut turn = |write: bool| -> Vec<String> {
        let id = s.launch_agent(agent("agy"), write, r#"-x the "task""#).unwrap();
        assert_eq!(wait(&mut s, id), Status::Done);
        assert_eq!(s.job(id).unwrap().tokens.total(), 9);
        assert_eq!(fs::read_to_string(format!("{}.stdin", agy.display())).unwrap(),
                   "{\"event\":\"user\",\"message\":{\"content\":\"-x the \\\"task\\\"\"}}\n");
        fs::read_to_string(format!("{}.argv", agy.display())).unwrap().lines().map(str::to_string).collect()
    };
    let read = r#"{"permissions":{"deny":["write_file(*)","command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]}}"#;
    let head = ["--input-format", "stream-json", "--output-format", "stream-json", "--settings"];
    assert_eq!(turn(false), [&head[..], &[read]].concat());
    assert_eq!(turn(false), [&head[..], &[read, "--conversation", "c-1"]].concat());
    let write = r#"{"permissions":{"deny":["command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]}}"#;
    assert_eq!(turn(true), [&head[..], &[write, "--mode", "accept-edits"]].concat());
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
    assert!(s.launch_agent(added, false, "x").unwrap_err().contains("no verified read-only mode"));
    let id = s.launch_agent(added, true, "fix {cwd} please").unwrap();
    assert_eq!(wait(&mut s, id), Status::Done);
    let wt = s.job(id).unwrap().worktree.clone().unwrap();
    let argv = fs::read_to_string(format!("{}.argv", tool.display())).unwrap();
    assert_eq!(argv.lines().collect::<Vec<_>>(), ["run", "--in", &*wt.to_string_lossy()]);
    assert_eq!(fs::read_to_string(format!("{}.stdin", tool.display())).unwrap(), "fix {cwd} please");
}

//! The conductor against fake agents in throwaway git repositories (Unix: the fakes are shell scripts).
#![cfg(unix)]

use ringo_cli::{Agent, Decision, Mode, Session, Status};

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
case "$*" in *workspace-write*) echo made > made.txt; printf 'y\nx\n\n' > b.txt; printf '\000\001\002binary' > c.bin
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
    let handoff = s.expand("check {#1} please").unwrap();
    assert!(handoff.contains("--- change made by job #1 ---") && handoff.contains("made.txt"));
    assert!(handoff.contains("Binary files") && !handoff.contains("GIT binary patch"),
            "a binary file is named, not sent as base85: {handoff}");
    assert_eq!(s.expand("render {# note #} and {#x} here").unwrap(), "render {# note #} and {#x} here",
               "only {{#N}} is a handoff");
    fs::write(r.join("a.txt"), "dirty\n").unwrap();
    assert!(s.apply(one).unwrap_err().contains("uncommitted"));
    git(&r, &["checkout", "--", "a.txt"]);
    s.apply(one).unwrap();
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
    let garbage = fake(&root(), "codex-garbage", "#!/bin/sh\ncat >/dev/null\necho 'not json'\n");
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
    let claude = fake(&root(), "claude-golden", &format!("#!/bin/sh\n{RECORD}head -n 1 > \"$0.stdin\"\n\
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
    let tail = ["--json", "--skip-git-repo-check", "-c", "forced_login_method=\"chatgpt\"", "-c", "model_provider=\"openai\"",
                "-c", "features.default_mode_request_user_input=false", "-c", "include_collaboration_mode_instructions=false",
                "-c", "analytics.enabled=false", "-c", "features.plugins=false", "-c", "features.apps=false", "-"];
    let want = |head: &[&str], rest: &[&str]| -> Vec<String> { head.iter().chain(rest).map(|a| a.to_string()).collect() };
    assert_eq!(turn(agent("codex"), false, &codex), (want(&["exec", "-s", "read-only", "-C", "REPO"], &tail), "the task".into()));
    assert_eq!(turn(agent("codex"), false, &codex),
               (want(&["exec", "resume", "t-1", "-c", "sandbox_mode=\"read-only\""], &tail), "the task".into()));
    assert_eq!(turn(agent("codex"), true, &codex), (want(&["exec", "-s", "workspace-write", "-C", "WT"], &tail), "the task".into()));
    // Ask mode (the default): every permission prompt comes to Ringo, the always-ask rules ride along, read turns
    // neither edit nor run commands; the prompt is one stream-json message on stdin, which stays open for the answers.
    let head = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--safe-mode",
                "--permission-prompt-tool", "stdio", "--permission-mode"];
    let rules = serde_json::json!({"permissions": {"ask": ringo_cli::ALWAYS_ASK}}).to_string();
    let deny = |extra: &[&'static str]| -> Vec<&'static str> {
        ["--disallowed-tools"].into_iter().chain(extra.iter().copied()).chain(ringo_cli::OUT_OF_TURN_TOOLS)
            .chain(["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"]).collect()
    };
    let read = [&["default"][..], &deny(&["Bash", "Edit", "Write", "NotebookEdit"]), &["--settings", &rules]].concat();
    let message = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"the task\"}}\n".to_string();
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&read[..], &["--append-system-prompt-file", "REPO/AGENTS.md"]].concat()), message.clone()));
    assert_eq!(turn(agent("claude"), false, &claude),
               (want(&head, &[&read[..], &["--resume", "s-1", "--append-system-prompt-file", "REPO/AGENTS.md"]].concat()),
                message.clone()));
    assert_eq!(turn(agent("claude"), true, &claude),
               (want(&head, &[&["acceptEdits"][..], &deny(&[]), &["--settings", &rules, "--append-system-prompt-file",
                                                                  "WT/AGENTS.md"]].concat()),
                message.clone()));
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
        if [ -e \"$0.fail\" ]; then rm \"$0.fail\"; echo '{{\"event\":\"result\",\"result\":{{\"status\":\"ERROR\",\"response\":\"quota\",\"usage\":{{}}}}}}'; exit 0; fi\n\
        case \" $* \" in *' --mode '*) k=1 ;; *' --conversation '*) k=$(($(cat \"$0.k\") + 1)); echo $k > \"$0.k\" ;; *) k=1; echo 1 > \"$0.k\" ;; esac\n\
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
    // A failed resumed turn reports no usage; the running total it leaves is still the last one reported.
    fs::write(format!("{}.fail", agy.display()), "").unwrap();
    let failed = s.launch_agent(agent("agy"), false, "again").unwrap();
    assert!(matches!(wait(&mut s, failed), Status::Failed(_)));
    assert_eq!(s.job(failed).unwrap().tokens.total(), 0);
    let next = s.launch_agent(agent("agy"), false, "and now?").unwrap();
    assert_eq!(wait(&mut s, next), Status::Done);
    assert_eq!(s.job(next).unwrap().tokens.total(), 9, "27 so far, 18 at the last report");
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

#[test]
fn modes_change_what_each_agent_may_do() {
    let r = repo("modes");
    const RECORD: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$0.argv\"\nenv > \"$0.env\"\n";
    let claude = fake(&root(), "claude-modes", &format!("#!/bin/sh\n{RECORD}head -n 1 >/dev/null\n\
        echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-m\",\"apiKeySource\":\"none\"}}'\n\
        echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
    let codex = fake(&root(), "codex-modes", &format!("#!/bin/sh\n{RECORD}cat >/dev/null\n\
        echo '{{\"type\":\"thread.started\",\"thread_id\":\"t-m\"}}'\n\
        echo '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"done\"}}}}'\n\
        echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n"));
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
    assert!(has(&argv, &["exec", "--approve-for-me", "-c", "sandbox_workspace_write.network_access=true", "-C"]), "{argv}");
    assert!(!argv.lines().any(|a| a == "-s"), "--approve-for-me picks the sandbox itself: {argv}");
    assert!(argv.contains("features.plugins=false"), "no connector named: no plugins: {argv}");
    turn(&mut s, "codex", true, "summarise my unread Gmail");
    let argv = read(&codex, "argv");
    assert!(!argv.contains("features.plugins=false") && argv.contains("analytics.enabled=false"), "{argv}");

    // A task naming a connector gets the owner's connectors, without their CLAUDE.md, memory, skills or main agent.
    turn(&mut s, "claude", false, "what is in my Jira sprint?");
    let (argv, env) = (read(&claude, "argv"), read(&claude, "env"));
    assert!(has(&argv, &["--agent", "claude", "--disable-slash-commands"]) && !argv.contains("--safe-mode"), "{argv}");
    assert!(has(&argv, &["--disallowed-tools", "Bash", "Edit", "Write", "NotebookEdit"]),
            "a read turn, in the owner's own tree, neither edits nor runs commands: {argv}");
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

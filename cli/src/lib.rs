//! Ringo's conductor: launches agent turns and Ringo pipelines as background jobs, follows their events, and keeps
//! agent edits in private git worktrees until the owner applies them. The terminal shell (`ringo`) and, later,
//! the IDE drive a `Session` and render its jobs.

pub mod parse;
pub mod rt;

use parse::{Note, Parser, Style, Tokens};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOG_LINES: usize = 2000; // per job in memory; the full stream is on disk

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    Codex,
    Claude,
    Grok,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Codex, Agent::Claude, Agent::Grok];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Codex => "codex",
            Agent::Claude => "claude",
            Agent::Grok => "grok",
        }
    }

    pub fn from_name(name: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.name() == name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Agent { agent: Agent, write: bool },
    Ringo { command: String },
}

pub struct Sub {
    pub id: String,
    pub label: String,
    pub activity: String,
    pub ended: Option<bool>,
}

pub struct Job {
    pub id: usize,
    pub kind: Kind,
    pub title: String,
    /// The task as given, uncut.
    pub title_full: String,
    pub status: Status,
    pub started: Instant,
    pub elapsed: Option<Duration>,
    pub activity: String,
    pub tokens: Tokens,
    pub calls: u32,
    pub subs: Vec<Sub>,
    pub log: VecDeque<String>,
    pub dir: PathBuf,
    pub worktree: Option<PathBuf>,
    pub base: Option<String>,
    pub applied: bool,
    /// Files the job edited, relative to its working folder.
    pub touched: BTreeSet<String>,
    pid: Option<u32>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn running(&self) -> bool {
        self.status == Status::Running
    }
    pub fn age(&self) -> Duration {
        self.elapsed.unwrap_or_else(|| self.started.elapsed())
    }
    pub fn reply(&self) -> String {
        fs::read_to_string(self.dir.join("reply.md")).unwrap_or_default()
    }
    fn push_log(&mut self, line: String) {
        for l in line.lines() {
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(l.to_string());
        }
    }
}

pub enum Event {
    Note(usize, Note),
    Ended(usize, Status),
}

pub struct Session {
    pub repo: PathBuf,
    /// Whether `repo` is a git project; outside one, agents can read and answer but not edit in a worktree.
    pub git: bool,
    pub dir: PathBuf,
    pub jobs: Vec<Job>,
    pub max_jobs: usize,
    /// The backend Ringo pipelines use.
    pub backend: String,
    pub ringo_home: PathBuf,
    /// The command that runs a Ringo pipeline, before its arguments: the `ringo` binary itself in the shell (it
    /// picks the native or the Python runtime as the release gate says); None means `python3 ringo_home/ringo.py`.
    pub pipeline: Option<Vec<OsString>>,
    /// The program each agent runs; tests point these at fakes.
    pub programs: HashMap<Agent, PathBuf>,
    /// Why no job may start now (an agent update is replacing a program), if one may not.
    pub hold: Option<String>,
    threads: HashMap<Agent, String>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

/// Where Ringo keeps private state: the same root as its releases (RINGO_RELEASE_HOME or ~/.local/share/ringo).
pub fn state_root() -> PathBuf {
    if let Some(root) = std::env::var_os("RINGO_RELEASE_HOME") {
        return PathBuf::from(root);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".local/share/ringo")
}

/// The Ringo checkout whose ringo.py runs the pipelines: RINGO_HOME, else the checkout this binary was built from.
pub fn ringo_home() -> PathBuf {
    std::env::var_os("RINGO_HOME").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf())
}

pub fn python() -> OsString {
    std::env::var_os("RINGO_PYTHON").unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into())
}

/// Whether a program is on PATH (or is an existing path).
pub fn installed(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| {
        d.join(program).is_file() || (cfg!(windows) && d.join(program).with_extension("exe").is_file())
    }))
}

fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// git's stdout exactly as written (a patch must stay byte-for-byte intact).
fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

/// git's stdout as a trimmed value (an id, a path, a status).
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git_raw(dir, args)?).trim_end().to_string())
}

/// An absolute path with symlinks resolved, for a path whose tail may not exist yet.
fn resolved(path: &Path) -> PathBuf {
    let path = std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf());
    let mut tail = Vec::new();
    let mut head = path.as_path();
    loop {
        if let Ok(real) = head.canonicalize() {
            return tail.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                head = parent;
            }
            _ => return path,
        }
    }
}

/// The agent child's environment: no API keys or endpoint overrides (subscription logins only), and RINGO_CHILD
/// so an agent cannot start Ringo again.
fn agent_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.ends_with("API_KEY") || k.ends_with("AUTH_TOKEN") || ["OPENAI_BASE_URL", "OPENAI_API_BASE",
            "OPENAI_ACCESS_TOKEN", "ANTHROPIC_BASE_URL", "OPENAI_IDENTITY_TOKEN_FILE",
            "OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID"].contains(&k.as_ref()) {
            cmd.env_remove(&key);
        }
    }
    cmd.env("RINGO_CHILD", "1");
}

/// Stops a job's whole process group: TERM, then KILL after a grace period. A Ringo pipeline gets INT first: its
/// model calls run in sessions of their own, out of the group's reach, and Python's interrupt handling ends them.
fn stop_group(pid: u32, grace: Duration, pipeline: bool) {
    #[cfg(unix)]
    {
        let signal = move |sig| unsafe { libc::killpg(pid as libc::pid_t, sig) }; // SAFETY: only sends a signal
        if pipeline {
            signal(libc::SIGINT);
        } else {
            signal(libc::SIGTERM);
        }
        thread::spawn(move || {
            thread::sleep(grace);
            if pipeline {
                signal(libc::SIGTERM);
                thread::sleep(grace);
            }
            signal(libc::SIGKILL);
        });
    }
    #[cfg(not(unix))]
    {
        let _ = (grace, pipeline);
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
    }
}

impl Session {
    pub fn new(cwd: &Path) -> Result<Session, String> {
        Session::with_state(cwd, &state_root())
    }

    /// A session whose private state lives under `root` (a relative root is taken from the current folder).
    pub fn with_state(cwd: &Path, root: &Path) -> Result<Session, String> {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
        Session::with_state_home(cwd, root, &home)
    }

    /// `with_state` with the owner's home folder given. A repository whose root is the home folder or above it
    /// holds the home folder (a dotfiles repository), not a project: there, as outside git, the session works in the
    /// current folder, as claude and codex do, and only `@agent!` and `/review` (which need a project) are refused.
    pub fn with_state_home(cwd: &Path, root: &Path, home: &Path) -> Result<Session, String> {
        let home = resolved(home);
        let top = git(cwd, &["rev-parse", "--show-toplevel"]).ok().map(|t| resolved(Path::new(&t)));
        let project = top.filter(|t| home.as_os_str().is_empty() || !home.starts_with(t));
        let repo = project.clone().unwrap_or_else(|| resolved(cwd));
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if root.is_symlink() {
            return Err(format!("the state folder {} cannot be a symlink", root.display()));
        }
        let parent = resolved(&root.join("shell")); // a symlinked shell folder resolves too
        if project.as_ref().is_some_and(|p| parent.starts_with(p)) {
            return Err(format!("the state folder {} is inside the repository", parent.display()));
        }
        private_dir(&parent)?;
        // An enclosing repository (a dotfiles repository holding the home folder) must never pick up the private
        // state, even with `git add -A`.
        let ignore = resolved(root).join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "# Ringo's private state: never commit it\n*\n").map_err(|e| format!("{}: {e}", ignore.display()))?;
        }
        // create_dir claims a name atomically, so two sessions never share a folder
        let dir = (0..100).map(|n| parent.join(format!("{stamp}-{}-{n}", std::process::id())))
            .find(|d| fs::create_dir(d).is_ok()).ok_or(format!("cannot create a session folder in {}", parent.display()))?;
        private_dir(&dir)?;
        let (tx, rx) = channel();
        let programs = Agent::ALL.iter().map(|a| (*a, PathBuf::from(a.name()))).collect();
        Ok(Session { repo, git: project.is_some(), dir, jobs: Vec::new(), max_jobs: 3, backend: "codex-chatgpt".into(),
                     ringo_home: ringo_home(), pipeline: None, programs, hold: None, threads: HashMap::new(), tx, rx })
    }

    pub fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.running()).count()
    }

    pub fn tokens(&self) -> u64 {
        self.jobs.iter().map(|j| j.tokens.total()).sum()
    }

    pub fn job(&self, id: usize) -> Result<&Job, String> {
        self.jobs.get(id.wrapping_sub(1)).ok_or(format!("no job #{id}"))
    }

    pub fn installed(&self, agent: Agent) -> bool {
        installed(&self.programs[&agent])
    }

    fn new_job(&mut self, kind: Kind, title: &str) -> Result<usize, String> {
        if let Some(why) = &self.hold {
            return Err(format!("{why}; try again when it finishes"));
        }
        if self.running() >= self.max_jobs {
            return Err(format!("{} jobs are running (limit {}); wait for one or /kill it", self.running(), self.max_jobs));
        }
        let id = self.jobs.len() + 1;
        let dir = self.dir.join("jobs").join(id.to_string());
        private_dir(&dir)?;
        self.jobs.push(Job { id, kind, title: parse::short(title, 60), title_full: title.to_string(), status: Status::Running, started: Instant::now(),
                             elapsed: None, activity: "starting".into(), tokens: Tokens::default(), calls: 0,
                             subs: Vec::new(), log: VecDeque::new(), dir, worktree: None, base: None, applied: false,
                             touched: BTreeSet::new(), pid: None, cancel: Arc::new(AtomicBool::new(false)) });
        Ok(id)
    }

    /// One agent turn. Read turns run in the repository with the agent's read-only setting and continue that
    /// agent's conversation; write turns start fresh in a private worktree from HEAD.
    pub fn launch_agent(&mut self, agent: Agent, write: bool, prompt: &str) -> Result<usize, String> {
        if write && !self.git {
            return Err(format!("@{}! edits in a private git worktree, so it needs a git project: start ringo inside one \
                                (or run git init here). @{} without ! works in any folder", agent.name(), agent.name()));
        }
        if !self.installed(agent) {
            return Err(format!("{} is not installed (no `{}` on PATH)", agent.name(), self.programs[&agent].display()));
        }
        if agent == Agent::Grok && !write {
            return Err("grok has no verified read-only mode; use @grok! for a write job in its own worktree".into());
        }
        if !write && let Some(busy) = self.jobs.iter().find(|j| j.running() && j.kind == (Kind::Agent { agent, write: false })) {
            return Err(format!("{} is busy with #{}; its conversation takes one turn at a time", agent.name(), busy.id));
        }
        let base = if write { Some(git(&self.repo, &["rev-parse", "HEAD"])?) } else { None };
        let id = self.new_job(Kind::Agent { agent, write }, prompt)?;
        let dir = self.jobs[id - 1].dir.clone();
        let _ = fs::write(dir.join("prompt.txt"), prompt);
        let cwd = match &base {
            Some(base) => {
                let wt = dir.join("wt");
                if let Err(e) = git(&self.repo, &["worktree", "add", "--detach", &wt.to_string_lossy(), base]) {
                    self.end(id, Status::Failed(e.clone()));
                    return Err(e);
                }
                let job = &mut self.jobs[id - 1];
                job.worktree = Some(wt.clone());
                job.base = Some(base.clone());
                job.push_log(format!("worktree {} from HEAD {}; uncommitted changes in your tree are not in it",
                                     wt.display(), &base[..12.min(base.len())]));
                wt
            }
            None => self.repo.clone(),
        };
        let program = self.programs[&agent].clone();
        let mut cmd = Command::new(&program);
        let resume = if write { None } else { self.threads.get(&agent).cloned() };
        let sandbox = if write { "workspace-write" } else { "read-only" };
        let mut stdin_prompt = true;
        let style = match agent {
            Agent::Codex => {
                cmd.args(["exec"]);
                if let Some(thread) = &resume {
                    cmd.args(["resume", thread]).arg("-c").arg(format!("sandbox_mode=\"{sandbox}\""));
                } else {
                    cmd.args(["-s", sandbox, "-C"]).arg(&cwd);
                }
                cmd.args(["--json", "--skip-git-repo-check", "-c", "forced_login_method=\"chatgpt\"",
                          "-c", "model_provider=\"openai\"", "-"]);
                Style::Codex
            }
            Agent::Claude => {
                // --safe-mode drops the owner's CLAUDE.md, skills, plugins, hooks and MCP servers: one trivial turn
                // measured 16.6k tokens instead of 50.2k, with the subscription login and built-in subagents kept.
                // The repository's own instructions are appended instead.
                cmd.args(["-p", "--output-format", "stream-json", "--verbose", "--safe-mode", "--permission-prompts",
                          "none", "--permission-mode", if write { "acceptEdits" } else { "default" }]);
                // Safe mode keeps the owner's permission rules; deny rules win over them. Read turns cannot edit or
                // run commands, write turns cannot run commands (they have no sandbox).
                cmd.arg("--disallowed-tools").args(if write { &["Bash"][..] } else { &["Bash", "Edit", "Write", "NotebookEdit"][..] });
                if let Some(session) = &resume {
                    cmd.args(["--resume", session]);
                }
                if let Some(doc) = ["CLAUDE.md", "AGENTS.md"].iter().map(|d| cwd.join(d)).find(|p| p.is_file()) {
                    cmd.arg("--append-system-prompt-file").arg(doc);
                }
                Style::Claude
            }
            Agent::Grok => {
                // Documented flags (docs.x.ai/build/cli/headless-scripting); unverified here: grok is not installed.
                // --always-approve approves every tool call, so this runs unsandboxed inside its worktree.
                cmd.arg("-p").arg(prompt).args(["--output-format", "plain", "--no-auto-update", "--always-approve",
                                                "--cwd"]).arg(&cwd);
                stdin_prompt = false;
                Style::Plain
            }
        };
        cmd.current_dir(&cwd);
        agent_env(&mut cmd);
        self.spawn(id, cmd, style, if stdin_prompt { Some(prompt.to_string()) } else { None }, None)?;
        Ok(id)
    }

    /// A Ringo pipeline (`ringo.py ARGS`), its trace followed for calls and tokens. The pipeline's own arguments
    /// and environment are passed unchanged.
    pub fn launch_ringo(&mut self, title: &str, args: Vec<OsString>, trace: PathBuf) -> Result<usize, String> {
        let command = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
        self.launch_ringo_as(&command, title, args, trace)
    }

    fn launch_ringo_as(&mut self, command: &str, title: &str, args: Vec<OsString>, trace: PathBuf) -> Result<usize, String> {
        let id = self.new_job(Kind::Ringo { command: command.into() }, title)?;
        let prefix = self.pipeline.clone().unwrap_or_else(|| vec![python(), self.ringo_home.join("ringo.py").into()]);
        let mut cmd = Command::new(&prefix[0]);
        cmd.args(&prefix[1..]).args(args).current_dir(&self.repo);
        self.spawn(id, cmd, Style::Trace, None, Some(trace))?;
        Ok(id)
    }

    /// `/review` of an agent's write job: its change becomes an unreferenced commit on the job's base (no hooks,
    /// no branch), reviewed by Ringo with the task as the change description.
    pub fn review_job(&mut self, of: usize) -> Result<usize, String> {
        let job = self.job(of)?;
        let (Some(wt), Some(base)) = (job.worktree.clone(), job.base.clone()) else {
            return Err(format!("#{of} is not a write job with a worktree"));
        };
        if job.status != Status::Done {
            return Err(format!("#{of} has not finished"));
        }
        git(&wt, &["add", "-A"])?;
        let tree = git(&wt, &["write-tree"])?;
        let head = git(&wt, &["-c", "user.name=ringo", "-c", "user.email=ringo@localhost", "commit-tree", &tree,
                              "-p", &base, "-m", &format!("ringo job {of}")])?;
        let prompt = fs::read_to_string(job.dir.join("prompt.txt")).unwrap_or_else(|_| job.title.clone());
        let dir = self.next_dir()?;
        let pr = dir.join("pr.txt");
        fs::write(&pr, format!("The task given to the agent:\n\n{prompt}\n")).map_err(|e| e.to_string())?;
        let args = self.review_args(&wt, &base, &head, &dir, Some(&pr));
        self.launch_ringo(&format!("review of #{of}"), args, dir.join("trace"))
    }

    /// The folder the next job will use.
    fn next_dir(&self) -> Result<PathBuf, String> {
        let dir = self.dir.join("jobs").join((self.jobs.len() + 1).to_string());
        private_dir(&dir)?;
        Ok(dir)
    }

    /// `/review BASE [HEAD]` of the owner's repository.
    pub fn review_repo(&mut self, base: &str, head: &str) -> Result<usize, String> {
        if !self.git {
            return Err("/review BASE [HEAD] reviews a git project's commits: start ringo inside one".into());
        }
        let dir = self.next_dir()?;
        let args = self.review_args(&self.repo, base, head, &dir, None);
        self.launch_ringo(&format!("review {base}..{head}"), args, dir.join("trace"))
    }

    fn review_args(&self, repo: &Path, base: &str, head: &str, dir: &Path, pr: Option<&Path>) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["review".into(), "--repo".into(), repo.into()];
        args.extend(["--base", base, "--head", head, "--backend", &self.backend, "--timeout", "180"].map(Into::into));
        if let Some(pr) = pr {
            args.extend(["--pr-text".into(), pr.into()]);
        }
        args.extend(["--trace".into(), dir.join("trace").into(), "--out".into(), dir.join("reply.md").into()]);
        args
    }

    /// `/design TEXT`: Ringo's one-call design pipeline on the brief.
    pub fn design(&mut self, brief: &str) -> Result<usize, String> {
        let dir = self.next_dir()?;
        fs::write(dir.join("brief.md"), brief).map_err(|e| e.to_string())?;
        let mut args: Vec<OsString> = vec!["design".into(), "--input".into(), dir.join("brief.md").into()];
        args.extend(["--backend", &self.backend, "--timeout", "180"].map(Into::into));
        args.extend(["--trace".into(), dir.join("trace").into(), "--out".into(), dir.join("reply.md").into()]);
        self.launch_ringo(brief, args, dir.join("trace"))
    }

    /// A plain line: one tool-free model call through Ringo's design pipeline with the ask plan (plans/ask.json),
    /// the cheapest way to ask anything. The material is the last few questions and answers of this conversation,
    /// then the question: no agent scaffolding, no tools, no files, so a question costs a few thousand tokens where
    /// an agent turn costs 17,000 before it starts. Questions that need the repository go to @codex or @claude.
    pub fn ask(&mut self, question: &str) -> Result<usize, String> {
        const KEEP: usize = 4; // earlier exchanges carried as context
        const ROOM: usize = 12_000; // characters of them at most, the newest kept first
        let mut earlier: Vec<String> = Vec::new();
        let mut used = 0;
        for job in self.jobs.iter().rev().filter(|j| j.kind == (Kind::Ringo { command: "ask".into() }) && j.status == Status::Done) {
            let text = format!("Question: {}\n\nAnswer:\n{}\n", job.title_full, job.reply().trim());
            if earlier.len() == KEEP || used + text.len() > ROOM {
                break;
            }
            used += text.len();
            earlier.push(text);
        }
        earlier.reverse();
        let mut material = String::new();
        if !earlier.is_empty() {
            material.push_str("Earlier in this conversation:\n\n");
            material.push_str(&earlier.join("\n"));
            material.push_str("\n---\n\n");
        }
        material.push_str(&format!("Question: {question}\n"));
        let dir = self.next_dir()?;
        fs::write(dir.join("ask.md"), &material).map_err(|e| e.to_string())?;
        let mut args: Vec<OsString> = vec!["design".into(), "--pipeline".into(), self.ringo_home.join("plans/ask.json").into(),
                                           "--input".into(), dir.join("ask.md").into()];
        args.extend(["--backend", &self.backend, "--timeout", "180"].map(Into::into));
        args.extend(["--trace".into(), dir.join("trace").into(), "--out".into(), dir.join("reply.md").into()]);
        let id = self.launch_ringo_as("ask", question, args, dir.join("trace"))?;
        Ok(id)
    }

    /// `/research TOPIC`: Ringo's research role: one search call, sources fetched and every quote checked in code,
    /// one write call citing only verified evidence (at most two calls).
    pub fn research(&mut self, topic: &str) -> Result<usize, String> {
        let dir = self.next_dir()?;
        fs::write(dir.join("brief.md"), topic).map_err(|e| e.to_string())?;
        let mut args: Vec<OsString> = vec!["research".into(), "--input".into(), dir.join("brief.md").into()];
        args.extend(["--backend", &self.backend].map(Into::into));
        args.extend(["--trace".into(), dir.join("trace").into(), "--out".into(), dir.join("reply.md").into()]);
        self.launch_ringo(topic, args, dir.join("trace"))
    }

    /// `/work SPEC`: Ringo's checked work loop on a task spec.
    pub fn work(&mut self, spec: &Path) -> Result<usize, String> {
        let dir = self.next_dir()?;
        let args = vec!["work".into(), "--spec".into(), spec.into(), "--out".into(), dir.join("work").into()];
        self.launch_ringo(&format!("work {}", spec.display()), args, dir.join("work"))
    }

    fn spawn(&mut self, id: usize, mut cmd: Command, style: Style, stdin: Option<String>, trace: Option<PathBuf>)
             -> Result<(), String> {
        let dir = self.jobs[id - 1].dir.clone();
        let stderr = fs::File::create(dir.join("stderr.txt")).map_err(|e| e.to_string())?;
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stderr(stderr);
        cmd.stdout(if trace.is_some() { Stdio::null() } else { Stdio::piped() });
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // its own group: /kill stops every descendant, and Ctrl-C in the shell does not
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let reason = format!("{}: {e}", cmd.get_program().to_string_lossy());
                self.end(id, Status::Failed(reason.clone()));
                return Err(reason);
            }
        };
        self.jobs[id - 1].pid = Some(child.id());
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes()); // dropping the pipe closes it
            });
        }
        let (tx, cancel, pid) = (self.tx.clone(), self.jobs[id - 1].cancel.clone(), child.id());
        let stdout = child.stdout.take();
        thread::spawn(move || {
            let mut parser = Parser::new(style);
            let mut events = fs::OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl")).ok();
            let mut handle = |line: &str, parser: &mut Parser| {
                if let Some(f) = events.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                for note in parser.line(line) {
                    if matches!(note, Note::Fail(_)) && style != Style::Trace {
                        stop_group(pid, Duration::from_secs(3), false); // fail closed: stop the turn now
                    }
                    let _ = tx.send(Event::Note(id, note));
                }
            };
            if let Some(out) = stdout {
                let mut reader = BufReader::new(out);
                let mut buf = Vec::new();
                while reader.read_until(b'\n', &mut buf).map(|n| n > 0).unwrap_or(false) {
                    handle(String::from_utf8_lossy(&buf).trim_end(), &mut parser);
                    buf.clear();
                }
            } else if let Some(trace) = trace {
                // A Ringo pipeline appends to its trace as it goes: poll for new whole lines.
                let (path, mut seen, mut partial) = (trace.join("trace.jsonl"), 0u64, Vec::new());
                loop {
                    let exited = matches!(child.try_wait(), Ok(Some(_)));
                    if let Ok(mut f) = fs::File::open(&path) {
                        if f.seek(SeekFrom::Start(seen)).is_ok() {
                            seen += f.read_to_end(&mut partial).unwrap_or(0) as u64;
                        }
                        while let Some(end) = partial.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = partial.drain(..=end).collect();
                            handle(String::from_utf8_lossy(&line).trim_end(), &mut parser);
                        }
                    }
                    if exited {
                        break;
                    }
                    // A pipeline's end is seen within one poll: 50 ms, one stat and read each, instead of 300
                    thread::sleep(Duration::from_millis(50));
                }
            }
            let exit = child.wait();
            let status = if cancel.load(Ordering::SeqCst) {
                Status::Cancelled
            } else if let Err(reason) = parser.finish() {
                Status::Failed(reason)
            } else {
                match exit {
                    Ok(code) if code.success() => Status::Done,
                    Ok(code) => {
                        let tail = fs::read_to_string(dir.join("stderr.txt")).unwrap_or_default();
                        let tail: String = tail.trim().lines().rev().take(2).collect::<Vec<_>>().join(" | ");
                        Status::Failed(format!("exit {}{}", code.code().unwrap_or(-1),
                                               if tail.is_empty() { String::new() } else { format!(": {}", parse::short(&tail, 200)) }))
                    }
                    Err(e) => Status::Failed(e.to_string()),
                }
            };
            if style != Style::Trace && !parser.reply().is_empty() {
                let _ = fs::write(dir.join("reply.md"), parser.reply());
            }
            let _ = tx.send(Event::Ended(id, status));
        });
        Ok(())
    }

    fn end(&mut self, id: usize, status: Status) {
        let job = &mut self.jobs[id - 1];
        job.elapsed = Some(job.started.elapsed());
        job.activity = match &status {
            Status::Done => parse::short(&job.reply(), 80),
            Status::Failed(reason) => reason.clone(),
            _ => String::new(),
        };
        if let Status::Failed(reason) = &status {
            job.push_log(format!("failed: {reason}"));
        }
        job.status = status;
        for sub in job.subs.iter_mut().filter(|s| s.ended.is_none()) {
            sub.ended = Some(false);
            sub.activity = "stopped with its parent".into();
        }
    }

    /// Folds pending events into the jobs; true when anything changed.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                Event::Ended(id, status) => {
                    let job = &mut self.jobs[id - 1];
                    if matches!(job.kind, Kind::Ringo { .. }) {
                        let report = job.dir.join("work").join("report.json");
                        let reply = job.reply();
                        if !reply.is_empty() {
                            job.push_log(reply);
                        } else if let Ok(text) = fs::read_to_string(&report) {
                            let state = serde_json::from_str::<serde_json::Value>(&text).ok()
                                .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
                            job.push_log(format!("work {}: {}", state.unwrap_or_default(), report.display()));
                        }
                    }
                    self.end(id, status);
                }
                Event::Note(id, Note::Session(session)) => {
                    if let Kind::Agent { agent, write: false } = self.jobs[id - 1].kind {
                        self.threads.insert(agent, session);
                    }
                }
                Event::Note(id, note) => {
                    let job = &mut self.jobs[id - 1];
                    match note {
                        Note::Session(_) => {}
                        Note::Activity { node: None, text } => job.activity = text,
                        Note::Activity { node: Some(node), text } => {
                            if let Some(sub) = job.subs.iter_mut().find(|s| s.id == node) {
                                sub.activity = text;
                            }
                        }
                        Note::Log(line) => job.push_log(line),
                        Note::Subagent { id: sub, label } => {
                            job.activity = format!("started subagent {label}");
                            job.subs.push(Sub { id: sub, label, activity: "starting".into(), ended: None });
                        }
                        Note::SubagentEnd { id: sub, ok } => {
                            if let Some(s) = job.subs.iter_mut().find(|s| s.id == sub) {
                                s.ended = Some(ok);
                            }
                        }
                        Note::Tokens(t) => job.tokens.add(t),
                        Note::Call => job.calls += 1,
                        Note::Touched(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path);
                            job.touched.insert(rel.to_string_lossy().into_owned());
                        }
                        Note::Fail(reason) => job.push_log(format!("stopping: {reason}")),
                    }
                }
            }
        }
        changed
    }

    /// Other unapplied write jobs that edit the same files as job `id`: (job, shared files). Zero tokens: it is
    /// the agents' own edit events, compared as they arrive.
    pub fn overlaps(&self, id: usize) -> Vec<(usize, Vec<String>)> {
        let Ok(job) = self.job(id) else { return Vec::new() };
        if job.worktree.is_none() || job.applied {
            return Vec::new();
        }
        self.jobs.iter().filter(|o| o.id != id && o.worktree.is_some() && !o.applied && o.status != Status::Cancelled)
            .filter_map(|o| {
                let shared: Vec<String> = job.touched.intersection(&o.touched).cloned().collect();
                (!shared.is_empty()).then_some((o.id, shared))
            }).collect()
    }

    pub fn kill(&mut self, id: usize) -> Result<(), String> {
        let job = self.job(id)?;
        let pid = job.pid.filter(|_| job.running()).ok_or(format!("#{id} is not running"))?;
        job.cancel.store(true, Ordering::SeqCst);
        stop_group(pid, Duration::from_secs(3), matches!(job.kind, Kind::Ringo { .. }));
        Ok(())
    }

    /// The job's change against its base, untracked files included, as a binary patch (also saved in the job
    /// folder).
    pub fn diff(&self, id: usize) -> Result<String, String> {
        let job = self.job(id)?;
        let (Some(wt), Some(base)) = (&job.worktree, &job.base) else {
            return Err(format!("#{id} is not a write job with a worktree"));
        };
        git(wt, &["add", "-A"])?;
        let patch = git_raw(wt, &["diff", "--cached", "--binary", base])?;
        fs::write(job.dir.join("change.patch"), &patch).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&patch).into_owned())
    }

    /// Applies a finished write job's change to the owner's tree, which must be clean. Nothing is committed; a
    /// conflict leaves both the tree and the job as they were.
    pub fn apply(&mut self, id: usize) -> Result<String, String> {
        let job = self.job(id)?;
        if job.status != Status::Done {
            return Err(format!("#{id} has not finished successfully"));
        }
        let patch = self.diff(id)?;
        if patch.is_empty() {
            return Err(format!("#{id} changed nothing"));
        }
        if !git(&self.repo, &["status", "--porcelain"])?.is_empty() {
            return Err("your tree has uncommitted changes; commit or stash them, then /apply again".into());
        }
        let file = self.job(id)?.dir.join("change.patch").to_string_lossy().into_owned();
        git(&self.repo, &["apply", "--check", "--3way", &file])
            .map_err(|e| format!("{e}\n#{id} is untouched; inspect it with /diff {id}"))?;
        git(&self.repo, &["apply", "--3way", &file])?;
        self.jobs[id - 1].applied = true;
        git(&self.repo, &["diff", "--stat", "HEAD"])
    }

    /// Removes a finished write job's worktree (its folder and logs stay).
    pub fn drop_worktree(&mut self, id: usize) -> Result<(), String> {
        let job = self.job(id)?;
        if job.running() {
            return Err(format!("#{id} is running; /kill it first"));
        }
        let wt = job.worktree.clone().ok_or(format!("#{id} has no worktree"))?;
        git(&self.repo, &["worktree", "remove", "--force", &wt.to_string_lossy()])?;
        self.jobs[id - 1].worktree = None;
        Ok(())
    }

    /// Inlines `{#N}`: job N's reply, and its change for a write job. Only finished jobs can be handed off.
    pub fn expand(&self, prompt: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = prompt;
        while let Some(start) = rest.find("{#") {
            let Some(len) = rest[start + 2..].find('}') else { break };
            let n: usize = rest[start + 2..start + 2 + len].parse().map_err(|_| "write {#N} with a job number")?;
            let job = self.job(n)?;
            if job.status != Status::Done {
                return Err(format!("#{n} has not finished successfully, so it cannot be handed off"));
            }
            out.push_str(&rest[..start]);
            out.push_str(&format!("\n--- reply of job #{n} ---\n{}\n", job.reply().trim_end()));
            if job.worktree.is_some() {
                out.push_str(&format!("--- change made by job #{n} ---\n{}\n", self.diff(n)?));
            }
            out.push_str("---\n");
            rest = &rest[start + 3 + len..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Stops every running job (the shell is closing).
    pub fn shutdown(&mut self) {
        for job in self.jobs.iter().filter(|j| j.running()) {
            if let Some(pid) = job.pid {
                job.cancel.store(true, Ordering::SeqCst);
                stop_group(pid, Duration::from_millis(1500), matches!(job.kind, Kind::Ringo { .. }));
            }
        }
        if self.running() > 0 {
            thread::sleep(Duration::from_millis(3100)); // let the last signal land before the process exits
        }
    }
}

// --- Agents the owner has set up, and their sign-in -------------------------------------------------------------

/// The agents set up with `ringo setup` (state_root()/agents.json); None before the first setup.
pub fn configured_agents() -> Option<Vec<Agent>> {
    let text = fs::read_to_string(state_root().join("agents.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let agents: Vec<Agent> = value.get("agents")?.as_array()?.iter()
        .filter_map(|a| a.as_str().and_then(Agent::from_name)).collect();
    Some(agents)
}

pub fn save_agents(agents: &[Agent]) -> Result<(), String> {
    let root = state_root();
    private_dir(&root)?;
    let names: Vec<&str> = agents.iter().map(|a| a.name()).collect();
    fs::write(root.join("agents.json"), format!("{}\n", serde_json::json!({ "agents": names })))
        .map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq)]
pub enum SignIn {
    /// Signed in with the subscription login; the detail names it.
    Yes(String),
    No(String),
    /// The agent has no status command Ringo can read.
    Unknown,
}

/// Whether the agent is signed in the way Ringo runs it: a subscription login, with API keys removed from the
/// environment. Reads the agent's own status command; no model call is made.
pub fn sign_in(agent: Agent, program: &Path) -> SignIn {
    let args: &[&str] = match agent {
        Agent::Codex => &["login", "status"],
        Agent::Claude => &["auth", "status", "--json"],
        Agent::Grok => return SignIn::Unknown,
    };
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    agent_env(&mut cmd);
    let Ok(out) = cmd.output() else { return SignIn::No("its status command did not run".into()) };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    match agent {
        Agent::Codex if out.status.success() && text.contains("ChatGPT") => SignIn::Yes("ChatGPT".into()),
        Agent::Codex if out.status.success() => SignIn::No(format!("{} (Ringo uses the ChatGPT login)", text.trim())),
        Agent::Claude => {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            match (v.get("loggedIn").and_then(|b| b.as_bool()), v.get("authMethod").and_then(|m| m.as_str())) {
                (Some(true), Some("claude.ai")) => SignIn::Yes(format!(
                    "claude.ai{}", v.get("subscriptionType").and_then(|s| s.as_str()).map(|s| format!(", {s}")).unwrap_or_default())),
                (Some(true), Some(method)) => SignIn::No(format!("signed in with {method}; Ringo uses the claude.ai login")),
                _ => SignIn::No("not signed in".into()),
            }
        }
        _ => SignIn::No(parse::short(&text, 120)),
    }
}

/// The agent's own sign-in command, run in the owner's terminal.
pub fn login_args(agent: Agent) -> &'static [&'static str] {
    match agent {
        Agent::Codex => &["login"],
        Agent::Claude => &["auth", "login"],
        Agent::Grok => &[], // its first interactive start opens the browser sign-in
    }
}

pub fn install_hint(agent: Agent) -> &'static str {
    match agent {
        Agent::Codex => "npm install -g @openai/codex      (or: brew install --cask codex)",
        Agent::Claude => "curl -fsSL https://claude.ai/install.sh | bash      (Windows: irm https://claude.ai/install.ps1 | iex)",
        Agent::Grok => "curl -fsSL https://x.ai/cli/install.sh | bash      (Windows: irm https://x.ai/cli/install.ps1 | iex)",
    }
}

pub fn company(agent: Agent) -> &'static str {
    match agent {
        Agent::Codex => "OpenAI Codex CLI",
        Agent::Claude => "Anthropic Claude Code",
        Agent::Grok => "xAI Grok Build",
    }
}

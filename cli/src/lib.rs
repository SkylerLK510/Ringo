//! Ringo's conductor: launches agent turns and Ringo pipelines as background jobs, follows their events, and keeps
//! agent edits in private git worktrees until the owner applies them. The terminal shell (`ringo`) and, later,
//! the IDE drive a `Session` and render its jobs.

pub mod parse;
pub mod rt;

use parse::{Note, Parser, Permission, Style, Tokens};
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

/// How Ringo talks to an agent's program. The protocols are code; which agents use them, and how each one is
/// installed, signed in and updated, is data: agents.json in the binary, plus the owner's own entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adapter {
    /// `codex exec --json`.
    Codex,
    /// `claude -p --output-format stream-json`.
    Claude,
    /// `agy --input-format stream-json --output-format stream-json`, the prompt on stdin.
    Agy,
    /// Any other program: its own command line, the reply as plain text, write turns only.
    Plain,
}

/// One agent Ringo can drive.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub name: String,
    pub company: String,
    pub adapter: Adapter,
    /// The program run (default: the name).
    pub program: String,
    pub install: String,
    /// Its own sign-in command, run in the owner's terminal (empty: its first interactive start signs in).
    pub login: Vec<String>,
    /// Whether it has a verified read-only mode; without one only `@name!` write turns run.
    pub read: bool,
    /// Plain adapter: the arguments after the program, `{prompt}` and `{cwd}` filled in; without `{prompt}` the
    /// prompt goes on stdin.
    pub args: Vec<String>,
    /// The npm package whose latest release the update check compares against (none: no release check).
    pub npm: Option<String>,
    /// The agent's own updater, for an install that is neither Homebrew nor npm.
    pub self_update: Option<String>,
    /// The vendor's install script for this OS: the update for any other install, run only by `/update`.
    pub installer: Option<String>,
    /// Added by the owner (kept in agents.json), not shipped with Ringo.
    pub custom: bool,
}

/// A name that can be a program on PATH and go into a command line: letters, digits, `-`, `_`, `.`.
pub fn valid_name(name: &str) -> bool {
    name.len() <= 40 && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

impl Spec {
    /// An agents.json entry, or why Ringo cannot run it.
    pub fn from_json(v: &serde_json::Value, custom: bool) -> Result<Spec, String> {
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let list = |k: &str| -> Result<Vec<String>, String> {
            let Some(items) = v.get(k) else { return Ok(Vec::new()) };
            items.as_array().ok_or(format!("\"{k}\" must be a list of strings"))?.iter()
                .map(|x| x.as_str().map(str::to_string).ok_or(format!("\"{k}\" must be a list of strings"))).collect()
        };
        let name = text("name").ok_or("an entry without a \"name\"")?;
        if !valid_name(&name) {
            return Err(format!("`{name}` cannot be an agent name: use letters, digits, - _ or ."));
        }
        let adapter = match text("adapter").as_deref().unwrap_or("plain") {
            "codex" => Adapter::Codex,
            "claude" => Adapter::Claude,
            "agy" => Adapter::Agy,
            "plain" => Adapter::Plain,
            other => return Err(format!("{name}: no adapter `{other}` (codex, claude, agy or plain)")),
        };
        let read = v.get("read").and_then(|b| b.as_bool()).unwrap_or(false);
        if read && adapter == Adapter::Plain {
            return Err(format!("{name}: Ringo cannot verify a read-only mode for a plain agent, so \"read\" must be \
                                false"));
        }
        let os = if cfg!(windows) { "windows" } else { "unix" };
        let installer = v.get("installer").and_then(|i| i.get(os)).and_then(|s| s.as_str()).map(str::to_string);
        Ok(Spec { program: text("program").unwrap_or_else(|| name.clone()),
                  company: text("company").unwrap_or_else(|| format!("{name} (added by you)")),
                  install: text("install").unwrap_or_default(), login: list("login")?, read, args: list("args")?,
                  npm: text("npm"), self_update: text("self_update"), installer, adapter, name, custom })
    }
}

static REGISTRY: std::sync::RwLock<Vec<&'static Spec>> = std::sync::RwLock::new(Vec::new());

/// Every agent this process knows, the built-in ones first.
fn registry() -> std::sync::RwLockReadGuard<'static, Vec<&'static Spec>> {
    static BUILT_IN: std::sync::Once = std::sync::Once::new();
    BUILT_IN.call_once(|| {
        let list: Vec<serde_json::Value> = serde_json::from_str(include_str!("agents.json")).expect("agents.json");
        let mut r = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
        r.extend(list.iter().map(|v| &*Box::leak(Box::new(Spec::from_json(v, false).expect("built-in agent")))));
    });
    REGISTRY.read().unwrap_or_else(|e| e.into_inner())
}

/// An agent: a handle on its entry, cheap to copy and compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Agent(usize);

impl Agent {
    /// Every agent Ringo knows: the built-in ones, then the owner's.
    pub fn all() -> Vec<Agent> {
        (0..registry().len()).map(Agent).collect()
    }

    pub fn spec(self) -> &'static Spec {
        registry()[self.0]
    }

    pub fn name(self) -> &'static str {
        &self.spec().name
    }

    pub fn from_name(name: &str) -> Option<Agent> {
        registry().iter().position(|s| s.name == name).map(Agent)
    }

    /// Adds an agent to this process; refused when the name is taken. Each entry lives as long as the process.
    pub fn register(spec: Spec) -> Result<Agent, String> {
        drop(registry()); // the built-in agents first
        let mut r = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
        if r.iter().any(|s| s.name == spec.name) {
            return Err(format!("Ringo already has an agent called {}", spec.name));
        }
        r.push(Box::leak(Box::new(spec)));
        Ok(Agent(r.len() - 1))
    }
}

/// How far agents act without asking: one switch for every agent (`/mode`, Shift-Tab in the shell).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Agents read, and edit their own worktree, freely; any other action (a command, a connector, the network, a
    /// push or a PR) waits for the owner's yes. Codex cannot stop to ask: it stays inside its sandbox.
    Ask,
    /// Agents act on their own; what their safety review flags, or an always-ask rule names, waits for the owner.
    Auto,
    /// Agents look but do not touch: no edits, no commands, no connectors.
    Read,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Ask, Mode::Auto, Mode::Read];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Read => "read",
        }
    }

    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == name)
    }

    /// The next mode in Shift-Tab's cycle: ask, auto, read.
    pub fn next(self) -> Mode {
        Mode::ALL[(Mode::ALL.iter().position(|m| *m == self).unwrap_or(0) + 1) % Mode::ALL.len()]
    }
}

/// The owner's answer to an agent that asks before it acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Once,
    /// Allow it and the same rule for the rest of this session, in every agent turn.
    Session,
    Deny,
}

/// Actions that wait for the owner in ask and auto mode alike, whatever an allow rule or a safety review says, as
/// Claude permission rules (ask rules win over allow rules). Claude's auto-mode review blocks more on its own
/// (force pushes, destroying files that existed before the session, downloading and running code, sending data
/// out); these name what must reach a person. The owner's own list, `permissions.json` {"ask": [...]} in Ringo's
/// state folder, is added to it.
pub const ALWAYS_ASK: [&str; 16] = [
    "Bash(sudo:*)", "Bash(dd:*)", "Bash(mkfs:*)", "Bash(diskutil:*)", "Bash(git push --force:*)", "Bash(git push -f:*)",
    "Bash(gh release:*)", "Bash(gh repo create:*)", "Bash(gh repo delete:*)", "Bash(gh gist create:*)",
    "Edit(~/.claude/**)", "Write(~/.claude/**)", "Edit(~/.codex/**)", "Write(~/.codex/**)", "Edit(~/.gemini/**)",
    "Write(~/.gemini/**)",
];

/// Claude Code tools that act outside the agent's turn, denied in every Claude turn Ringo starts.
pub const OUT_OF_TURN_TOOLS: [&str; 13] = ["CronCreate", "CronDelete", "CronList", "ScheduleWakeup", "RemoteTrigger",
    "PushNotification", "SendToProject", "SendToProjectContext", "EnterWorktree", "ExitWorktree", "DesignSync",
    "ReportFindings", "Workflow"];

/// Words that name a service reached through the owner's connectors (MCP servers). A Claude turn whose task names
/// one runs with the owner's connectors loaded; any other runs without them (16.6k tokens a trivial turn instead of
/// 27.5k, measured with Claude Code 2.1.316).
const CONNECTOR_WORDS: [&str; 18] = ["jira", "confluence", "atlassian", "clickup", "gmail", "email", "emails", "inbox",
    "calendar", "gdrive", "slack", "linear", "notion", "figma", "datadog", "intercom", "connector", "mcp"];
const CONNECTOR_PHRASES: [&str; 4] = ["google drive", "my drive", "google doc", "google docs"];

/// Whether a Claude permission rule (`Tool`, `Tool(exact)`, `Bash(prefix:*)`, `Edit(~/dir/**)`) covers an action.
/// A `Bash(prefix:*)` rule matches the prefix as a command anywhere in the line (after `;`, `&&`, `|`, `$(`, a
/// newline, or a space: `xargs sudo`), so a chained command cannot slip past it; a match too many only means the
/// action is asked about once more.
pub fn rule_covers(rule: &str, p: &Permission) -> bool {
    let Some((tool, content)) = rule.strip_suffix(')').and_then(|r| r.split_once('(')) else { return rule == p.tool };
    if tool != p.tool {
        return false;
    }
    let field = ["command", "file_path", "notebook_path", "path"].iter()
        .find_map(|k| p.input.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty())).unwrap_or("");
    if let Some(prefix) = content.strip_suffix(":*") {
        let boundary = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || ";&|()`$".contains(c));
        return field.match_indices(prefix).any(|(i, _)| {
            boundary(field[..i].chars().next_back()) && boundary(field[i + prefix.len()..].chars().next())
        });
    }
    if let Some(dir) = content.strip_suffix("/**") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let dir = dir.strip_prefix("~/").map_or_else(|| PathBuf::from(dir), |rest| home.join(rest));
        return resolved(Path::new(field)).starts_with(resolved(&dir));
    }
    field == content
}

/// Whether a task names a service only the owner's connectors reach. An identifier (`email_parser`) is one word.
pub fn names_connector(task: &str) -> bool {
    let lower = task.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
    words.iter().any(|w| CONNECTOR_WORDS.contains(w))
        || CONNECTOR_PHRASES.iter().any(|p| words.windows(2).any(|w| w.join(" ") == *p))
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
    /// Lines added to `log` that the shell has not printed yet; it takes them as it prints them (`log` keeps only
    /// the last LOG_LINES, and a long reply arrives at once).
    pub fresh: Vec<String>,
    pub dir: PathBuf,
    pub worktree: Option<PathBuf>,
    pub base: Option<String>,
    pub applied: bool,
    /// Files the job edited, relative to its working folder.
    pub touched: BTreeSet<String>,
    /// What the agent waits for the owner to allow, oldest first.
    pub asks: VecDeque<Permission>,
    /// Lines for the agent's stdin while the turn runs (Claude's answers to its permission requests); None closes it.
    input: Option<Sender<Option<String>>>,
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
            self.fresh.push(l.to_string());
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
    /// The running total each resumable conversation last reported, by agent and conversation id.
    usage_so_far: HashMap<(Agent, String), Tokens>,
    /// How far agents act without asking.
    pub mode: Mode,
    /// Rules that wait for the owner in every mode: ALWAYS_ASK and the owner's own.
    pub always_ask: Vec<String>,
    /// Rules the owner allowed for this session: answered at once, and given to every later Claude turn.
    pub allowed: BTreeSet<String>,
    /// Plain lines the ask call answered with `ROUTE: <route>` (a task, not a question): (ask job, route), for the
    /// shell to start.
    pub routed: VecDeque<(usize, String)>,
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
        let programs = Agent::all().into_iter().map(|a| (a, PathBuf::from(&a.spec().program))).collect();
        let mut always_ask: Vec<String> = ALWAYS_ASK.iter().map(|r| r.to_string()).collect();
        let own = fs::read_to_string(root.join("permissions.json")).ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        always_ask.extend(own.iter().filter_map(|v| v.get("ask")?.as_array()).flatten()
            .filter_map(|r| r.as_str()).map(str::to_string).filter(|r| !always_ask.contains(r)).collect::<Vec<_>>());
        Ok(Session { repo, git: project.is_some(), dir, jobs: Vec::new(), max_jobs: 3, backend: "codex-chatgpt".into(),
                     ringo_home: ringo_home(), pipeline: None, programs, hold: None, threads: HashMap::new(),
                     usage_so_far: HashMap::new(), mode: Mode::Ask, always_ask, allowed: BTreeSet::new(),
                     routed: VecDeque::new(), tx, rx })
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

    /// The program an agent runs: as set (tests point it at a fake), else its entry's (an agent added after the
    /// session opened).
    pub fn program(&self, agent: Agent) -> PathBuf {
        self.programs.get(&agent).cloned().unwrap_or_else(|| PathBuf::from(&agent.spec().program))
    }

    pub fn installed(&self, agent: Agent) -> bool {
        installed(&self.program(agent))
    }

    /// The read turn the agent is busy with: its conversation takes one turn at a time.
    pub fn busy(&self, agent: Agent) -> Option<usize> {
        self.jobs.iter().find(|j| j.running() && j.kind == (Kind::Agent { agent, write: false })).map(|j| j.id)
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
                             subs: Vec::new(), log: VecDeque::new(), fresh: Vec::new(), dir, worktree: None, base: None, applied: false,
                             touched: BTreeSet::new(), asks: VecDeque::new(), input: None, pid: None,
                             cancel: Arc::new(AtomicBool::new(false)) });
        Ok(id)
    }

    /// One agent turn. Read turns run in the repository with the agent's read-only setting and continue that
    /// agent's conversation; write turns start fresh in a private worktree from HEAD.
    pub fn launch_agent(&mut self, agent: Agent, write: bool, prompt: &str) -> Result<usize, String> {
        if write && !self.git {
            return Err(format!("@{}! edits in a private git worktree, so it needs a git project: start ringo inside one \
                                (or run git init here). @{} without ! works in any folder", agent.name(), agent.name()));
        }
        let (name, spec) = (agent.name(), agent.spec());
        if !self.installed(agent) {
            return Err(format!("{name} is not installed (no `{}` on PATH)", self.program(agent).display()));
        }
        if !spec.read && !write {
            return Err(format!("{name} has no verified read-only mode; use @{name}! for a write job in its own worktree"));
        }
        if write && self.mode == Mode::Read {
            return Err(format!("read mode: agents do not edit. /mode ask (or Shift-Tab) lets @{name}! edit in a worktree"));
        }
        if !write && let Some(busy) = self.busy(agent) {
            return Err(format!("{name} is busy with #{busy}; its conversation takes one turn at a time"));
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
        let mut cmd = Command::new(self.program(agent));
        let resume = if write { None } else { self.threads.get(&agent).cloned() };
        let sandbox = if write { "workspace-write" } else { "read-only" };
        let mut input = Some(prompt.to_string());
        let style = match spec.adapter {
            Adapter::Codex => {
                cmd.args(["exec"]);
                if let Some(thread) = &resume {
                    cmd.args(["resume", thread]).arg("-c").arg(format!("sandbox_mode=\"{sandbox}\""));
                } else if write && self.mode == Mode::Auto {
                    // Auto: Codex's own reviewer answers its approval requests (--approve-for-me implies the
                    // workspace-write sandbox), and the sandbox has the network, for git, gh and package installs.
                    // Verified with codex-cli 0.165.0: the sandbox alone has no network.
                    cmd.args(["--approve-for-me", "-c", "sandbox_workspace_write.network_access=true", "-C"]).arg(&cwd);
                } else {
                    cmd.args(["-s", sandbox, "-C"]).arg(&cwd);
                }
                cmd.args(["--json", "--skip-git-repo-check", "-c", "forced_login_method=\"chatgpt\"",
                          "-c", "model_provider=\"openai\""]);
                // Codex cannot stop to ask here, so the ask tool and its mode instructions go, and analytics (its
                // upload delays the exit by about 0.33 s). Plugins and apps (documents, browser and computer-use
                // skills, a computer-use tool that acts outside the sandbox) load only for a task that names a
                // connector, as Claude's do. A trivial read turn: 17,133 tokens a call before, 14,350 after
                // (codex-cli 0.165.0). The same settings work after `exec resume`.
                cmd.args(["-c", "features.default_mode_request_user_input=false", "-c",
                          "include_collaboration_mode_instructions=false", "-c", "analytics.enabled=false"]);
                if !names_connector(prompt) {
                    cmd.args(["-c", "features.plugins=false", "-c", "features.apps=false"]);
                }
                cmd.arg("-");
                Style::Codex
            }
            Adapter::Claude => {
                // The prompt goes in on stdin as one stream-json message, and stdin stays open until the result: a
                // permission request comes out as a control_request and Ringo writes the owner's answer back.
                cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"]);
                if self.mode != Mode::Read && names_connector(prompt) {
                    // The owner's connectors (MCP servers, plugins) load, but not their CLAUDE.md, memory, skills or
                    // main agent: 27.5k tokens a trivial turn instead of 57.8k with all of it (Claude Code 2.1.316).
                    // Without CLAUDE.md the owner's instruction to use Ringo cannot start Ringo again either.
                    cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1").env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
                    cmd.args(["--agent", "claude", "--disable-slash-commands"]);
                } else {
                    // --safe-mode drops the owner's CLAUDE.md, skills, plugins, hooks and MCP servers: one trivial
                    // turn measured 16.6k tokens instead of 50.2k, with the subscription login and built-in subagents
                    // kept. The repository's own instructions are appended instead.
                    cmd.arg("--safe-mode");
                }
                // Ask: Claude's default permission checks, every prompt sent to Ringo (`--permission-prompt-tool
                // stdio`, the Agent SDK's own channel; `--permission-prompts host` alone denies). Auto: Claude's
                // auto mode, whose review sends what it blocks repeatedly to Ringo too. Read: nothing may prompt.
                match self.mode {
                    Mode::Read => cmd.args(["--permission-prompts", "none", "--permission-mode", "default"]),
                    Mode::Ask => cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode",
                                           if write { "acceptEdits" } else { "default" }]),
                    Mode::Auto => cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode", "auto"]),
                };
                // Read turns run in the owner's own tree with no sandbox, so they never edit and never run commands (a
                // command could write files); deny rules win in every mode. Commands run in write turns' worktrees.
                // Tools that act outside the turn go in every mode: scheduling, notifications, remote triggers, other
                // projects, entering a worktree of the owner's repository, design sync, review reports and
                // multi-agent workflows. With a permission host Claude Code also offers AskUserQuestion,
                // EnterPlanMode and ExitPlanMode, which Ringo does not serve. A trivial turn (Claude Code 2.1.316):
                // read 16,547 tokens before, 11,984 after; write 17,133 before, 12,570 after.
                let mut deny: Vec<&str> = Vec::new();
                if !write {
                    deny.extend(["Bash", "Edit", "Write", "NotebookEdit"]);
                }
                deny.extend(OUT_OF_TURN_TOOLS);
                if self.mode != Mode::Read {
                    deny.extend(["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"]);
                }
                cmd.arg("--disallowed-tools").args(deny);
                if self.mode != Mode::Read {
                    cmd.arg("--settings").arg(serde_json::json!({"permissions": {"ask": self.always_ask}}).to_string());
                    if !self.allowed.is_empty() {
                        cmd.arg("--allowedTools").args(&self.allowed);
                    }
                }
                if let Some(session) = &resume {
                    cmd.args(["--resume", session]);
                }
                if let Some(doc) = ["CLAUDE.md", "AGENTS.md"].iter().map(|d| cwd.join(d)).find(|p| p.is_file()) {
                    cmd.arg("--append-system-prompt-file").arg(doc);
                }
                input = Some(format!("{}\n", serde_json::json!({"type": "user", "message": {"role": "user", "content": prompt}})));
                Style::Claude
            }
            Adapter::Agy => {
                // Measured with agy 1.4.11: print mode cannot ask, so a tool needing a permission it lacks ends the
                // turn with no reply; a deny rule instead returns an error the model sees and works around. Write turns
                // edit (accept-edits) but run no commands, MCP tools or browser actions, as claude's write turns run no
                // Bash; read turns cannot write files either. The rules last for this session only.
                // The prompt goes in on stdin as one stream-json message (verified with agy 1.4.12, which drops a
                // flag's prompt in this mode): no argument-length limit, and the process table never shows it.
                let deny = if write { r#"["command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]"# }
                           else { r#"["write_file(*)","command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]"# };
                cmd.args(["--input-format", "stream-json", "--output-format", "stream-json", "--settings"])
                    .arg(format!(r#"{{"permissions":{{"deny":{deny}}}}}"#));
                if write {
                    cmd.args(["--mode", "accept-edits"]);
                }
                if let Some(conversation) = &resume {
                    cmd.args(["--conversation", conversation]);
                }
                input = Some(format!("{}\n", serde_json::json!({"event": "user", "message": {"content": prompt}})));
                Style::Agy
            }
            Adapter::Plain => {
                // The entry's own command line, e.g. grok's documented headless flags (docs.x.ai/build/cli/
                // headless-scripting; unverified here). Grok's --always-approve runs every tool call unsandboxed
                // inside its worktree.
                for arg in &spec.args { // the prompt goes in last, so its own text is never filled in
                    cmd.arg(arg.replace("{cwd}", &cwd.to_string_lossy()).replace("{prompt}", prompt));
                }
                if spec.args.iter().any(|a| a.contains("{prompt}")) {
                    input = None;
                }
                Style::Plain
            }
        };
        cmd.current_dir(&cwd);
        agent_env(&mut cmd);
        self.spawn(id, cmd, style, input, None)?;
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
    /// an agent turn costs 17,000 before it starts. A line that is a task (files to change, review or read, or live
    /// sources) is answered with one `ROUTE: <route>` line, which `pump` hands to the shell to start.
    pub fn ask(&mut self, question: &str) -> Result<usize, String> {
        const KEEP: usize = 4; // earlier exchanges carried as context
        const ROOM: usize = 12_000; // characters of them at most, the newest kept first
        let mut earlier: Vec<String> = Vec::new();
        let mut used = 0;
        for job in self.jobs.iter().rev().filter(|j| matches!(&j.kind, Kind::Ringo { command } if command == "ask") && j.status == Status::Done
                                                     && !j.reply().trim_start().starts_with("ROUTE:")) {
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
        // The stdin text, then for Claude the owner's answers until the turn's result; None closes the pipe.
        let (lines, queued) = channel::<Option<String>>();
        let keep_open = style == Style::Claude;
        let mut closer = None;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = lines.send(Some(text));
            if keep_open {
                self.jobs[id - 1].input = Some(lines.clone());
                closer = Some(lines);
            } else {
                let _ = lines.send(None);
            }
            thread::spawn(move || {
                while let Ok(Some(line)) = queued.recv() {
                    if pipe.write_all(line.as_bytes()).and_then(|_| pipe.flush()).is_err() {
                        break;
                    }
                } // dropping the pipe closes it
            });
        }
        let (tx, cancel, pid) = (self.tx.clone(), self.jobs[id - 1].cancel.clone(), child.id());
        let stdout = child.stdout.take();
        thread::spawn(move || {
            let mut parser = Parser::new(style);
            // An agent's raw stream; a pipeline's would only copy its trace.jsonl.
            let mut events = (style != Style::Trace).then(|| fs::OpenOptions::new().create(true).append(true)
                .open(dir.join("events.jsonl")).ok()).flatten();
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
                if parser.completed() && let Some(close) = closer.take() {
                    let _ = close.send(None); // the result is in: nothing more will be asked, so the agent may exit
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
        job.asks.clear(); // nobody is waiting for these answers any more
        job.input = None;
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
                    if matches!(&job.kind, Kind::Ringo { command } if command == "ask") && status == Status::Done {
                        // The ask prompt answers a task with one line, `ROUTE: edit|read|review|research`.
                        if let Some(route) = job.reply().trim().strip_prefix("ROUTE:") {
                            // the route word alone: "ROUTE: edit." or a line of reasons after it still starts the task
                            let word = route.split(|c: char| !c.is_ascii_alphabetic()).find(|w| !w.is_empty());
                            self.routed.push_back((id, word.unwrap_or("").to_lowercase()));
                        }
                    }
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
                Event::Note(id, Note::Permission(p)) => {
                    if self.allowed.contains(&p.rule) && !self.always_asks(&p) {
                        self.answer(id, &p, true, "allowed (this session)");
                    } else {
                        // The whole request in the job's log too, where a long one can be read before answering.
                        self.jobs[id - 1].push_log(format!("asks before it acts: {}", p.what));
                        self.jobs[id - 1].asks.push_back(p);
                    }
                }
                Event::Note(id, Note::Unserved(request)) => {
                    let line = serde_json::json!({"type": "control_response", "response": {"subtype": "error",
                        "request_id": request, "error": "Ringo does not serve this request"}});
                    if let Some(input) = &self.jobs[id - 1].input {
                        let _ = input.send(Some(format!("{line}\n")));
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
                        Note::Tokens(t) => {
                            // A resumed Codex or agy turn reports its conversation's usage so far (Codex 16,829
                            // then 34,835 input tokens for a one-word follow-up; agy 6,981 then 14,260), so only
                            // what this turn added is counted. Claude reports each run on its own. A running total
                            // never falls, so a result without usage (zero) keeps the last one.
                            let t = match job.kind {
                                Kind::Agent { agent, write: false }
                                    if matches!(agent.spec().adapter, Adapter::Codex | Adapter::Agy) =>
                                {
                                    let key = (agent, self.threads.get(&agent).cloned().unwrap_or_default());
                                    let before = self.usage_so_far.get(&key).copied().unwrap_or_default();
                                    if t.total() >= before.total() {
                                        self.usage_so_far.insert(key, t);
                                    }
                                    t.since(before)
                                }
                                _ => t,
                            };
                            job.tokens.add(t)
                        }
                        Note::Call => job.calls += 1,
                        Note::Touched(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path);
                            job.touched.insert(rel.to_string_lossy().into_owned());
                        }
                        Note::Fail(reason) => job.push_log(format!("stopping: {reason}")),
                        Note::Permission(_) | Note::Unserved(_) => {} // handled above
                    }
                }
            }
        }
        changed
    }

    /// The oldest action an agent waits on the owner for, across every job: (job, what it asks).
    pub fn pending(&self) -> Option<(usize, &Permission)> {
        self.jobs.iter().find_map(|j| j.asks.front().map(|p| (j.id, p)))
    }

    /// How many actions wait on the owner, across every job.
    pub fn waiting(&self) -> usize {
        self.jobs.iter().map(|j| j.asks.len()).sum()
    }

    /// The owner's answer to job `id`'s oldest waiting action. "This session" adds its rule to the session's allowed
    /// rules and answers every other waiting action with the same rule too.
    pub fn decide(&mut self, id: usize, decision: Decision) -> Result<String, String> {
        let p = self.jobs.get_mut(id.wrapping_sub(1)).and_then(|j| j.asks.pop_front())
            .ok_or(format!("#{id} is not waiting for an answer"))?;
        // An always-ask action is allowed one at a time: "this session" counts as once for it.
        let decision = if decision == Decision::Session && self.always_asks(&p) { Decision::Once } else { decision };
        let said = match decision {
            Decision::Once => "you allowed",
            Decision::Session => "you allowed for this session",
            Decision::Deny => "you declined",
        };
        self.answer(id, &p, decision != Decision::Deny, said);
        if decision == Decision::Session {
            self.allowed.insert(p.rule.clone());
            for job in 1..=self.jobs.len() {
                let same: Vec<Permission> = self.jobs[job - 1].asks.iter()
                    .filter(|q| q.rule == p.rule && !self.always_asks(q)).cloned().collect();
                self.jobs[job - 1].asks.retain(|q| !same.contains(q));
                for q in same {
                    self.answer(job, &q, true, "allowed (this session)");
                }
            }
        }
        Ok(format!("{said}: {}", parse::short(&p.what, 120)))
    }

    /// Whether an always-ask rule covers this action: such an action reaches the owner every time.
    pub fn always_asks(&self, p: &Permission) -> bool {
        self.always_ask.iter().any(|rule| rule_covers(rule, p))
    }

    /// Writes an answer to the agent, logs it in the job, and keeps it in the session's action log.
    fn answer(&mut self, id: usize, p: &Permission, allow: bool, said: &str) {
        let response = if allow {
            serde_json::json!({"behavior": "allow", "updatedInput": p.input})
        } else {
            serde_json::json!({"behavior": "deny", "message": "The owner declined this action."})
        };
        let line = serde_json::json!({"type": "control_response", "response": {"subtype": "success", "request_id": p.id,
                                      "response": response}});
        let job = &mut self.jobs[id - 1];
        if let Some(input) = &job.input {
            let _ = input.send(Some(format!("{line}\n")));
        }
        job.push_log(format!("{said}: {}", p.what));
        if job.asks.is_empty() {
            job.activity = if allow { "working".into() } else { "declined; carrying on".into() };
        }
        let who = match job.kind { Kind::Agent { agent, .. } => agent.name().to_string(), Kind::Ringo { .. } => "ringo".into() };
        let record = serde_json::json!({"job": id, "agent": who, "tool": p.tool, "action": p.what, "rule": p.rule,
                                        "answer": said, "mode": self.mode.name()});
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(self.dir.join("actions.jsonl")) {
            let _ = writeln!(f, "{record}");
        }
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
            // Only `{#` digits `}` is a handoff; anything else (a template comment `{# note #}`) stays as written.
            let tail = &rest[start + 2..];
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || !tail[digits..].starts_with('}') {
                out.push_str(&rest[..start + 2]);
                rest = tail;
                continue;
            }
            let n: usize = tail[..digits].parse().map_err(|_| "write {#N} with a job number")?;
            let job = self.job(n)?;
            if job.status != Status::Done {
                return Err(format!("#{n} has not finished successfully, so it cannot be handed off"));
            }
            out.push_str(&rest[..start]);
            out.push_str(&format!("\n--- reply of job #{n} ---\n{}\n", job.reply().trim_end()));
            if let (Some(wt), Some(base)) = (&job.worktree, &job.base) {
                // The change as text: a binary file is one "Binary files ... differ" line, not base85 the agent
                // cannot read (a 20 KB binary file was 19,546 tokens of it). /apply keeps the binary patch.
                git(wt, &["add", "-A"])?;
                let patch = git_raw(wt, &["diff", "--cached", base])?;
                out.push_str(&format!("--- change made by job #{n} ---\n{}\n", String::from_utf8_lossy(&patch)));
            }
            out.push_str("---\n");
            rest = &tail[digits + 1..];
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
        // Let the last signal land before the process exits: on Unix only until every group is gone (most go within a
        // millisecond of TERM), at most the 3.1 s the TERM, INT and KILL steps take.
        let groups: Vec<u32> = self.jobs.iter().filter(|j| j.running()).filter_map(|j| j.pid).collect();
        let deadline = Instant::now() + Duration::from_millis(3100);
        while !groups.is_empty() && Instant::now() < deadline {
            #[cfg(unix)]
            {
                // SAFETY: signal 0 only checks whether the group exists. EPERM: only zombies are left.
                if groups.iter().all(|&g| unsafe { libc::killpg(g as libc::pid_t, 0) } != 0) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(15));
        }
    }
}

// --- Agents the owner has set up, and their sign-in -------------------------------------------------------------

fn agents_file(root: &Path) -> PathBuf {
    root.join("agents.json")
}

/// The owner's agents.json: `agents` (the ones set up) and `custom` (entries they added), or an empty one.
fn read_agents_file(root: &Path) -> serde_json::Value {
    fs::read(agents_file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({}))
}

/// Runs a read-modify-write of agents.json holding an exclusive lock, so two shells saving at once cannot drop each
/// other's change (std's File::lock: flock on Unix, LockFileEx on Windows).
fn with_agents_lock<T>(root: &Path, change: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    private_dir(root)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(root.join("agents.json.lock"))
        .map_err(|e| format!("agents.json.lock: {e}"))?;
    lock.lock().map_err(|e| format!("agents.json.lock: {e}"))?; // released when `lock` closes
    let result = change();
    drop(lock);
    result
}

/// Written whole and renamed into place, so a crash or a second shell cannot leave half a file.
fn write_agents_file(root: &Path, value: &serde_json::Value) -> Result<(), String> {
    private_dir(root)?;
    let (path, tmp) = (agents_file(root), root.join(format!("agents.json.tmp{}", std::process::id())));
    fs::write(&tmp, format!("{value:#}\n")).and_then(|_| fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Registers the owner's own entries from `root`/agents.json; the reason for each one that cannot run.
pub fn load_custom_agents(root: &Path) -> Vec<String> {
    let file = read_agents_file(root);
    let mut problems = Vec::new();
    for entry in file.get("custom").and_then(|c| c.as_array()).into_iter().flatten() {
        match Spec::from_json(entry, true) {
            Ok(spec) if Agent::from_name(&spec.name).is_some_and(|a| a.spec() == &spec) => {} // loaded already
            Ok(spec) => {
                if let Err(e) = Agent::register(spec) {
                    problems.push(e);
                }
            }
            Err(e) => problems.push(e),
        }
    }
    problems
}

/// Adds the owner's entry for an agent Ringo does not ship, kept in `root`/agents.json for next time.
pub fn add_custom_agent(root: &Path, entry: serde_json::Value) -> Result<Agent, String> {
    let spec = Spec::from_json(&entry, true)?;
    if Agent::from_name(&spec.name).is_some() {
        return Err(format!("Ringo already has an agent called {}", spec.name));
    }
    with_agents_lock(root, || {
        let mut file = read_agents_file(root);
        if !file["custom"].is_array() {
            file["custom"] = serde_json::json!([]);
        }
        file["custom"].as_array_mut().unwrap().push(entry);
        write_agents_file(root, &file)
    })?;
    Agent::register(spec)
}

/// The agents set up with `ringo setup` (state_root()/agents.json), the owner's own entries registered first;
/// None before the first setup.
pub fn configured_agents() -> Option<Vec<Agent>> {
    let root = state_root();
    load_custom_agents(&root);
    let file = read_agents_file(&root);
    let agents: Vec<Agent> = file.get("agents")?.as_array()?.iter()
        .filter_map(|a| a.as_str().and_then(Agent::from_name)).collect();
    Some(agents)
}

/// Saves which agents are set up; the owner's own entries are kept as they are.
pub fn save_agents(agents: &[Agent]) -> Result<(), String> {
    let root = state_root();
    with_agents_lock(&root, || {
        let mut file = read_agents_file(&root);
        file["agents"] = agents.iter().map(|a| a.name()).collect::<Vec<_>>().into();
        write_agents_file(&root, &file)
    })
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
    let adapter = agent.spec().adapter;
    let args: &[&str] = match adapter {
        Adapter::Codex => &["login", "status"],
        Adapter::Claude => &["auth", "status", "--json"],
        Adapter::Agy | Adapter::Plain => return SignIn::Unknown,
    };
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    agent_env(&mut cmd);
    let Ok(out) = cmd.output() else { return SignIn::No("its status command did not run".into()) };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    match adapter {
        Adapter::Codex if out.status.success() && text.contains("ChatGPT") => SignIn::Yes("ChatGPT".into()),
        Adapter::Codex if out.status.success() => SignIn::No(format!("{} (Ringo uses the ChatGPT login)", text.trim())),
        Adapter::Claude => {
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

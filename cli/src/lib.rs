//! Ringo's conductor: launches agent turns and Ringo pipelines as background jobs, follows their events, and keeps
//! agent edits in private git worktrees until the owner applies them. The terminal shell (`ringo`) and, later,
//! the IDE drive a `Session` and render its jobs.

pub mod parse;
pub mod rt;

use parse::{Note, Parser, Permission, Style, Tokens};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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

/// How hard agents think, and whether Ringo checks their change (`/tier`). The shell starts in Standard every time,
/// so a tier that spends more is never left on by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Low reasoning effort: fewer tokens, for small or plain tasks.
    Quick,
    /// Each agent's own default (and the owner's settings): what a turn has always been.
    Standard,
    /// High reasoning effort.
    Deep,
    /// High effort, then Ringo reviews each change a turn makes here and the agent gets one revision turn when the
    /// review finds defects: the one extra spend Ringo measured to raise quality (3 of 11 to 11 of 11 on its seeded
    /// task). The shell drives the review and the revision.
    Ultra,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Quick, Tier::Standard, Tier::Deep, Tier::Ultra];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Quick => "quick",
            Tier::Standard => "standard",
            Tier::Deep => "deep",
            Tier::Ultra => "ultra",
        }
    }

    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The reasoning effort agents run at; None leaves each agent's own.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Tier::Quick => Some("low"),
            Tier::Standard => None,
            Tier::Deep | Tier::Ultra => Some("high"),
        }
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
    /// What the running turn has used so far by its own messages' counts, until its final count is in `tokens`.
    pub live: Tokens,
    pub calls: u32,
    pub subs: Vec<Sub>,
    pub log: VecDeque<String>,
    /// What the job added to `log` that the shell has not printed yet; it takes these as it prints them (`log` keeps
    /// only the last LOG_LINES, and a long reply arrives at once).
    pub fresh: Vec<Said>,
    pub dir: PathBuf,
    pub worktree: Option<PathBuf>,
    pub base: Option<String>,
    pub applied: bool,
    /// A write job that finished without changing anything (an empty diff; its worktree is kept: a file git ignores,
    /// which the agent may have written, is not in the diff).
    pub unchanged: bool,
    /// Files the job edited, relative to its working folder.
    pub touched: BTreeSet<String>,
    /// A turn in the owner's folder that could edit it: the folder's files as git trees before and after it (see
    /// `snapshot`), the files that differ, and whether /undo put them back.
    pub before: Option<String>,
    pub after: Option<String>,
    pub changed: Vec<String>,
    pub undone: bool,
    /// What the agent waits for the owner to allow, oldest first.
    pub asks: VecDeque<Permission>,
    /// Lines for the agent's stdin while the turn runs (Claude's answers to its permission requests); None closes it.
    input: Option<Sender<Option<String>>>,
    pid: Option<u32>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    /// Tokens used so far: the final counts, plus what a running (or stopped) turn's messages reported.
    pub fn tokens_so_far(&self) -> u64 {
        self.tokens.total() + self.live.total()
    }

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
            self.push(Said::Line(l.to_string()));
        }
    }

    fn push(&mut self, said: Said) {
        let (Said::Reply(text) | Said::Step(text) | Said::Line(text)) = &said else {
            self.fresh.push(said); // a piece of the reply: the log takes the whole reply when it comes
            return;
        };
        for l in text.lines() {
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(l.to_string());
        }
        self.fresh.push(said);
    }
}

/// Something a job adds to the conversation.
#[derive(Clone, Debug, PartialEq)]
pub enum Said {
    /// The agent's reply, as markdown.
    /// A piece of the reply as it is written; the Reply that follows replaces the pieces.
    Partial(String),
    Reply(String),
    /// A step it took (a tool call or a command) and what came of it.
    Step(String),
    /// A line shown as it is: a plain agent's output, or Ringo's note about the job.
    Line(String),
}

pub enum Event {
    Note(usize, Note),
    Ended(usize, Status),
}

pub struct Session {
    pub repo: PathBuf,
    /// Whether `repo` is a git project; outside one, agents can read and answer but not edit in a worktree.
    pub git: bool,
    /// The project's own index file and the branch it is on, read once when the session opens.
    index: Option<PathBuf>,
    pub branch: Option<String>,
    /// Outside a git project: Ringo's own undo store for the folder (see `undo_store`).
    store: Option<PathBuf>,
    /// Why /undo is off here, when it is.
    pub no_undo: Option<String>,
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
    /// How hard they think (see `Tier`).
    pub tier: Tier,
    /// Ringo's state folder, where `usage.jsonl` keeps what each finished job used.
    root: PathBuf,
    /// The last rate-limit event a Claude turn reported here: /usage's fallback when Claude's own answer fails.
    pub claude_limits: Option<serde_json::Value>,
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

/// The git command's stdout exactly as written (a patch must stay byte-for-byte intact).
fn run_git(mut cmd: Command, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = cmd.args(args).output().map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

fn trimmed(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).trim_end().to_string()
}

fn git_in(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run_git(git_in(dir), args)
}

/// git's stdout as a trimmed value (an id, a path, a status).
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_raw(dir, args).map(trimmed)
}

/// `git` with a private index file in place of the repository's own.
fn git_index(dir: &Path, index: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = git_in(dir);
    cmd.env("GIT_INDEX_FILE", index);
    run_git(cmd, args).map(trimmed)
}

/// What Ringo's undo store ignores, past the folder's own .gitignore files: tools' caches and installed packages.
const STORE_IGNORES: &[&str] = &["node_modules", ".venv", "venv", "__pycache__", "*.pyc", ".DS_Store", ".gradle"];
/// The most a folder outside git may hold for Ringo's undo store, past what it ignores: each turn's start and end
/// hash every new or changed file in it.
const STORE_FILES: u64 = 5_000;
const STORE_BYTES: u64 = 200 << 20;

/// Ok when `dir` suits the undo store: at most `files` files and `bytes` bytes, not counting what the store ignores,
/// and no git project inside it (git would keep only a link to its commit, not the files a turn edits there). Stops
/// at the first limit it passes; symlinks are not followed.
fn fits_store(dir: &Path, files: u64, bytes: u64) -> Result<(), String> {
    let ignored = |name: &str| STORE_IGNORES.iter().any(|i| match i.strip_prefix('*') {
        Some(ext) => name.ends_with(ext),
        None => name == *i,
    });
    let (mut seen, mut size, mut todo) = (0, 0, vec![dir.to_path_buf()]);
    while let Some(d) = todo.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let (Ok(kind), name) = (entry.file_type(), entry.file_name()) else { continue };
            if name == ".git" {
                let inner = d.strip_prefix(dir).unwrap_or(&d).display().to_string();
                return Err(format!("{inner} is a git project of its own; start ringo in it, where /undo works"));
            }
            if ignored(&name.to_string_lossy()) {
                continue;
            }
            if kind.is_dir() {
                todo.push(entry.path());
            } else if kind.is_file() {
                seen += 1;
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if seen > files || size > bytes {
                    return Err(format!("this folder holds more than {files} files or {} MB; run `git init` here once \
                                        to turn /undo on", bytes >> 20));
                }
            }
        }
    }
    Ok(())
}

/// Outside a git project, each turn's before and after for /undo live in a git store of Ringo's own, one for each
/// folder, under its state folder (`root`/undo); the folder itself gets no repository. Not for the home folder or
/// one holding it or Ringo's state, and not for a folder too big to hash at every turn.
// ceiling: a store is never pruned, and a folder moved or renamed starts a new one; delete `root`/undo to reclaim it.
fn undo_store(root: &Path, folder: &Path, home: &Path) -> Result<PathBuf, String> {
    if folder.parent().is_none() || (!home.as_os_str().is_empty() && home.starts_with(folder)) {
        return Err("Ringo keeps no /undo for your whole home folder; start ringo in the folder you work on".into());
    }
    let root = resolved(root); // the store is used from the folder: a relative root would point elsewhere there
    if root.starts_with(folder) {
        return Err("Ringo's own state is inside this folder".into());
    }
    fits_store(folder, STORE_FILES, STORE_BYTES)?;
    let hash = folder.as_os_str().as_encoded_bytes().iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)); // FNV-1a
    let store = root.join("undo").join(format!("{hash:016x}"));
    if !store.join("HEAD").is_file() {
        // Made beside it and renamed into place, so two sessions opening the folder at once never meet halfway.
        let new = store.with_extension(format!("new-{}-{:?}", std::process::id(), thread::current().id()));
        private_dir(&new)?;
        git(&new, &["init", "--bare", "-q", "."])?;
        let ignores: String = STORE_IGNORES.iter().map(|i| format!("{i}\n")).collect();
        fs::write(new.join("info").join("exclude"), ignores).map_err(|e| e.to_string())?;
        fs::write(new.join("folder"), folder.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        if fs::rename(&new, &store).is_err() {
            let _ = fs::remove_dir_all(&new); // another session's store is there first
            if !store.join("HEAD").is_file() {
                return Err(format!("cannot make the undo store {}", store.display()));
            }
        }
    }
    Ok(store)
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
        // One git call: the project's top, its index file and its HEAD file (paths relative to `cwd`)
        let found = git(cwd, &["rev-parse", "--show-toplevel", "--git-path", "index", "--git-path", "HEAD"]).unwrap_or_default();
        let mut found = found.lines();
        let (top, index, head) = (found.next().map(|t| resolved(Path::new(t))), found.next(), found.next());
        let project = top.filter(|t| home.as_os_str().is_empty() || !home.starts_with(t));
        let repo = project.clone().unwrap_or_else(|| resolved(cwd));
        let index = project.as_ref().and(index).map(|i| cwd.join(i));
        // `ref: refs/heads/NAME` on a branch; a commit id when detached, shown as HEAD as git does
        let branch = project.as_ref().and(head).map(|h| match fs::read_to_string(cwd.join(h)) {
            Ok(text) => text.trim().strip_prefix("ref: refs/heads/").unwrap_or("HEAD").to_string(),
            Err(_) => "HEAD".into(),
        });
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
        let (store, no_undo) = match project {
            Some(_) => (None, None),
            None => match undo_store(root, &repo, &home) {
                Ok(store) => (Some(store), None),
                Err(why) => (None, Some(why)),
            },
        };
        Ok(Session { repo, git: project.is_some(), index, branch, store, no_undo, dir, jobs: Vec::new(), max_jobs: 3,
                     backend: "codex-chatgpt".into(),
                     ringo_home: ringo_home(), pipeline: None, programs, hold: None, threads: HashMap::new(),
                     usage_so_far: HashMap::new(), mode: Mode::Ask, tier: Tier::Standard, root: resolved(root), claude_limits: None, always_ask, allowed: BTreeSet::new(),
                     routed: VecDeque::new(), tx, rx })
    }

    pub fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.running()).count()
    }

    pub fn tokens(&self) -> u64 {
        self.jobs.iter().map(Job::tokens_so_far).sum()
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

    /// The agent turn running in the owner's folder, if one is: one at a time, since it may edit the folder and
    /// continues its agent's conversation.
    pub fn busy(&self) -> Option<usize> {
        self.jobs.iter().find(|j| j.running() && matches!(j.kind, Kind::Agent { write: false, .. })).map(|j| j.id)
    }

    /// Whether the agent has a conversation to continue (a turn of it in the folder has started one).
    pub fn has_conversation(&self, agent: Agent) -> bool {
        self.threads.contains_key(&agent)
    }

    /// A new conversation: the next turn of each agent starts fresh instead of resuming (`/clear`).
    pub fn forget(&mut self) {
        self.threads.clear();
    }

    /// The folder's files as they are now, as a git tree: every file git does not ignore, untracked ones too. A
    /// private copy of the index (`index`) stages them, so the owner's index, stash and HEAD are never touched.
    /// ceiling: hashes each new or changed file every call; a large file git does not ignore slows a turn's start.
    fn snapshot(&self, index: &Path) -> Result<String, String> {
        if self.store.is_some() {
            // the store keeps its own index: each file's stat stays in it, so only new or changed files are hashed
            self.folder_git(&["add", "-A"])?;
            return self.folder_git(&["write-tree"]);
        }
        let own = self.index.clone().ok_or("not a git project")?;
        match own.is_file() {
            true => fs::copy(&own, index).map(|_| ()).map_err(|e| format!("{}: {e}", own.display()))?,
            false => { let _ = fs::remove_file(index); } // a repository with nothing staged yet has no index
        }
        git_index(&self.repo, index, &["add", "-A"])?;
        git_index(&self.repo, index, &["write-tree"])
    }

    /// git on the owner's folder as /undo keeps it: the project's repository, or outside one Ringo's undo store,
    /// run from the folder's top (a patch applied from below it would skip the files outside it).
    fn folder(&self) -> Command {
        match &self.store {
            Some(store) => {
                let mut cmd = Command::new("git");
                cmd.arg("--git-dir").arg(store).arg("--work-tree").arg(&self.repo).current_dir(&self.repo);
                cmd
            }
            None => git_in(&self.repo),
        }
    }

    fn folder_git(&self, args: &[&str]) -> Result<String, String> {
        run_git(self.folder(), args).map(trimmed)
    }

    /// Whether /undo works here: a git project, or Ringo's own store for the folder.
    pub fn undoable(&self) -> bool {
        self.git || self.store.is_some()
    }

    /// A commit holding `tree`, on `parent` when there is one; no branch points at it.
    fn commit(&self, tree: &str, parent: Option<&str>, message: &str) -> Result<String, String> {
        let mut args = vec!["-c", "user.name=ringo", "-c", "user.email=ringo@localhost", "commit-tree", "--no-gpg-sign",
                            tree, "-m", message];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        git(&self.repo, &args)
    }

    /// Refuses a change to the owner's folder while an agent turn works in it: the turn's own after would take it in.
    fn idle(&self, what: &str) -> Result<(), String> {
        match self.busy() {
            Some(id) => Err(format!("#{id} is working in this folder: {what} when it ends (Esc stops it)")),
            None => Ok(()),
        }
    }

    /// `/undo`: the files the latest agent turn in this folder changed go back to how they were before it (each
    /// /undo goes one turn further back). Any of them edited since that turn, anywhere in it, makes it refuse,
    /// leaving everything as is.
    pub fn undo(&mut self) -> Result<String, String> {
        if let Some(why) = &self.no_undo {
            return Err(format!("/undo is off here: {why}"));
        }
        self.idle("/undo")?;
        let job = self.jobs.iter().rev()
            .find(|j| !j.running() && !j.undone && j.after.is_some() && !j.changed.is_empty())
            .ok_or("nothing to undo: no agent turn has changed files here yet")?;
        let (id, before, after) = (job.id, job.before.clone().unwrap_or_default(), job.after.clone().unwrap_or_default());
        let (file, index, changed) = (job.dir.join("undo.patch"), job.dir.join("index"), job.changed.clone());
        // git apply alone would also take back a turn's lines in a file edited elsewhere since: compare first
        let now = self.snapshot(&index)?;
        let mut args = vec!["diff", "--name-only", after.as_str(), now.as_str(), "--"];
        args.extend(changed.iter().map(String::as_str));
        let edited = self.folder_git(&args)?;
        if !edited.is_empty() {
            return Err(format!("{} changed after #{id}, so nothing was undone", edited.lines().collect::<Vec<_>>().join(", ")));
        }
        fs::write(&file, run_git(self.folder(), &["diff", "--binary", &after, &before])?).map_err(|e| e.to_string())?;
        let file = file.to_string_lossy().into_owned();
        self.folder_git(&["apply", "--check", &file])
            .map_err(|e| format!("#{id}'s files no longer match it, so nothing was undone ({e})"))?;
        self.folder_git(&["apply", &file])?;
        let job = &mut self.jobs[id - 1];
        job.undone = true;
        Ok(format!("undid #{id}: {} back as before", job.changed.join(", ")))
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
                             elapsed: None, activity: "starting".into(), tokens: Tokens::default(), live: Tokens::default(), calls: 0,
                             subs: Vec::new(), log: VecDeque::new(), fresh: Vec::new(), dir, worktree: None, base: None, applied: false, unchanged: false,
                             touched: BTreeSet::new(), before: None, after: None, changed: Vec::new(), undone: false,
                             asks: VecDeque::new(), input: None, pid: None,
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
            return Err(format!("{name} works only in its own copy of the folder: use @{name}!"));
        }
        if write && self.mode == Mode::Read {
            return Err(format!("read mode: agents do not edit. /mode ask (or Shift-Tab) lets @{name}! edit its own copy"));
        }
        if !write && let Some(busy) = self.busy() {
            return Err(format!("busy: #{busy} is working in this folder; one turn at a time"));
        }
        // A turn in the owner's folder edits it, unless in read mode; a write turn edits its own copy.
        let edits = write || self.mode != Mode::Read;
        let id = self.new_job(Kind::Agent { agent, write }, prompt)?;
        let dir = self.jobs[id - 1].dir.clone();
        let _ = fs::write(dir.join("prompt.txt"), prompt);
        let cwd = if write {
            // The copy starts from the folder as it is now, uncommitted changes included, so its change applies
            // back onto it.
            let wt = dir.join("wt");
            let head = git(&self.repo, &["rev-parse", "--verify", "-q", "HEAD"]).ok();
            let base = self.snapshot(&dir.join("index"))
                .and_then(|tree| self.commit(&tree, head.as_deref(), &format!("ringo job {id}: the folder as it began")))
                .and_then(|base| git(&self.repo, &["worktree", "add", "--detach", &wt.to_string_lossy(), &base]).map(|_| base));
            let base = match base {
                Ok(base) => base,
                Err(e) => {
                    self.end(id, Status::Failed(e.clone()));
                    return Err(e);
                }
            };
            let job = &mut self.jobs[id - 1];
            job.worktree = Some(wt.clone());
            job.base = Some(base);
            job.push_log(format!("its own copy of this folder as it is now (/show {id}: where)"));
            wt
        } else {
            if edits && self.undoable() {
                // Kept for /undo; a turn whose before cannot be taken still runs, without an undo. Taken anew even
                // right behind another turn: an /undo or an edit of yours in between belongs to neither.
                match self.snapshot(&dir.join("index")) {
                    Ok(tree) => self.jobs[id - 1].before = Some(tree),
                    Err(e) => self.jobs[id - 1].push_log(format!("no /undo for this turn: {e}")),
                }
            }
            self.repo.clone()
        };
        let mut cmd = Command::new(self.program(agent));
        let resume = if write { None } else { self.threads.get(&agent).cloned() };
        let sandbox = if edits { "workspace-write" } else { "read-only" };
        let (mut input, mut turn) = (Some(prompt.to_string()), None);
        let style = match spec.adapter {
            Adapter::Codex => {
                // One `codex app-server` per turn, so the reply streams as it is written: the same agent and settings
                // as `codex exec`, and the same model input, compared request for request against a local stand-in
                // for the API (codex-cli 0.165.0: a new turn in each mode, and a resumed one). The thread's settings
                // go in thread/start or thread/resume; turn/start follows once the thread's id is known.
                cmd.arg("app-server");
                let auto = edits && self.mode == Mode::Auto;
                if edits {
                    // Auto: the sandbox has the network, for git, gh and package installs, and Codex's own reviewer
                    // answers its approval requests. Ask: no network, said outright, whatever an earlier turn had.
                    cmd.args(["-c", &format!("sandbox_workspace_write.network_access={auto}")]);
                }
                cmd.args(["-c", "forced_login_method=\"chatgpt\"", "-c", "model_provider=\"openai\""]);
                // Codex cannot stop to ask here, so the ask tool and its mode instructions go, and analytics (its
                // upload delays the exit by about 0.33 s). Plugins and apps (documents, browser and computer-use
                // skills, a computer-use tool that acts outside the sandbox) load only for a task that names a
                // connector, as Claude's do. A trivial read turn: 17,133 tokens a call before, 14,350 after
                // (codex-cli 0.165.0).
                cmd.args(["-c", "features.default_mode_request_user_input=false", "-c",
                          "include_collaboration_mode_instructions=false", "-c", "analytics.enabled=false"]);
                if !names_connector(prompt) {
                    cmd.args(["-c", "features.plugins=false", "-c", "features.apps=false"]);
                }
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
                }
                let mut thread = serde_json::json!({"cwd": cwd, "sandbox": sandbox,
                                                    "approvalPolicy": if auto { "on-request" } else { "never" }});
                if auto {
                    thread["approvalsReviewer"] = "auto_review".into();
                }
                let method = match &resume {
                    Some(id) => {
                        thread["threadId"] = id.clone().into();
                        thread["excludeTurns"] = true.into(); // the answer leaves out the conversation so far
                        "thread/resume"
                    }
                    None => "thread/start",
                };
                let hello = serde_json::json!({"id": 1, "method": "initialize",
                                               "params": {"clientInfo": {"name": "ringo", "version": env!("CARGO_PKG_VERSION")}}});
                input = Some(format!("{hello}\n{}\n{}\n", serde_json::json!({"method": "initialized"}),
                                     serde_json::json!({"id": 2, "method": method, "params": thread})));
                turn = Some(serde_json::json!({"id": 3, "method": "turn/start",
                                               "params": {"input": [{"type": "text", "text": prompt}]}}));
                Style::Codex
            }
            Adapter::Claude => {
                // The prompt goes in on stdin as one stream-json message, and stdin stays open until the result: a
                // permission request comes out as a control_request and Ringo writes the owner's answer back.
                // The reply streams as it is written (--include-partial-messages); it changes no tokens.
                cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                          "--include-partial-messages"]);
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
                    Mode::Ask => cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode", "acceptEdits"]),
                    Mode::Auto => cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode", "auto"]),
                };
                // A turn in read mode runs in the owner's own tree with no sandbox, so it never edits and never runs
                // commands (a command could write files); deny rules win in every mode. Otherwise edits in its folder
                // go through (/undo puts them back) and a command asks first, as Claude Code's accept-edits mode does.
                // Tools that act outside the turn go in every mode: scheduling, notifications, remote triggers, other
                // projects, entering a worktree of the owner's repository, design sync, review reports and
                // multi-agent workflows. With a permission host Claude Code also offers AskUserQuestion,
                // EnterPlanMode and ExitPlanMode, which Ringo does not serve. A trivial turn (Claude Code 2.1.316):
                // read 16,547 tokens before, 11,984 after; write 17,133 before, 12,570 after.
                let mut deny: Vec<&str> = Vec::new();
                if !edits {
                    deny.extend(["Bash", "Edit", "Write", "NotebookEdit"]);
                }
                deny.extend(OUT_OF_TURN_TOOLS);
                if self.mode != Mode::Read {
                    deny.extend(["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"]);
                }
                cmd.arg("--disallowed-tools").args(deny);
                if self.mode != Mode::Read {
                    // A commit or a PR is the owner's, as one made by hand is: no co-author trailer and no "Generated
                    // with" line (an empty string turns each off, Claude Code 2.1.316). Safe mode drops the owner's
                    // CLAUDE.md, where such a rule would otherwise live. It also drops Claude Code's attribution reminder
                    // from every turn: 626 bytes less a request (measured against a local capture server).
                    // Codex 0.165.0 adds none (codex_git_commit is removed and off) and agy has no such line.
                    let settings = serde_json::json!({"permissions": {"ask": self.always_ask},
                                                      "attribution": {"commit": "", "pr": ""}});
                    cmd.arg("--settings").arg(settings.to_string());
                    if !self.allowed.is_empty() {
                        cmd.arg("--allowedTools").args(&self.allowed);
                    }
                }
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["--effort", effort]);
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
                let deny = if edits { r#"["command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]"# }
                           else { r#"["write_file(*)","command(*)","unsandboxed(*)","mcp(*)","execute_url(*)"]"# };
                cmd.args(["--input-format", "stream-json", "--output-format", "stream-json", "--settings"])
                    .arg(format!(r#"{{"permissions":{{"deny":{deny}}}}}"#));
                if edits {
                    cmd.args(["--mode", "accept-edits"]);
                }
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["--effort", effort]);
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
        let parser = match turn {
            Some(turn) => Parser::new(style).with_turn(turn),
            None => Parser::new(style),
        };
        self.spawn(id, cmd, parser, input, None)?;
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
        self.spawn(id, cmd, Parser::new(Style::Trace), None, Some(trace))?;
        Ok(id)
    }

    /// `/review` of an agent's write job: its change becomes an unreferenced commit on the job's base (no hooks,
    /// no branch), reviewed by Ringo with the task as the change description.
    pub fn review_job(&mut self, of: usize) -> Result<usize, String> {
        let job = self.job(of)?;
        if job.status != Status::Done {
            return Err(format!("#{of} has not finished"));
        }
        // A copy's change against where it began; a turn in this folder's, from its before and after.
        let (wt, base, head) = match (job.worktree.clone(), job.base.clone(), job.before.clone(), job.after.clone()) {
            (Some(wt), Some(base), ..) => {
                git(&wt, &["add", "-A"])?;
                let tree = git(&wt, &["write-tree"])?;
                let head = self.commit(&tree, Some(&base), &format!("ringo job {of}"))?;
                (wt, base, head)
            }
            (_, _, Some(_), Some(_)) if !self.git && !job.changed.is_empty() => {
                return Err(format!("/review #{of} needs a git project"));
            }
            (_, _, Some(before), Some(after)) if !job.changed.is_empty() => {
                let base = self.commit(&before, None, &format!("ringo job {of}: before"))?;
                let head = self.commit(&after, Some(&base), &format!("ringo job {of}"))?;
                (self.repo.clone(), base, head)
            }
            _ => return Err(format!("#{of} changed no files to review")),
        };
        let job = self.job(of)?;
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
        let asked = self.jobs.iter().rev()
            .filter(|j| matches!(&j.kind, Kind::Ringo { command } if command == "ask") && j.status == Status::Done)
            .map(|j| (j, j.reply())).filter(|(_, reply)| !reply.trim_start().starts_with("ROUTE:"));
        for (job, reply) in asked {
            let text = format!("Question: {}\n\nAnswer:\n{}\n", job.title_full, reply.trim());
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

    fn spawn(&mut self, id: usize, mut cmd: Command, mut parser: Parser, stdin: Option<String>, trace: Option<PathBuf>)
             -> Result<(), String> {
        let style = parser.style();
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
        // The stdin text, then until the turn's result Claude's answers from the owner and Codex's turn request;
        // None closes the pipe.
        let (lines, queued) = channel::<Option<String>>();
        let keep_open = matches!(style, Style::Claude | Style::Codex);
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
                for line in parser.take_writes() {
                    if let Some(pipe) = &closer {
                        let _ = pipe.send(Some(format!("{line}\n")));
                    }
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
        let reply = (status == Status::Done).then(|| self.jobs[id - 1].reply());
        self.end_with(id, status, reply.unwrap_or_default());
    }

    /// `end` with the job's reply already read.
    fn end_with(&mut self, id: usize, status: Status, reply: String) {
        let job = &mut self.jobs[id - 1];
        job.elapsed = Some(job.started.elapsed());
        job.activity = match &status {
            Status::Done => parse::short(&reply, 80),
            Status::Failed(reason) => reason.clone(),
            _ => String::new(),
        };
        if let Status::Failed(reason) = &status {
            job.push_log(format!("failed: {reason}"));
        }
        // What it used, kept for /usage's last day and week (a stopped turn's messages' own counts included).
        let used = Tokens { input: job.tokens.input + job.live.input, cache_read: job.tokens.cache_read + job.live.cache_read,
                            cache_write: job.tokens.cache_write + job.live.cache_write, output: job.tokens.output + job.live.output };
        if used.total() > 0 {
            let who = match job.kind { Kind::Agent { agent, .. } => agent.name().to_string(), Kind::Ringo { .. } => "ringo".into() };
            let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let record = serde_json::json!({"at": at, "agent": who, "input": used.input, "cache_read": used.cache_read,
                                            "cache_write": used.cache_write, "output": used.output});
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(self.root.join("usage.jsonl")) {
                let _ = writeln!(f, "{record}");
            }
        }
        job.asks.clear(); // nobody is waiting for these answers any more
        job.input = None;
        job.status = status;
        for sub in job.subs.iter_mut().filter(|s| s.ended.is_none()) {
            sub.ended = Some(false);
            sub.activity = "stopped with its parent".into();
        }
        // A write job that changed nothing has nothing to apply (git's exit code says so; no patch is written). Its
        // worktree stays for /drop: removing it here would also remove what the diff cannot see (a file git ignores
        // that the agent wrote).
        if job.status == Status::Done && let (Some(wt), Some(base)) = (job.worktree.clone(), job.base.clone())
            && git(&wt, &["add", "-A"]).is_ok() && git(&wt, &["diff", "--cached", "--quiet", &base]).is_ok() {
            self.jobs[id - 1].unchanged = true;
        }
        // A turn in the owner's folder, stopped or not: what it changed there, for its last line and /undo.
        if let Some(before) = self.jobs[id - 1].before.clone()
            && let Ok(after) = self.snapshot(&self.jobs[id - 1].dir.join("index")) {
            let changed = self.folder_git(&["diff", "--name-only", &before, &after]).unwrap_or_default();
            let job = &mut self.jobs[id - 1];
            job.changed = changed.lines().map(str::to_string).collect();
            job.after = Some(after);
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
                    let reply = job.reply(); // read once, for the route, the conversation and the job's last line
                    if matches!(&job.kind, Kind::Ringo { command } if command == "ask") && status == Status::Done {
                        // The ask prompt answers a task with one line, `ROUTE: edit|read|review|research`.
                        if let Some(route) = reply.trim().strip_prefix("ROUTE:") {
                            // the route word alone: "ROUTE: edit." or a line of reasons after it still starts the task
                            let word = route.split(|c: char| !c.is_ascii_alphabetic()).find(|w| !w.is_empty());
                            self.routed.push_back((id, word.unwrap_or("").to_lowercase()));
                        }
                    }
                    if matches!(job.kind, Kind::Ringo { .. }) {
                        let report = job.dir.join("work").join("report.json");
                        if !reply.is_empty() {
                            job.push(Said::Reply(reply.clone()));
                        } else if let Ok(text) = fs::read_to_string(&report) {
                            let state = serde_json::from_str::<serde_json::Value>(&text).ok()
                                .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
                            job.push_log(format!("work {}: {}", state.unwrap_or_default(), report.display()));
                        }
                    }
                    self.end_with(id, status, reply);
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
                        Note::Reply(text) => job.push(Said::Reply(text)),
                        Note::Partial(text) => job.push(Said::Partial(text)),
                        Note::Step(text) => job.push(Said::Step(text)),
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
                            job.tokens.add(t);
                            if t.total() > 0 {
                                // the turn's own count replaces what its messages said; a result without one (an
                                // error result) keeps them
                                job.live = Tokens::default();
                            }
                        }
                        Note::Live(t) => job.live = t,
                        Note::Call => job.calls += 1,
                        Note::Touched(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path);
                            job.touched.insert(rel.to_string_lossy().into_owned());
                        }
                        Note::Fail(reason) => job.push_log(format!("stopping: {reason}")),
                        Note::Limits(info) => self.claude_limits = Some(info),
                        Note::Permission(_) | Note::Unserved(_) => {} // handled above
                    }
                }
            }
        }
        changed
    }

    /// What finished jobs used in the last `secs` seconds, by agent, from `usage.jsonl` (every session's), and when its
    /// first record was written.
    // ceiling: reads the whole file, one short line a job; a year of daily use is a few MB.
    pub fn used_since(&self, secs: u64) -> (BTreeMap<String, Tokens>, Option<u64>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let (mut by, mut first) = (BTreeMap::new(), None);
        for line in fs::read_to_string(self.root.join("usage.jsonl")).unwrap_or_default().lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let at = r["at"].as_u64().unwrap_or(0);
            first = first.or(Some(at));
            if now.saturating_sub(at) <= secs {
                let n = |k: &str| r[k].as_u64().unwrap_or(0);
                let t: &mut Tokens = by.entry(r["agent"].as_str().unwrap_or("?").to_string()).or_default();
                t.add(Tokens { input: n("input"), cache_read: n("cache_read"), cache_write: n("cache_write"), output: n("output") });
            }
        }
        (by, first)
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

    /// Applies a finished write job's change to the owner's files. Nothing is staged or committed. The copy began
    /// as the folder was, so the change applies onto it as it is, uncommitted changes and all; where the folder has
    /// moved on (a commit since), a clean tree gets a three-way merge. Anything else that does not apply leaves both
    /// the folder and the job as they were.
    pub fn apply(&mut self, id: usize) -> Result<String, String> {
        let job = self.job(id)?;
        if job.status != Status::Done {
            return Err(format!("#{id} has not finished successfully"));
        }
        self.idle(&format!("/apply {id}"))?;
        let patch = self.diff(id)?;
        if patch.is_empty() {
            return Err(format!("#{id} changed nothing"));
        }
        let file = self.job(id)?.dir.join("change.patch").to_string_lossy().into_owned();
        if git(&self.repo, &["apply", "--check", &file]).is_ok() {
            git(&self.repo, &["apply", &file])?;
        } else if git(&self.repo, &["status", "--porcelain"])?.is_empty() {
            git(&self.repo, &["apply", "--check", "--3way", &file])
                .map_err(|e| format!("{e}\n#{id} is untouched; inspect it with /diff {id}"))?;
            git(&self.repo, &["apply", "--3way", &file])?;
        } else {
            return Err(format!("your files changed where #{id} edits them, so nothing was applied; /diff {id} shows its \
                                change (commit or stash yours and /apply {id} again for a three-way merge)"));
        }
        self.jobs[id - 1].applied = true;
        git(&self.repo, &["apply", "--stat", &file])
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
            // The change as text: a binary file is one "Binary files ... differ" line, not base85 the agent cannot
            // read (a 20 KB binary file was 19,546 tokens of it). /apply keeps the binary patch.
            let patch = match (&job.worktree, &job.base, &job.before, &job.after) {
                (Some(wt), Some(base), ..) => {
                    git(wt, &["add", "-A"])?;
                    Some(git_raw(wt, &["diff", "--cached", base])?)
                }
                (_, _, Some(before), Some(after)) => Some(run_git(self.folder(), &["diff", before, after])?),
                _ => None,
            };
            if let Some(patch) = patch.filter(|p| !p.is_empty()) {
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
    register_custom(&read_agents_file(root))
}

fn register_custom(file: &serde_json::Value) -> Vec<String> {
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
    let file = read_agents_file(&state_root());
    register_custom(&file);
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

// --- The owner's subscription plans: how much of each limit is used, asked of the agents with no model call -----

/// One limit of a plan: what it is, how much of it is used (0-100) and when it starts over (Unix seconds).
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub name: String,
    pub percent: f64,
    pub resets: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// The plan's name as the agent reports it (max, plus, ...).
    pub name: String,
    pub windows: Vec<Window>,
    /// Anything else worth a line: reset credits left, extra usage.
    pub notes: Vec<String>,
}

/// Unix seconds of an ISO 8601 time such as `2026-11-02T04:30:00.368171+00:00` (fractions dropped).
pub fn iso_secs(text: &str) -> Option<u64> {
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    // days from 1970-01-01 to the date (H. Hinnant's days_from_civil)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let tail = &text[19..];
    let tail = tail.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match tail.as_bytes().first() {
        Some(b'+' | b'-') if tail.len() >= 6 => {
            let mins = tail[1..3].parse::<i64>().ok()? * 60 + tail[4..6].parse::<i64>().ok()?;
            if tail.starts_with('-') { -mins } else { mins }
        }
        _ => 0, // Z, or none
    };
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec - offset * 60).ok()
}

/// Writes `requests` to the program's stdin, one a line, and returns the first stdout line `wanted` accepts; the
/// program is stopped then, or after `limit`.
fn ask_once(mut cmd: Command, requests: &[serde_json::Value], wanted: impl Fn(&serde_json::Value) -> bool + Send + 'static,
            limit: Duration) -> Result<serde_json::Value, String> {
    agent_env(&mut cmd);
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        .map_err(|e| format!("{}: {e}", cmd.get_program().to_string_lossy()))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    for r in requests {
        writeln!(stdin, "{r}").map_err(|e| e.to_string())?;
    }
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) && wanted(&v) {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let answer = rx.recv_timeout(limit);
    drop(stdin); // both end when their input does
    let _ = child.kill();
    let _ = child.wait();
    answer.map_err(|_| format!("no answer within {} s", limit.as_secs()))
}

/// The owner's plan as an agent reports it, asked without a model call: Claude Code's `get_usage` control request (a
/// session that is never sent a message), Codex's app-server `account/rateLimits/read`. Each takes about a second.
/// Claude marks `get_usage` experimental; when it fails, the shell falls back to the last turn's rate-limit event.
pub fn plan(agent: Agent, program: &Path) -> Result<Plan, String> {
    match agent.spec().adapter {
        Adapter::Claude => {
            let mut cmd = Command::new(program);
            cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--safe-mode",
                      "--no-session-persistence"]);
            let ask = serde_json::json!({"type": "control_request", "request_id": "ringo-usage",
                                         "request": {"subtype": "get_usage", "skip_behaviors": true}});
            let v = ask_once(cmd, &[ask], |v| v["type"] == "control_response", Duration::from_secs(15))?;
            if v.pointer("/response/subtype").and_then(|s| s.as_str()) != Some("success") {
                return Err(parse::short(&v.pointer("/response/error").map(|e| e.to_string()).unwrap_or_default(), 120));
            }
            Ok(claude_plan(&v["response"]["response"]))
        }
        Adapter::Codex => {
            let mut cmd = Command::new(program);
            cmd.args(["app-server", "-c", "forced_login_method=\"chatgpt\"", "-c", "model_provider=\"openai\"",
                      "-c", "analytics.enabled=false"]);
            let hello = serde_json::json!({"id": 1, "method": "initialize",
                                           "params": {"clientInfo": {"name": "ringo", "version": env!("CARGO_PKG_VERSION")}}});
            let ask = serde_json::json!({"id": 2, "method": "account/rateLimits/read", "params": {"excludeResetCreditDetails": true}});
            let v = ask_once(cmd, &[hello, serde_json::json!({"method": "initialized"}), ask], |v| v["id"] == 2,
                             Duration::from_secs(15))?;
            if let Some(e) = v.get("error") {
                return Err(parse::short(e["message"].as_str().unwrap_or("an error"), 120));
            }
            Ok(codex_plan(&v["result"]))
        }
        Adapter::Agy | Adapter::Plain => Err("reports no plan usage".into()),
    }
}

fn claude_window(kind: &str) -> String {
    match kind {
        "session" | "five_hour" => "5 hours".into(),
        "weekly_all" | "seven_day" => "week".into(),
        other => other.replace('_', " "),
    }
}

/// Claude Code's `get_usage` answer (2.1.316): `rate_limits.limits` as its /usage shows them, else the two windows.
pub fn claude_plan(r: &serde_json::Value) -> Plan {
    let mut plan = Plan { name: r["subscription_type"].as_str().unwrap_or("").to_string(), ..Plan::default() };
    let limits = r.pointer("/rate_limits/limits").and_then(|l| l.as_array()).cloned().unwrap_or_default();
    for l in &limits {
        plan.windows.push(Window { name: claude_window(l["kind"].as_str().unwrap_or("")), percent: l["percent"].as_f64().unwrap_or(0.0),
                                   resets: l["resets_at"].as_str().and_then(iso_secs) });
    }
    if limits.is_empty() {
        for key in ["five_hour", "seven_day"] {
            if let Some(w) = r["rate_limits"].get(key).filter(|w| w.is_object()) {
                plan.windows.push(Window { name: claude_window(key), percent: w["utilization"].as_f64().unwrap_or(0.0),
                                           resets: w["resets_at"].as_str().and_then(iso_secs) });
            }
        }
    }
    if r.pointer("/rate_limits/extra_usage/is_enabled").and_then(|b| b.as_bool()) == Some(true) {
        plan.notes.push("extra usage is on".into());
    }
    plan
}

/// The windows of a Claude turn's `rate_limit_event` (`rate_limit_info`), whose utilization is a fraction.
pub fn claude_plan_from_event(info: &serde_json::Value) -> Plan {
    let mut plan = Plan::default();
    for key in ["five_hour", "seven_day"] {
        if let Some(w) = info.pointer(&format!("/unifiedWindows/{key}")).filter(|w| w.is_object()) {
            plan.windows.push(Window { name: claude_window(key), percent: w["utilization"].as_f64().unwrap_or(0.0) * 100.0,
                                       resets: w["resetsAt"].as_u64() });
        }
    }
    plan
}

/// Codex's `account/rateLimits/read` answer (codex-cli 0.166.0).
pub fn codex_plan(r: &serde_json::Value) -> Plan {
    let limits = &r["rateLimits"];
    let mut plan = Plan { name: limits["planType"].as_str().unwrap_or("").to_string(), ..Plan::default() };
    for key in ["primary", "secondary"] {
        let w = &limits[key];
        if !w.is_object() {
            continue;
        }
        let name = match w["windowDurationMins"].as_u64() {
            Some(300) => "5 hours".to_string(),
            Some(10_080) => "week".to_string(),
            Some(m) if m % 1440 == 0 => format!("{} days", m / 1440),
            Some(m) => format!("{} hours", m / 60),
            None => key.to_string(),
        };
        plan.windows.push(Window { name, percent: w["usedPercent"].as_f64().unwrap_or(0.0), resets: w["resetsAt"].as_u64() });
    }
    if let Some(n) = r.pointer("/rateLimitResetCredits/availableCount").and_then(|n| n.as_u64()).filter(|n| *n > 0) {
        plan.notes.push(format!("{n} full reset{} left", if n == 1 { "" } else { "s" }));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_read_as_the_agents_report_them() {
        assert_eq!(iso_secs("2026-11-02T04:30:00.368171+00:00"), Some(1_793_593_800));
        assert_eq!(iso_secs("2026-11-02T00:30:00-04:00"), Some(1_793_593_800));
        assert_eq!(iso_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_secs("soon"), None);
        // Claude Code 2.1.316's get_usage answer (trimmed)
        let claude = serde_json::json!({"subscription_type": "max", "rate_limits": {
            "five_hour": {"utilization": 5, "resets_at": "2026-11-02T04:30:00.368171+00:00"},
            "limits": [{"kind": "session", "group": "session", "percent": 5, "resets_at": "2026-11-02T04:30:00.368171+00:00"},
                       {"kind": "weekly_all", "group": "weekly", "percent": 59, "resets_at": "2026-11-04T13:00:00+00:00"}],
            "extra_usage": {"is_enabled": false}}});
        let plan = claude_plan(&claude);
        assert_eq!(plan.name, "max");
        assert_eq!(plan.windows, [Window { name: "5 hours".into(), percent: 5.0, resets: Some(1_793_593_800) },
                                  Window { name: "week".into(), percent: 59.0, resets: iso_secs("2026-11-04T13:00:00+00:00") }]);
        assert!(plan.notes.is_empty());
        // a Claude turn's rate_limit_event: utilization as a fraction
        let event = serde_json::json!({"status": "allowed", "unifiedWindows": {
            "five_hour": {"utilization": 0.21, "resetsAt": 1_793_332_800u64}, "seven_day": {"utilization": 0.32, "resetsAt": 1_793_797_200u64}}});
        let plan = claude_plan_from_event(&event);
        assert_eq!((plan.windows[0].name.as_str(), plan.windows[0].percent.round(), plan.windows[1].resets),
                   ("5 hours", 21.0, Some(1_793_797_200)));
        // codex-cli 0.166.0's account/rateLimits/read answer (trimmed)
        let codex = serde_json::json!({"rateLimits": {"limitId": "codex", "planType": "plus",
            "primary": {"usedPercent": 41, "windowDurationMins": 300, "resetsAt": 1_793_594_030u64},
            "secondary": {"usedPercent": 34, "windowDurationMins": 10080, "resetsAt": 1_794_078_152u64}},
            "rateLimitResetCredits": {"availableCount": 2, "credits": null}});
        let plan = codex_plan(&codex);
        assert_eq!(plan.name, "plus");
        assert_eq!(plan.windows.iter().map(|w| (w.name.as_str(), w.percent)).collect::<Vec<_>>(), [("5 hours", 41.0), ("week", 34.0)]);
        assert_eq!(plan.notes, ["2 full resets left"]);
    }

    #[test]
    fn the_undo_store_takes_only_a_small_folder() {
        let dir = std::env::temp_dir().join(format!("ringo-small-{}", std::process::id()));
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        for (name, text) in [("a.txt", "12345"), ("b.txt", "12345"), ("node_modules/pkg/x.js", "ignored"), ("c.pyc", "ignored")] {
            fs::write(dir.join(name), text).unwrap();
        }
        assert_eq!(fits_store(&dir, 2, 10), Ok(()), "two files of five bytes; what the store ignores is not counted");
        assert!(fits_store(&dir, 1, 10).unwrap_err().contains("more than 1 files") && fits_store(&dir, 2, 9).is_err());
        // a git project inside: its files would be one link in the store, so the store is refused
        fs::create_dir_all(dir.join("CSC133/.git")).unwrap();
        assert!(fits_store(&dir, 9, 99).unwrap_err().contains("CSC133 is a git project of its own"));
        // a relative state root (taken from the current folder) still names the same store from the folder
        let folder = dir.join("class");
        fs::create_dir_all(&folder).unwrap();
        let relative = Path::new("target").join(format!("ringo-relative-state-{}", std::process::id()));
        let store = undo_store(&relative, &resolved(&folder), Path::new("")).unwrap();
        assert!(store.is_absolute() && store.join("HEAD").is_file(), "{}", store.display());
        let _ = fs::remove_dir_all(&relative);
        let _ = fs::remove_dir_all(dir);
    }
}

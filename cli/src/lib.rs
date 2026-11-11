//! Ringo's conductor: launches agent turns and Ringo pipelines as background jobs, follows their events, and keeps
//! agent edits in private git worktrees until the owner applies them. The terminal shell (`ringo`) and, later,
//! the IDE drive a `Session` and render its jobs.

pub mod parse;
pub mod rt;

use parse::{Note, Parser, Permission, Style, Tokens};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOG_LINES: usize = 2000; // per job in memory; the full stream is on disk
/// Past this many tokens in an agent's window, its next turn starts fresh from Ringo's record of the conversation
/// instead of re-reading all of it at every call (see `Session::handoff`).
pub const HANDOFF_AT: u64 = 64_000;
/// How much of Ringo's record goes into a turn, in characters (about four a token): the turns, newest first, then
/// the change they made.
const RECORD_CHARS: usize = 60_000;
/// What a shell keeps in its folder for a later one to go on from (`Session::keep`, `ringo --resume`).
pub const KEPT: &str = "resume.json";
/// Where an agent's conversation compacts itself mid-turn, in tokens: Claude's and Codex's (see `launch_agent`).
const COMPACT_AT: u64 = 200_000;

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
    /// What makes an agent of the owner's a specialist: a file of standing instructions that goes to its every turn
    /// (see `instructions`). A Claude or Codex agent only: Ringo has no place to give them to another.
    pub instructions: Option<String>,
    /// Claude only: outside read mode its turns load the owner's Claude Code skills, plugins and connectors, which
    /// every other turn goes without (see `launch_agent`).
    pub skills: bool,
}

/// A path as its owner typed it: `~/` is their home folder.
pub fn home_path(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
    path.strip_prefix("~/").map_or_else(|| PathBuf::from(path), |rest| home.join(rest))
}

/// The most a specialist's instructions may hold: they are read on every call of its turns (about 100k tokens here).
pub const INSTRUCTIONS_MAX: usize = 400_000;

/// A specialist's file as it stands now: its standing instructions, and what its opening `---` block says of the
/// model it runs on and the tools it uses, as an agent file made for Claude Code has them.
#[derive(Debug, Default, PartialEq)]
pub struct OwnFile {
    pub text: Vec<u8>,
    /// `model:` (not `inherit`): the model its turns run on, unless the owner chose one with /model.
    pub model: Option<String>,
    /// `tools:` the built-in tools it is offered, and no others (a connector's tools are not Ringo's to offer): none
    /// when the line names none of those. No such line, or `default` in it: None, every tool as usual.
    pub tools: Option<Vec<String>>,
}

/// A specialist's standing instructions (`Spec::instructions`), read anew for each turn, so an edit applies at the
/// next one: the file's text without a leading `---` block (a Claude Code agent file opens with its name and
/// description there). An agent that has none: None. A file that cannot be read, holds nothing or is too large is an
/// error, never a turn without them: that turn would not be this agent's.
pub fn instructions(spec: &Spec) -> Result<Option<Vec<u8>>, String> {
    own_file(spec).map(|file| file.map(|f| f.text))
}

/// `instructions`, with what the file's opening block says of its model and tools.
pub fn own_file(spec: &Spec) -> Result<Option<OwnFile>, String> {
    let Some(path) = &spec.instructions else { return Ok(None) };
    let file = home_path(path);
    let bytes = fs::read(&file).map_err(|e| format!("{}'s instructions could not be read ({}: {e})", spec.name, file.display()))?;
    let (mut body, mut head) = (&bytes[..], &bytes[..0]);
    if let Some(rest) = bytes.strip_prefix(b"---\n").or_else(|| bytes.strip_prefix(b"---\r\n")) {
        let mut at = 0;
        for line in rest.split_inclusive(|b| *b == b'\n') {
            if line.trim_ascii_end() == b"---" {
                (head, body) = (&rest[..at], &rest[at + line.len()..]);
                break;
            }
            at += line.len();
        }
    }
    let body = body.trim_ascii();
    match body.len() {
        0 => return Err(format!("{}'s instructions file has no text in it ({})", spec.name, file.display())),
        n if n > INSTRUCTIONS_MAX => return Err(format!("{}'s instructions are {n} bytes, and they are read on every call: \
                                                         keep them under {INSTRUCTIONS_MAX} ({})", spec.name, file.display())),
        _ => {}
    }
    // The block's `model:` and `tools:` lines, the two that change how a turn runs. A value is one line, or for the
    // tools a list: `tools: Read, Grep`, `tools: [Read, Grep]`, or `- Read` lines under it.
    let bare = |v: &str| v.trim().trim_matches(['"', '\'']).trim().to_string();
    let (mut model, mut tools, mut listing) = (None, None::<Vec<String>>, false);
    for line in String::from_utf8_lossy(head).lines() {
        if let Some(item) = line.trim_start().strip_prefix('-').filter(|_| listing) {
            tools.get_or_insert_default().push(bare(item));
        } else if let Some(value) = line.strip_prefix("model:") {
            listing = false;
            model = Some(bare(value)).filter(|m| m != "inherit" && valid_model(m));
        } else if let Some(value) = line.strip_prefix("tools:") {
            listing = value.trim().is_empty();
            tools = Some(value.trim().trim_matches(['[', ']']).split(',').map(bare).collect());
        } else if !line.trim().is_empty() {
            listing = false;
        }
    }
    // `default` is Claude Code's word for its whole set: no restriction. Otherwise the names Ringo can pass on are
    // kept, and a line that leaves none offers none: it was written to narrow, and must not widen by being unreadable.
    let tools = tools.filter(|all| !all.iter().any(|t| t.eq_ignore_ascii_case("default"))).map(|mut all| {
        all.retain(|t| !t.is_empty() && !t.starts_with("mcp__") && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        all
    });
    Ok(Some(OwnFile { text: body.to_vec(), model, tools }))
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
        let instructions = text("instructions").filter(|p| !p.trim().is_empty());
        if instructions.is_some() && !matches!(adapter, Adapter::Claude | Adapter::Codex) {
            return Err(format!("{name}: \"instructions\" go to a claude or codex agent; Ringo has no place to give them \
                                to another"));
        }
        let skills = v.get("skills").and_then(|b| b.as_bool()).unwrap_or(false);
        if skills && adapter != Adapter::Claude {
            return Err(format!("{name}: \"skills\" are Claude Code's, so its adapter must be claude"));
        }
        let os = if cfg!(windows) { "windows" } else { "unix" };
        let installer = v.get("installer").and_then(|i| i.get(os)).and_then(|s| s.as_str()).map(str::to_string);
        Ok(Spec { program: text("program").unwrap_or_else(|| name.clone()),
                  company: text("company").unwrap_or_else(|| format!("{name} (added by you)")),
                  install: text("install").unwrap_or_default(), login: list("login")?, read, args: list("args")?,
                  npm: text("npm"), self_update: text("self_update"), installer, adapter, name, custom, instructions, skills })
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
    /// Extra-high reasoning effort (xhigh).
    Deep,
    /// Deep, then Ringo reviews each change a turn makes here and the agent gets one revision turn when the
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
            // xhigh: Claude and Codex's models take it ("high" would lower an owner whose own settings run Claude at
            // xhigh; a setting above xhigh, max, is still lowered to it); agy's default model tops out at high, which
            // it gets instead (see launch_agent).
            Tier::Deep | Tier::Ultra => Some("xhigh"),
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
pub const OUT_OF_TURN_TOOLS: [&str; 15] = ["CronCreate", "CronDelete", "CronList", "ScheduleWakeup", "RemoteTrigger",
    "PushNotification", "SendToProject", "SendToProjectContext", "EnterWorktree", "ExitWorktree", "DesignSync",
    "ReportFindings", "Workflow", "ListAgents", "SendMessage"];

/// Sent to a Codex turn whose owner's own Codex instructions send it to Ringo: without it, one small task (add a
/// function and check it) read Ringo's 25 KB guide, started a Ringo review inside the turn and used 283,594 tokens;
/// with it (and the two tools `launch_agent` turns off), 55,966 for the same result (codex-cli 0.166.0). It promises
/// no review: only the ultra tier has one. Claude's safe mode drops the owner's CLAUDE.md instead. ceiling: the
/// owner's file is still sent (3.8 KB here); a private CODEX_HOME would drop it, as the pipelines' calls have, but
/// turns would lose the owner's settings and threads.
const CODEX_IN_RINGO: &str = "Ringo launched this conversation, so do not read Ringo's guides or run Ringo inside it, \
    even where other instructions say to; do the task directly and check it as you normally would.";

/// Words that name a service reached through the owner's connectors (MCP servers). A Claude turn whose task names
/// one, or that follows a turn that called one, runs with the owner's connectors loaded; any other runs without them (16.6k tokens a trivial turn instead of
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

/// Added to a Claude turn that reaches a connector. One ClickUp question (Claude Code 2.1.316) loaded 30 ClickUp tools
/// to use 3, and read the whole workspace's week (a 42,839-character answer) before asking for the owner's own (1,308):
/// every call after re-read both, 254k tokens in 7 calls.
const CLAUDE_CONNECTOR_NOTE: &str = "\n\n(Ringo: this reaches a connector. Load only the tools it needs (ToolSearch with \
    max_results 5 or fewer: each loaded tool is re-read by every later call), and ask for only what answers it, filtered \
    on the service's side: the owner's own items unless the message says otherwise, only the dates and fields needed.)";
/// The same for Codex, whose connectors come as apps rather than tools to search for.
const CODEX_CONNECTOR_NOTE: &str = "\n\n(Ringo: this reaches a connector. Ask for only what answers it, filtered on the \
    service's side: the owner's own items unless the message says otherwise, only the dates and fields needed.)";

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
    /// The tier the session was in when the job started; it keeps it whatever the session changes to.
    pub tier: Tier,
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
    /// A turn in this folder: None in the conversation all agents share; Some(its agent) when it ran in /separate,
    /// in that agent's own.
    pub lane: Option<Agent>,
    /// Each step's first line (a command and its exit, an edit), for Ringo's record of the conversation.
    pub steps: Vec<String>,
    /// Whether it ran a shell command, which may have written files it did not report.
    pub ran: bool,
    /// Lines for the agent's stdin while the turn runs (Claude's answers to its permission requests); None closes it.
    input: Option<Sender<Option<String>>>,
    /// The owner's lines sent to the running turn (`Session::steer`), and the gate they pass with the reader.
    pub steered: Vec<String>,
    gate: Arc<Mutex<Gate>>,
    /// Codex: the thread and the running turn, which a line for it names.
    codex_turn: Option<(String, String)>,
    /// Whether the turn runs with the owner's connectors, which a line that names one needs, and whether it called
    /// one (its follow-up keeps them).
    connectors: bool,
    reached: bool,
    pid: Option<u32>,
    cancel: Arc<AtomicBool>,
    /// The model the agent said it runs, when it says (for the usage record).
    pub model: Option<String>,
    /// A turn in the agents' talk (the shell's /discuss): the others hear all of its reply, not its first lines.
    pub talk: bool,
    /// Whether this turn was given the means to ask another agent (`serve_asks`), and how many asks it has made.
    pub can_ask: bool,
    pub asked: usize,
    /// A turn another agent asked for in the middle of its own: that turn's job, and the file its answer goes in.
    pub asked_by: Option<(usize, PathBuf)>,
    /// Whether the job reported any token count: one that finishes without is recorded as unknown, not left out.
    counted: bool,
    /// How many of its model calls reported no count (a pipeline's): its counts are then of the others alone.
    uncounted: u32,
    /// The running total its agent last reported inside this job, where a turn's own count is not reported and the
    /// job has a conversation of its own (`Note::TokensSoFar` in a write job).
    so_far: Tokens,
    /// What starting this turn marked as told to its agent: the other agents' turns (job ids) and the turns before a
    /// restart. A turn that fails before its agent takes its message in gives them back (`end_with`).
    told_now: (Vec<usize>, Option<String>),
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
        let (Said::Reply(text) | Said::Step(text) | Said::Line(text) | Said::Looked(_, text)) = &said else {
            self.fresh.push(said); // a piece of the reply: the log takes the whole reply when it comes
            return;
        };
        if let Said::Step(step) = &said {
            self.steps.push(step.lines().next().unwrap_or("").to_string());
        }
        for l in text.lines() {
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(l.to_string());
        }
        self.fresh.push(said);
    }
}

/// How many of the owner's lines went to a running turn, and whether its stdin has closed: the sender and the reader
/// take the same lock, so a line is either written before the close (and the reader waits until the agent has taken
/// it in) or refused, to go as the next turn. None is lost.
#[derive(Default)]
struct Gate {
    sent: usize,
    closed: bool,
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
    /// A step that only looked (see `Note::Looked`): the shell counts these; the log keeps each one.
    Looked(&'static str, String),
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
    /// Whether the session opened in the owner's home folder (or above it), where no one project is meant.
    pub at_home: bool,
    /// Codex's own folder ($CODEX_HOME, else ~/.codex), whose AGENTS.md Codex reads into every turn.
    pub codex_home: PathBuf,
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
    /// What each agent is told on its first turn after a /restart: Ringo's record of the turns it had not heard.
    told: HashMap<Agent, String>,
    /// Ringo's record of the turns before a /restart that each agent's conversation holds: a fresh start here begins
    /// with it, so a restart neither keeps a long conversation going nor drops what came before it (see `handoff`).
    before: HashMap<Agent, String>,
    /// The file this shell went on from (/restart or `ringo --resume`), whose conversation becomes this one's once it
    /// keeps its own (see `keep`).
    pub source: Option<PathBuf>,
    /// The owner's latest message in the conversation it went on from: how `ringo --resume` lists it until a turn here.
    pub about: String,
    /// The agents whose last turn in the conversation went on from called a connector: the next keeps them (`reached`).
    reached_before: HashSet<Agent>,
    /// How full each agent's conversation is, in tokens, as its latest call read it (see `handoff`).
    context: HashMap<Agent, u64>,
    /// The other agents' turns each agent's conversation has been told, by job id (see `unheard`).
    heard: HashMap<Agent, BTreeSet<usize>>,
    /// Past this, an agent's next turn starts fresh from Ringo's record instead (tests lower it).
    pub handoff_at: u64,
    /// /separate: each agent keeps a conversation of its own, and they can take turns here at the same time.
    pub separate: bool,
    /// The owner's lines a turn ended before taking in (`steer`): the job and the line, for the shell to send next
    /// (or, the turn stopped, to put back in the input).
    pub returned: Vec<(usize, Agent, String)>,
    /// The first job of the conversation (/clear starts a new one).
    start: usize,
    /// The running total each resumable conversation last reported, by agent and conversation id.
    usage_so_far: HashMap<(Agent, String), Tokens>,
    /// How far agents act without asking.
    pub mode: Mode,
    /// How hard they think (see `Tier`).
    pub tier: Tier,
    /// The model each agent's turns run on, where the owner chose one (`/model`): a name the agent's own program
    /// takes (Claude Code's `--model`, Codex's thread `model`). None for an agent: its own default.
    pub models: HashMap<Agent, String>,
    /// Ringo's state folder, where `usage.jsonl` keeps what each finished job used.
    root: PathBuf,
    /// The owner's home folder, where the agents they made for Claude Code are (`.claude/agents`).
    pub home: PathBuf,
    /// The agents set up in the shell that runs this session: the ones a turn here may ask a question in the middle
    /// of its work (`serve_asks`). With none, no turn is given the means to.
    pub peers: Vec<Agent>,
    /// The program that serves a turn's asks: this one (`ringo ask-server`).
    pub asker: PathBuf,
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

/// Tokens as a log line says them: 12.3k.
fn fmt_k(n: u64) -> String {
    format!("{:.1}k", n as f64 / 1e3)
}

/// A conversation `ringo --resume` can go on with: its file, when it was kept and the owner's latest message.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub path: PathBuf,
    pub saved: u64,
    pub about: String,
}

/// The conversations kept for `folder` (a session's `repo`) under the state `root`, newest first.
pub fn kept(root: &Path, folder: &Path) -> Vec<Kept> {
    let Ok(shells) = fs::read_dir(resolved(&root.join("shell"))) else { return Vec::new() }; // as `Session::new` names it
    let mut all: Vec<Kept> = shells.flatten().filter_map(|shell| {
        let path = shell.path().join(KEPT);
        let kept: serde_json::Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        (Path::new(kept["folder"].as_str()?) == folder).then(|| Kept {
            saved: kept["saved"].as_u64().unwrap_or(0), about: kept["about"].as_str().unwrap_or_default().to_string(), path,
        })
    }).collect();
    all.sort_by_key(|k| std::cmp::Reverse(k.saved));
    all
}

/// The strings in a JSON list (none if it is not one).
fn list_names(v: &serde_json::Value) -> impl Iterator<Item = &str> {
    v.as_array().into_iter().flatten().filter_map(|n| n.as_str())
}

/// The newest `room` bytes of `record` at most, from a line's start, saying so when the earliest are cut.
fn newest(record: &str, room: usize) -> String {
    if record.len() <= room {
        return record.to_string();
    }
    let at = (record.len() - room..record.len()).find(|&i| record.is_char_boundary(i)).unwrap_or(record.len());
    let kept = record[at..].split_once('\n').map_or("", |(_, rest)| rest);
    format!("(the earliest turns before the restart are cut)\n{kept}")
}

/// Files as Ringo's record names them: the first 20, then how many more.
fn files(names: &BTreeSet<String>) -> String {
    let shown: Vec<&str> = names.iter().take(20).map(String::as_str).collect();
    match names.len().checked_sub(20).filter(|&more| more > 0) {
        Some(more) => format!("{} and {more} more", shown.join(", ")),
        None => shown.join(", "),
    }
}

/// Where Ringo keeps private state: the same root as its releases (RINGO_RELEASE_HOME or ~/.local/share/ringo).
pub fn state_root() -> PathBuf {
    if let Some(root) = std::env::var_os("RINGO_RELEASE_HOME") {
        return PathBuf::from(root);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".local/share/ringo")
}

/// The Ringo checkout whose ringo.py runs the pipelines: RINGO_HOME, else the one this binary was built from (the
/// updater's copy of that version, or the checkout `cargo install` ran in).
pub fn ringo_home() -> PathBuf {
    std::env::var_os("RINGO_HOME").map(PathBuf::from).unwrap_or_else(|| match env!("RINGO_BUILD_HOME") {
        "" => Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf(),
        home => PathBuf::from(home),
    })
}

/// The commit this binary was built from; empty for a build outside git.
pub const COMMIT: &str = env!("RINGO_COMMIT");

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
        let at_home = !home.as_os_str().is_empty() && home.starts_with(&repo);
        let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
        Ok(Session { repo, git: project.is_some(), index, branch, store, no_undo, at_home, codex_home, home: home.clone(), peers: Vec::new(), asker: std::env::current_exe().unwrap_or_default(), dir, jobs: Vec::new(), max_jobs: 3,
                     backend: "codex-chatgpt".into(), models: HashMap::new(),
                     ringo_home: ringo_home(), pipeline: None, programs, hold: None, threads: HashMap::new(), told: HashMap::new(), before: HashMap::new(), source: None, about: String::new(), reached_before: HashSet::new(),
                     context: HashMap::new(), heard: HashMap::new(), handoff_at: HANDOFF_AT, separate: false,
                     returned: Vec::new(), start: 1,
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
        self.jobs.iter().rev().find(|j| j.running() && matches!(j.kind, Kind::Agent { write: false, .. })).map(|j| j.id)
    }

    /// Sends the owner's `text` into job `id`, a turn of Claude's or Codex's still running, as Claude Code and Codex
    /// take a message typed while they work: it reads it at its next step. False when the turn cannot take it (agy,
    /// a /compact, a Codex turn not yet started, or one whose input has closed); the line then waits for the next turn.
    /// Whether job `id` can take a line as it works (`steer`), whatever the line says: a Claude or Codex turn at work
    /// that is not a `/compact` and has not closed its input.
    pub fn steerable(&self, id: usize) -> bool {
        let Some(job) = self.jobs.get(id.wrapping_sub(1)) else { return false };
        let Kind::Agent { agent, .. } = job.kind else { return false };
        let open = match agent.spec().adapter {
            Adapter::Claude => true,
            Adapter::Codex => job.codex_turn.is_some(),
            _ => false,
        };
        open && job.running() && job.title_full.trim() != "/compact" && job.input.is_some()
            && !job.gate.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    pub fn steer(&mut self, id: usize, text: &str) -> bool {
        if !self.steerable(id) {
            return false;
        }
        let job = &mut self.jobs[id - 1];
        let Kind::Agent { agent, .. } = job.kind else { return false };
        // a line that names a connector needs a turn started with them
        if names_connector(text) && !job.connectors {
            return false;
        }
        let line = match agent.spec().adapter {
            Adapter::Claude => serde_json::json!({"type": "user", "message": {"role": "user", "content": text}}),
            Adapter::Codex => match &job.codex_turn {
                Some((thread, turn)) => serde_json::json!({"id": parse::STEER_ID + job.steered.len() as u64,
                    "method": "turn/steer", "params": {"threadId": thread, "expectedTurnId": turn,
                                                       "input": [{"type": "text", "text": text}]}}),
                None => return false,
            },
            _ => return false,
        };
        let Some(input) = &job.input else { return false };
        let mut gate = job.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.closed || input.send(Some(format!("{line}\n"))).is_err() {
            return false;
        }
        gate.sent += 1;
        drop(gate);
        job.steered.push(text.to_string());
        // the owner's words, for Ringo's record of the conversation
        job.title_full = format!("{}\n\nthen, while it worked: {text}", job.title_full.trim_end());
        true
    }

    /// Whether job `id` was stopped (Esc, /kill).
    pub fn stopped(&self, id: usize) -> bool {
        self.jobs.get(id.wrapping_sub(1)).is_some_and(|j| j.cancel.load(Ordering::SeqCst))
    }

    /// Each agent's conversation, by name: what /restart hands the next shell so the conversations go on.
    pub fn threads(&self) -> Vec<(String, String)> {
        self.threads.iter().map(|(a, id)| (a.name().to_string(), id.clone())).collect()
    }

    /// What /restart hands the next shell for each agent: Ringo's record of the turns its next turn would have been
    /// told here (the other agents' since its own last one; all of them for one with no conversation yet), so
    /// restarting drops nothing from the shared conversation and adds nothing to it.
    pub fn carry(&self) -> Vec<(String, String)> {
        Agent::all().into_iter().filter_map(|agent| {
            let lane = self.lane(agent, self.jobs.len() + 1);
            let turns = match self.threads.contains_key(&agent) { true => self.unheard(agent, &lane), false => lane };
            let record = self.record(&turns).0;
            // Bounded together: each restart added a record to what the one before carried, without limit, for an
            // agent that never took a turn in between. The newest stay, as in a fresh start's record.
            // ceiling: the turns cut here are in no file the next shell is pointed to.
            let told = self.told.get(&agent).map_or(String::new(), |before| {
                format!("{}\n", newest(before.trim_end(), RECORD_CHARS.saturating_sub(record.len())))
            });
            (!turns.is_empty() || !told.is_empty()).then(|| (agent.name().to_string(), told + &record))
        }).collect()
    }

    /// What else /restart hands the next shell, so each agent's next turn there starts fresh or goes on as it would
    /// have here: how full its conversation is, and Ringo's record of the turns it holds. Without them, a conversation
    /// past `handoff_at` went on after every restart (two turns re-read 98k and 155k at every call). The record uncut
    /// is written to this shell's folder, and the record handed on ends with where.
    pub fn carry_record(&self) -> Vec<(String, u64, String)> {
        Agent::all().into_iter().filter_map(|agent| {
            let fills = self.context.get(&agent).copied().unwrap_or(0);
            let (record, whole) = self.fresh_record(agent, &self.lane(agent, self.jobs.len() + 1));
            let file = self.dir.join(format!("record-{}.md", agent.name()));
            let record = match record.is_empty() || fs::write(&file, whole).is_err() {
                true => record,
                false => format!("{record}\n(these turns uncut: {})", file.display()),
            };
            (fills > 0 || !record.is_empty()).then(|| (agent.name().to_string(), fills, record))
        }).collect()
    }

    /// Goes on with what the shell before a /restart handed on: each agent's conversation, what it had yet to be told,
    /// and how full that conversation is with Ringo's record of it (agents no longer set up are left out).
    pub fn resume(&mut self, threads: &[(String, String)], told: &[(String, String)], records: &[(String, u64, String)]) {
        for (name, id) in threads {
            if let Some(agent) = Agent::from_name(name) {
                self.threads.insert(agent, id.clone());
            }
        }
        for (name, record) in told {
            if let Some(agent) = Agent::from_name(name).filter(|_| !record.trim().is_empty()) {
                self.told.insert(agent, record.clone());
            }
        }
        for (name, fills, record) in records {
            let Some(agent) = Agent::from_name(name) else { continue };
            if *fills > 0 {
                self.context.insert(agent, *fills);
            }
            if !record.trim().is_empty() {
                self.before.insert(agent, record.clone());
            }
        }
    }

    /// Goes on from a kept conversation's file (`keep`, or what /restart handed on): each agent's conversation, what
    /// it had yet to hear and Ringo's record of it. The file's contents, for the text it names; None if unreadable.
    pub fn go_on(&mut self, path: &Path) -> Option<serde_json::Value> {
        let kept: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let list = |key: &str| serde_json::from_value::<Vec<(String, String)>>(kept[key].clone()).unwrap_or_default();
        // a shell before records were handed on has none: its conversations go on as they did
        let records: Vec<(String, u64, String)> = serde_json::from_value(kept["records"].clone()).unwrap_or_default();
        self.resume(&list("threads"), &list("told"), &records);
        // The running total each conversation had reported, so its next turn here counts only what it adds: without
        // it a resumed Codex or agy turn was counted with all of its conversation's usage again. A shell before these
        // were kept has none, and its first resumed turn is counted as before.
        let totals: Vec<(String, String, [u64; 4])> = serde_json::from_value(kept["usage"].clone()).unwrap_or_default();
        for (name, thread, [input, cache_write, cache_read, output]) in totals {
            if let Some(agent) = Agent::from_name(&name).filter(|a| self.threads.get(a) == Some(&thread)) {
                self.usage_so_far.insert((agent, thread), Tokens { input, cache_write, cache_read, output });
            }
        }
        self.source = Some(path.to_path_buf());
        self.about = kept["about"].as_str().unwrap_or_default().to_string();
        self.separate = kept["separate"].as_bool().unwrap_or(false);
        self.reached_before = list_names(&kept["reached"]).filter_map(Agent::from_name).collect();
        Some(kept)
    }

    /// Saves what a later shell goes on from, as /restart hands it on, with this folder and the owner's latest message
    /// for `ringo --resume` to list it by. The shell keeps it after every turn and when it ends, so a crash loses at
    /// most the turn it cut short. The conversation it went on from is this one from then on, so that one's file goes.
    /// ceiling: a turn cut short by a crash may have started a new conversation; the kept one is the last turn's
    pub fn keep(&self, conversation: Option<&Path>) -> Result<PathBuf, String> {
        let about = self.jobs.iter().rev().find(|j| matches!(j.kind, Kind::Agent { .. })).map_or(self.about.clone(), |j| j.title.clone());
        let saved = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let reached: Vec<&str> = Agent::all().into_iter().filter(|&a| self.reached(a, self.jobs.len() + 1)).map(|a| a.name()).collect();
        // each conversation's running total (see `go_on`), for the conversations handed on only
        let mut usage: Vec<(&str, &str, [u64; 4])> = self.usage_so_far.iter()
            .filter(|((agent, thread), _)| self.threads.get(agent) == Some(thread))
            .map(|((agent, thread), t)| (agent.name(), thread.as_str(), [t.input, t.cache_write, t.cache_read, t.output])).collect();
        usage.sort();
        let kept = serde_json::json!({"threads": self.threads(), "told": self.carry(), "records": self.carry_record(),
                                      "separate": self.separate, "reached": reached, "usage": usage,
                                      "conversation": conversation, "folder": self.repo, "about": about, "saved": saved});
        let (path, part) = (self.dir.join(KEPT), self.dir.join(format!("{KEPT}.part")));
        // written whole, then renamed: a crash mid-write leaves the last one
        fs::write(&part, kept.to_string()).and_then(|_| fs::rename(&part, &path)).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(from) = self.source.as_deref().and_then(Path::parent).filter(|d| *d != self.dir) {
            let _ = fs::rename(from.join(KEPT), from.join("resumed.json"));
        }
        Ok(path)
    }

    /// `agent`'s own turn running here, which its next one waits for. The agents work side by side, together or
    /// separate: together, each is told what the others did as it starts, and what they are still doing.
    pub fn busy_for(&self, agent: Agent) -> Option<usize> {
        self.jobs.iter().rev().find(|j| j.running() && j.kind == Kind::Agent { agent, write: false }).map(|j| j.id)
    }

    /// Whether the agent has a conversation to continue (a turn of it in the folder has started one).
    pub fn has_conversation(&self, agent: Agent) -> bool {
        self.threads.contains_key(&agent)
    }

    /// `agent`'s next turn starts its conversation again from Ringo's record of it (see `handoff`), not from the one
    /// its program keeps: for what that one holds on to, as its model does (checked live, Claude Code 2.1.322 and
    /// codex-cli 0.167.0: resumed with no model named, each ran the model it was last given). False when it had none.
    pub fn start_over(&mut self, agent: Agent) -> bool {
        let Some(old) = self.threads.remove(&agent) else { return false };
        self.usage_so_far.remove(&(agent, old));
        self.context.remove(&agent);
        true
    }

    /// The agents `agent`'s turn may ask (`peers`): the others set up that work in this folder and are installed.
    fn askable(&self, agent: Agent) -> Vec<Agent> {
        self.peers.iter().copied().filter(|a| *a != agent && a.spec().read && self.installed(*a)).collect()
    }

    /// Starts what the running turns have asked of other agents since the last look (`serve_asks` wrote each ask
    /// into its turn's folder). The agent asked takes a turn of its own, in read mode whatever the session's and
    /// without the means to ask in its turn; its reply is the answer (`end_with`). An ask that cannot be served is
    /// answered at once with the reason, which the asking agent reads and works around. True when one came.
    fn take_asks(&mut self) -> bool {
        let asking: Vec<usize> = self.jobs.iter().filter(|j| j.running() && j.can_ask).map(|j| j.id).collect();
        let mut any = false;
        for id in asking {
            let n = self.jobs[id - 1].asked + 1;
            let asks = self.jobs[id - 1].dir.join("asks");
            let Ok(bytes) = fs::read(asks.join(format!("{n}.json"))) else { continue };
            any = true;
            self.jobs[id - 1].asked = n;
            // the server writes each ask whole; one that does not read is answered, not waited on
            let ask = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default();
            let Kind::Agent { agent: asker, .. } = self.jobs[id - 1].kind else { continue };
            let (name, question) = (ask["agent"].as_str().unwrap_or(""), ask["question"].as_str().unwrap_or(""));
            let (answer, others) = (asks.join(format!("{n}.answer")), self.askable(asker));
            let target = match Agent::from_name(name).filter(|a| others.contains(a)) {
                _ if n > MAX_ASKS => Err(format!("this turn has asked {MAX_ASKS} times already: go on with what you have")),
                None => Err(format!("there is no agent {name} to ask here: {}",
                                    others.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "))),
                Some(target) if self.busy_for(target).is_some() =>
                    Err(format!("{name} is at work on something else: go on without it")),
                Some(target) => Ok(target),
            };
            let started = target.and_then(|target| {
                let prompt = format!("{0} is in the middle of a turn for me and asks you:\n\n{question}\n\nAnswer {0} directly \
                                      and briefly. Look at the files if it helps; change nothing.", asker.name());
                let (mode, peers) = (std::mem::replace(&mut self.mode, Mode::Read), std::mem::take(&mut self.peers));
                let started = self.launch_agent(target, false, &prompt);
                (self.mode, self.peers) = (mode, peers);
                started.map_err(|e| format!("{name} could not be asked: {e}"))
            });
            match started {
                Ok(asked) => {
                    let job = &mut self.jobs[asked - 1];
                    job.asked_by = Some((id, answer));
                    // what it was told of the turns it missed is in its log (/show N); the conversation shows its answer
                    job.fresh.retain(|said| !matches!(said, Said::Line(_)));
                    self.jobs[id - 1].push_log(format!("asks {name}: {}", parse::short(question, 200)));
                }
                Err(why) => {
                    self.jobs[id - 1].push_log(format!("asked {name}, and was told: {why}"));
                    answer_ask(&answer, false, &why);
                }
            }
        }
        any
    }

    /// A new conversation: the next turn of each agent starts fresh instead of resuming (`/clear`).
    pub fn forget(&mut self) {
        self.threads.clear();
        self.context.clear();
        self.heard.clear();
        self.told.clear();
        self.before.clear();
        self.reached_before.clear();
        self.start = self.jobs.len() + 1;
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

    /// Whether the owner's own Codex instructions (the first of AGENTS.override.md and AGENTS.md with any text in it)
    /// send Codex to Ringo: a turn Ringo launched is then told it is already in Ringo (see CODEX_IN_RINGO). Not when
    /// the owner's Codex settings give developer instructions of their own, which the thread's would replace.
    /// ceiling: the settings are scanned for the key, not parsed; such an owner's turns still follow the file.
    /// Whether the owner's own Codex config.toml sets `key` (theirs wins over a setting of Ringo's).
    /// ceiling: a key under a profile counts as set too.
    fn codex_sets(&self, key: &str) -> bool {
        fs::read(self.codex_home.join("config.toml")).is_ok_and(|b| String::from_utf8_lossy(&b).lines()
            .any(|l| l.trim_start().strip_prefix(key).is_some_and(|rest| rest.trim_start().starts_with('='))))
    }

    fn codex_told_to_use_ringo(&self) -> bool {
        let read = |name: &str| fs::read(self.codex_home.join(name)).map(|b| String::from_utf8_lossy(&b).to_lowercase());
        let own = read("config.toml").is_ok_and(|t| t.lines().any(|l| l.trim_start().starts_with("developer_instructions")));
        !own && ["AGENTS.override.md", "AGENTS.md"].into_iter().filter_map(|f| read(f).ok())
            .find(|text| !text.trim().is_empty()).is_some_and(|text| text.contains("ringo"))
    }

    /// Ringo's state folder for this session: agents.json and rules.md are there.
    pub fn state(&self) -> &Path {
        &self.root
    }

    /// The owner's own rules for every Claude turn: a file they write in Ringo's state folder. With none, a turn is as
    /// it always was.
    pub fn rules_path(&self) -> PathBuf {
        self.root.join("rules.md")
    }

    /// Whether /undo works here: a git project, or Ringo's own store for the folder.
    pub fn undoable(&self) -> bool {
        self.git || self.store.is_some()
    }

    /// A commit holding `tree`, on `parent` when there is one; no branch points at it. It goes where the folder's
    /// trees are: the project's repository, or outside one Ringo's undo store.
    fn commit(&self, tree: &str, parent: Option<&str>, message: &str) -> Result<String, String> {
        let mut args = vec!["-c", "user.name=ringo", "-c", "user.email=ringo@localhost", "commit-tree", "--no-gpg-sign",
                            tree, "-m", message];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        self.folder_git(&args)
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
        let mut args = vec!["diff", "--binary", after.as_str(), before.as_str(), "--"];
        args.extend(changed.iter().map(String::as_str)); // its own files only (see `end_with`)
        fs::write(&file, run_git(self.folder(), &args)?).map_err(|e| e.to_string())?;
        let file = file.to_string_lossy().into_owned();
        self.folder_git(&["apply", "--check", &file])
            .map_err(|e| format!("#{id}'s files no longer match it, so nothing was undone ({e})"))?;
        self.folder_git(&["apply", &file])?;
        let job = &mut self.jobs[id - 1];
        job.undone = true;
        Ok(format!("undid #{id}: {} back as before", job.changed.join(", ")))
    }

    /// The finished turns here that `agent`'s conversation holds, before job `upto`, oldest first: since /clear, every
    /// agent's turns in the shared conversation and its own; in /separate its own alone. /compact is no turn of it.
    fn lane(&self, agent: Agent, upto: usize) -> Vec<&Job> {
        self.jobs[self.start - 1..upto - 1].iter().filter(|j| match j.kind {
            Kind::Agent { agent: a, write: false } => !j.running() && j.title_full.trim() != "/compact"
                && (a == agent || (!self.separate && j.lane.is_none())),
            _ => false,
        }).collect()
    }

    /// Whether the last turn of `agent`'s conversation before job `upto` called a connector, here or, with none here
    /// yet, before a restart or `ringo --resume`.
    fn reached(&self, agent: Agent, upto: usize) -> bool {
        match self.lane(agent, upto).last() {
            Some(j) => j.reached,
            None => self.reached_before.contains(&agent),
        }
    }

    /// The turns in `lane` that `agent`, going on with its conversation, has not been told: the other agents'. Agents
    /// work side by side, so a turn can end after a later one began: this is no position in the lane.
    fn unheard<'a>(&self, agent: Agent, lane: &[&'a Job]) -> Vec<&'a Job> {
        let heard = self.heard.get(&agent);
        lane.iter().copied().filter(|j| !matches!(j.kind, Kind::Agent { agent: a, .. } if a == agent)
            && !heard.is_some_and(|h| h.contains(&j.id))).collect()
    }

    /// The other agents' turns still at work in this folder as `agent`'s turn `id` starts, in the shared conversation:
    /// one line each, its task and the files it has edited so far, so the new turn neither redoes nor undoes them.
    /// It hears how they ended at its next turn, as any turn it was away for.
    fn in_flight(&self, id: usize, agent: Agent) -> Option<String> {
        // a turn started in /separate is its agent's own, even after /together
        let shared = |j: &&Job| !self.separate && j.id != id && j.running() && j.lane.is_none();
        let busy: Vec<String> = self.jobs.iter().filter(shared).filter_map(|j| {
            let Kind::Agent { agent: other, write: false } = j.kind else { return None };
            let task: String = j.title_full.split_whitespace().collect::<Vec<_>>().join(" ");
            let task = match task.char_indices().nth(300) { Some((at, _)) => format!("{}…", &task[..at]), None => task };
            let edited = match j.touched.is_empty() {
                true => String::new(),
                false => format!("; edited so far: {}", files(&j.touched)),
            };
            (other != agent).then(|| format!("#{} {}, asked: \"{task}\"{edited}", j.id, other.name()))
        }).collect();
        (!busy.is_empty()).then(|| format!("Still at work in this folder as you start, so leave their work to them; \
                                            you hear how they end at your next turn:\n{}\n\n", busy.join("\n")))
    }

    /// What `agent`'s turn `id` is sent instead of the owner's `prompt` alone, and a line for its log. In the shared
    /// conversation, a conversation it continues hears what the other agents did that it has not been told, and one
    /// it starts hears all of it; either way it hears which of their turns are still at work here. A conversation
    /// past `handoff_at` tokens starts fresh from the record (`resume` goes), when the record is at most half its
    /// size. Ringo's own code writes the record from what it kept of each turn: no model call, and never a summary of
    /// a summary. The record uncut goes in the job's folder.
    fn handoff(&mut self, id: usize, agent: Agent, resume: &mut Option<String>, prompt: &str) -> Option<(String, String)> {
        let fills = self.context.get(&agent).copied().unwrap_or(0);
        let lane = self.lane(agent, id);
        let unheard = self.unheard(agent, &lane);
        // a fresh start that would save too little resumes, told what the others did since, as any other turn
        let picked = [resume.is_none() || fills > self.handoff_at, false].into_iter().find_map(|fresh| {
            let turns = if fresh { &lane[..] } else { &unheard[..] };
            let restarted = fresh && self.carried(agent).is_some();
            let span = match (turns.first(), turns.last()) {
                (Some(f), Some(l)) if f.id == l.id => format!("#{}", f.id),
                (Some(f), Some(l)) => format!("#{} to #{}", f.id, l.id),
                _ if restarted => String::new(),
                _ => return None,
            };
            let span = match (restarted, span.is_empty()) {
                (true, true) => "the turns before the restart".to_string(),
                (true, false) => format!("the turns before the restart and {span}"),
                _ => span,
            };
            let (record, whole) = if fresh { self.fresh_record(agent, turns) } else { self.record(turns) };
            let head = match fresh {
                true => "Ringo's record of this conversation so far, which you start fresh from",
                false => "While you were away, other agents took turns in this conversation. Ringo's record of them",
            };
            // agy is not told where the whole record is: it is outside its folder, and a read agy has no permission
            // for ends its turn with no reply (live, 11/10: it read the project's files, opened record.md and stopped,
            // 82.5k tokens in). The others are refused such a read and go on, or may make it.
            let whole_at = match agent.spec().adapter {
                Adapter::Agy => String::new(),
                _ => format!(" The whole record, uncut, is in {}.", self.jobs[id - 1].dir.join("record.md").display()),
            };
            let text = format!("{head} (the owner's words are exact; the replies and steps are the agents' own account \
                                and can be wrong; the files here are the truth).{whole_at}\n\n{}\n\n", record.trim_end());
            let about = text.len() as u64 / 4; // the owner's message goes either way
            let ids: Vec<usize> = turns.iter().map(|j| j.id).collect();
            (!fresh || resume.is_none() || about * 2 <= fills).then_some((fresh, text, about, span, whole, ids))
        });
        let busy = self.in_flight(id, agent);
        let heard = self.heard.entry(agent).or_default();
        match &picked {
            Some((true, .., ids)) => *heard = ids.iter().copied().collect(),
            Some((.., ids)) => heard.extend(ids),
            None if resume.is_none() => heard.clear(), // a new conversation, with nothing to tell it
            None => {}
        }
        let Some((fresh, text, about, span, whole, _)) = picked else {
            return busy.map(|busy| (format!("{busy}The owner's new message:\n{prompt}"),
                                    format!("told {} which turns are still at work here", agent.name())));
        };
        let file = self.jobs[id - 1].dir.join("record.md");
        let note = match (fresh, resume.is_some()) {
            (true, true) => format!("handoff: {} starts fresh from Ringo's record of {span} (about {} tokens) instead \
                                     of re-reading {} at every call", agent.name(), fmt_k(about), fmt_k(fills)),
            (true, false) => format!("told {} the conversation so far, {span} (about {} tokens)", agent.name(), fmt_k(about)),
            _ => format!("told {} what {span} did (about {} tokens)", agent.name(), fmt_k(about)),
        };
        let note = if busy.is_some() { note + ", and which turns are still at work here" } else { note };
        let text = format!("{text}{}The owner's new message:\n{prompt}", busy.unwrap_or_default());
        if let Err(e) = fs::write(&file, whole) {
            self.jobs[id - 1].push_log(format!("the whole record could not be kept: {e}"));
        }
        if fresh {
            self.told.remove(&agent); // the turns before the restart it had not heard are in the record (`carried`)
        }
        if fresh && let Some(old) = resume.take() {
            self.threads.remove(&agent);
            self.usage_so_far.remove(&(agent, old));
            self.context.remove(&agent);
        }
        Some((text, note))
    }

    /// The turns before a /restart that `agent`'s fresh start begins with: the record of its conversation there, or,
    /// from a shell that handed on no records, the turns it had yet to be told (its own were never recorded).
    fn carried(&self, agent: Agent) -> Option<&String> {
        self.before.get(&agent).or_else(|| self.told.get(&agent))
    }

    /// Ringo's record for `agent`'s fresh start: the turns before a /restart that its conversation held, if any, then
    /// `turns`, as `record` gives them; the older turns are cut first.
    fn fresh_record(&self, agent: Agent, turns: &[&Job]) -> (String, String) {
        let (record, whole) = self.record(turns);
        let Some(before) = self.carried(agent) else { return (record, whole) };
        let join = |a: &str, b: &str| match b.is_empty() {
            true => a.trim_end().to_string(),
            false => format!("{}\n\n(Ringo restarted here; the turns since are numbered from #1 again)\n\n{b}", a.trim_end()),
        };
        (join(&newest(before, RECORD_CHARS.saturating_sub(record.len())), &record), join(before, &whole))
    }

    /// Ringo's record of `turns`, oldest first, and the same uncut: each turn's message from the owner, word for
    /// word; the agent's reply (the newest whole, older ones' first paragraph); its failed steps and its last ones;
    /// the files it changed; then the change the turns made, as a diff. Newest first into RECORD_CHARS; an older turn
    /// past it is left to the whole record.
    fn record(&self, turns: &[&Job]) -> (String, String) {
        let cut = |text: &str, n: usize| match text.char_indices().nth(n) {
            Some((at, _)) => format!("{}… (cut; the whole is in the record)", &text[..at]),
            None => text.to_string(),
        };
        let render = |j: &Job, whole: bool, full: bool| {
            let who = match j.kind { Kind::Agent { agent, .. } => agent.name(), Kind::Ringo { .. } => "ringo" };
            let said = j.title_full.trim();
            let mut part = format!("#{} · the owner to {who}: {}\n", j.id, if full { said.to_string() } else { cut(said, 4000) });
            let reply = j.reply();
            let reply = reply.trim();
            part += &match &j.status {
                Status::Done if full => format!("{who}: {reply}\n"),
                Status::Done if whole => format!("{who}: {}\n", cut(reply, 6000)),
                Status::Done => format!("{who}: {}\n", cut(reply.split("\n\n").next().unwrap_or(""), 600)),
                Status::Failed(why) => format!("{who}: (failed: {why})\n"),
                _ => format!("{who}: (stopped before it finished)\n"),
            };
            let failed: Vec<&str> = j.steps.iter().map(String::as_str).filter(|s| s.starts_with(parse::FAILED)).collect();
            let steps: Vec<&str> = match full {
                true => j.steps.iter().map(String::as_str).collect(),
                false => failed.iter().copied().take(5)
                    .chain(j.steps.iter().rev().take(if whole { 5 } else { 0 }).rev().map(String::as_str)
                        .filter(|s| !s.starts_with(parse::FAILED))).collect(),
            };
            if !steps.is_empty() {
                part += &format!("steps ({} in all): {}\n", j.steps.len(), steps.join(" | "));
            }
            if !j.changed.is_empty() {
                part += &format!("changed: {}{}\n", j.changed.join(", "), if j.undone { " (taken back with /undo)" } else { "" });
            } else if !j.touched.is_empty() {
                // nothing git saw as its own: no snapshot of the folder (no /undo here: the home folder, a large one),
                // or another turn at work here at the same time ran commands; the files it said it edited
                part += &format!("edited (as it reported): {}\n", files(&j.touched));
            }
            part
        };
        let mut kept = Vec::new();
        let mut used = 0;
        for (i, j) in turns.iter().enumerate().rev() {
            let part = render(j, i + 1 == turns.len() || j.talk || j.asked_by.is_some(), false);
            if used + part.len() > RECORD_CHARS && !kept.is_empty() {
                kept.push(format!("(#{} to #{}, {} earlier turns, are in the whole record)\n", turns[0].id, j.id, i + 1));
                break;
            }
            used += part.len();
            kept.push(part);
        }
        kept.reverse();
        let mut whole: Vec<String> = turns.iter().map(|j| render(j, true, true)).collect();
        // The change the turns made: from before the first to after the one that ended last (agents work side by
        // side, so that need not be the last in the list), in their files only.
        // ceiling: a file one of these turns changed that a turn left out of them (its agent's own) changed too
        // shows that turn's lines as well.
        let files: BTreeSet<&str> = turns.iter().flat_map(|j| j.changed.iter().map(String::as_str)).collect();
        let first = turns.iter().find_map(|j| j.before.as_deref());
        let last = turns.iter().filter(|j| j.after.is_some()).max_by_key(|j| j.started + j.age()).and_then(|j| j.after.as_deref());
        if let (Some(first), Some(last), false) = (first, last, files.is_empty()) {
            let mut args = vec!["diff", first, last, "--"];
            args.extend(&files);
            if let Ok(diff) = run_git(self.folder(), &args).map(|d| String::from_utf8_lossy(&d).into_owned()) {
                let room = RECORD_CHARS.saturating_sub(used).clamp(2_000, 16_000);
                kept.push(format!("The change these turns made, as git shows it:\n{}", cut(&diff, room)));
                whole.push(format!("The change these turns made, as git shows it:\n{diff}"));
            }
        }
        (kept.join("\n"), whole.join("\n"))
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
        self.jobs.push(Job { id, kind, title: parse::short(title, 60), title_full: title.to_string(), status: Status::Running, started: Instant::now(), tier: self.tier,
                             elapsed: None, activity: "starting".into(), tokens: Tokens::default(), live: Tokens::default(), calls: 0,
                             subs: Vec::new(), log: VecDeque::new(), fresh: Vec::new(), dir, worktree: None, base: None, applied: false, unchanged: false,
                             touched: BTreeSet::new(), before: None, after: None, changed: Vec::new(), undone: false,
                             asks: VecDeque::new(), lane: None, steps: Vec::new(), ran: false, input: None, pid: None,
                             cancel: Arc::new(AtomicBool::new(false)), steered: Vec::new(), gate: Arc::default(),
                             codex_turn: None, connectors: false, reached: false, model: None, talk: false, can_ask: false, asked: 0, asked_by: None, counted: false,
                             uncounted: 0, so_far: Tokens::default(), told_now: (Vec::new(), None) });
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
        if !write && let Some(busy) = self.busy_for(agent) {
            return Err(format!("busy: {name}'s #{busy} is working in this folder; one turn of each agent's at a time"));
        }
        // A turn in the owner's folder edits it, unless in read mode; a write turn edits its own copy.
        let edits = write || self.mode != Mode::Read;
        // a specialist of the owner's: what makes it one, or no turn at all
        let (own, own_model, own_tools) = match own_file(spec)? {
            Some(file) => (Some(file.text), file.model, file.tools),
            None => (None, None, None),
        };
        let id = self.new_job(Kind::Agent { agent, write }, prompt)?;
        let dir = self.jobs[id - 1].dir.clone();
        let _ = fs::write(dir.join("prompt.txt"), prompt);
        if let Some(own) = &own && let Err(e) = fs::write(dir.join("agent.md"), own) {
            let e = format!("{name}'s instructions could not be kept for its turn: {e}");
            self.end(id, Status::Failed(e.clone()));
            return Err(e);
        }
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
        let mut resume = if write { None } else { self.threads.get(&agent).cloned() };
        let mut sent = prompt.to_string();
        // put back if the turn never starts: what the handoff took as heard, and the turns before a restart it took as told
        let (heard, untold) = (self.heard.get(&agent).cloned(), self.told.get(&agent).cloned());
        let connectors = prompt.trim() != "/compact"
            && (names_connector(prompt) || (!write && self.reached(agent, id)));
        if !write {
            self.jobs[id - 1].lane = self.separate.then_some(agent);
            if prompt.trim() != "/compact" && let Some((text, note)) = self.handoff(id, agent, &mut resume, prompt) {
                sent = text;
                self.jobs[id - 1].push_log(note);
            }
            // the turns before a /restart that this agent had not heard, once, ahead of anything since
            if prompt.trim() != "/compact" && let Some(before) = self.told.remove(&agent) {
                let rest = if sent == prompt { format!("The owner's new message:\n{prompt}") } else { sent };
                sent = format!("Before Ringo restarted, other turns were taken in this conversation. Ringo's record of \
                                them (the owner's words are exact; the replies and steps are the agents' own account and \
                                can be wrong; the files here are the truth):\n\n{}\n\n{rest}", before.trim_end());
                self.jobs[id - 1].push_log(format!("told {} the turns before the restart (about {} tokens)", agent.name(),
                                                   fmt_k(before.len() as u64 / 4)));
            }
        }
        // The connectors load for a message that names one, and for the conversation's turn after one that called one
        // (an @agent! turn starts fresh, so it does not continue one) ("and mine?"
        // after a ClickUp question): without them that follow-up searched Ringo's own logs for the earlier answer,
        // 104k tokens and three minutes, and said the data was a day old. Claude's load outside read mode, Codex's in
        // every mode.
        if connectors {
            self.jobs[id - 1].connectors = spec.adapter == Adapter::Codex || (spec.adapter == Adapter::Claude && self.mode != Mode::Read);
            sent.push_str(match spec.adapter {
                Adapter::Claude if self.mode != Mode::Read => CLAUDE_CONNECTOR_NOTE,
                Adapter::Codex => CODEX_CONNECTOR_NOTE,
                _ => "",
            });
        }
        let sandbox = if edits { "workspace-write" } else { "read-only" };
        // `sent` is what the agent gets; the owner's message (`prompt`) stays the job's title, prompt.txt and what
        // decides the turn's settings
        let (mut input, mut turn) = (Some(sent.clone()), None);
        let style = match spec.adapter {
            Adapter::Codex => {
                // One `codex app-server` per turn, so the reply streams as it is written: the same agent and settings
                // as `codex exec`, and the same model input, compared request for request against a local stand-in
                // for the API (codex-cli 0.165.0: a new turn in each mode, and a resumed one). The thread's settings
                // go in thread/start or thread/resume; turn/start follows once the thread's id is known.
                cmd.arg("app-server");
                // The means to ask another agent here in the middle of the turn, as Claude's turn is given them
                // (`serve_asks`): one MCP server in Codex's settings, for this run only. The values are TOML, and a
                // JSON string or list of strings reads as the same TOML.
                let others = if write || prompt.trim() == "/compact" { Vec::new() } else { self.askable(agent) };
                if !others.is_empty() {
                    let mut args = vec!["ask-server".to_string(), dir.to_string_lossy().into_owned()];
                    args.extend(others.iter().map(|a| a.name().to_string()));
                    cmd.args(["-c", &format!("mcp_servers.ringo.command={}", serde_json::json!(self.asker))]);
                    cmd.args(["-c", &format!("mcp_servers.ringo.args={}", serde_json::json!(args))]);
                    // Its one tool is approved ahead: a turn that may not ask for approvals (read and ask mode) was
                    // otherwise refused it ("ask_agent required approval, but this session's policy disallows
                    // approvals", codex-cli 0.167.0). The turn it starts looks and does not touch.
                    cmd.args(["-c", "mcp_servers.ringo.default_tools_approval_mode=\"approve\""]);
                    self.jobs[id - 1].can_ask = true;
                }
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
                if !connectors {
                    cmd.args(["-c", "features.plugins=false", "-c", "features.apps=false"]);
                }
                // Image generation, and goals (an aim Codex pursues by itself over many turns), are tools no turn here
                // uses: 14,733 tokens a trivial turn with them, 13,564 without (codex-cli 0.166.0).
                cmd.args(["-c", "features.image_generation=false", "-c", "features.goals=false"]);
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
                }
                // Where Claude's turns compact (COMPACT_AT): Codex's own limit is near its 475k window, so one long
                // turn here read 2.9M tokens with its calls still growing past 137k each (codex-cli 0.166.0). An owner
                // whose config.toml sets the limit keeps their own.
                if !self.codex_sets("model_auto_compact_token_limit") {
                    cmd.args(["-c", &format!("model_auto_compact_token_limit={COMPACT_AT}")]);
                }
                let mut thread = serde_json::json!({"cwd": cwd, "sandbox": sandbox,
                                                    "approvalPolicy": if auto { "on-request" } else { "never" }});
                if auto {
                    thread["approvalsReviewer"] = "auto_review".into();
                }
                // the owner's choice of model (/model); given on resuming too, so a conversation goes on with it
                if let Some(model) = self.models.get(&agent) {
                    thread["model"] = model.as_str().into();
                }
                // Kept by the thread once it is given; given on resuming too, for a thread started without it. A
                // specialist's instructions go the same way, ahead of it: for its turns they stand in for any
                // developer instructions in the owner's Codex settings, which the thread's replace.
                let told = self.codex_told_to_use_ringo().then_some(CODEX_IN_RINGO);
                match (&own, told) {
                    (Some(own), told) => thread["developerInstructions"] =
                        [&*String::from_utf8_lossy(own), told.unwrap_or("")].join("\n\n").trim_end().into(),
                    (None, Some(told)) => thread["developerInstructions"] = told.into(),
                    (None, None) => {}
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
                                               "params": {"input": [{"type": "text", "text": sent}]}}));
                Style::Codex
            }
            Adapter::Claude => {
                // The prompt goes in on stdin as one stream-json message, and stdin stays open until the result: a
                // permission request comes out as a control_request and Ringo writes the owner's answer back, and a
                // line the owner sends while it works goes in as another message (`steer`), echoed back when Claude
                // takes it in (--replay-user-messages), so stdin closes only after it is answered.
                // The reply streams as it is written (--include-partial-messages); neither changes any tokens.
                cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                          "--include-partial-messages", "--replay-user-messages"]);
                // whom this turn may ask in the middle of its work (see below); a turn in its own copy hears no one
                // Only when the owner's message names one of them: the tool takes the turn out of safe mode (below),
                // and every other turn of Claude's stays as it was, to the argument.
                let others = if write || prompt.trim() == "/compact" { Vec::new() } else { self.askable(agent) };
                let others: Vec<Agent> = others.into_iter().filter(|_| names_agent(prompt, &self.askable(agent))).collect();
                let can_ask = !others.is_empty();
                // a turn that would have run in safe mode, and has the tool instead (see below)
                let lean_asks = can_ask && !(self.mode != Mode::Read && (spec.skills || connectors));
                if self.mode != Mode::Read && (spec.skills || connectors) {
                    // The owner's connectors (MCP servers, plugins) load, but not their CLAUDE.md, memory, skills or
                    // main agent: 27.5k tokens a trivial turn instead of 57.8k with all of it (Claude Code 2.1.316).
                    // Without CLAUDE.md the owner's instruction to use Ringo cannot start Ringo again either.
                    // A specialist the owner set up with skills keeps those too: its instructions name them, and the
                    // owner chose their cost for this agent's turns alone. Not in read mode, where no turn loads the
                    // owner's customizations: a connector's tool their own settings allow could write, and read
                    // mode's promise is that nothing can.
                    cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1").env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
                    cmd.args(["--agent", "claude"]);
                    if !spec.skills {
                        cmd.arg("--disable-slash-commands");
                    }
                } else {
                    // --safe-mode drops the owner's CLAUDE.md, skills, plugins, hooks and MCP servers: one trivial
                    // turn measured 16.6k tokens instead of 50.2k, with the subscription login and built-in subagents
                    // kept. The repository's own instructions are appended instead. Skills go too: safe mode still lists
                    // 20 of Claude Code's own (scheduling, loops, sending to other projects, plugin authoring...) in
                    // every call, 12,432 tokens a trivial turn with them and 9,320 without (Claude Code 2.1.316), and
                    // no turn in Ringo's record had called one. /compact is one of the commands, so its turn keeps them.
                    if prompt.trim() != "/compact" {
                        cmd.arg("--disable-slash-commands");
                    }
                    match can_ask {
                        false => cmd.arg("--safe-mode"),
                        // Safe mode would drop the one tool this turn is given with the rest (checked live, Claude
                        // Code 2.1.322: the tool was not there). So what it drops is turned off piece by piece: no
                        // CLAUDE.md or memory, no commands or skills (above), no main agent of the owner's, no hooks
                        // (in the settings below), and no MCP server but the one given below. The owner's settings
                        // files still load, as they do in safe mode: their deny rules, model and effort are theirs
                        // to keep. Skipping those files (--setting-sources "") was 361 tokens leaner and dropped the
                        // owner's own deny rules, so it was not taken. What is left over safe mode: the owner's
                        // plugins and agent files load (one trivial turn: 10,655 tokens against 10,069).
                        true => cmd.env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1").env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
                            .args(["--agent", "claude", "--strict-mcp-config"]),
                    };
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
                    let mut settings = serde_json::json!({"permissions": {"ask": self.always_ask},
                                                          "attribution": {"commit": "", "pr": ""}});
                    if lean_asks {
                        settings["disableAllHooks"] = true.into();
                    }
                    cmd.arg("--settings").arg(settings.to_string());
                } else if lean_asks {
                    cmd.arg("--settings").arg(r#"{"disableAllHooks":true}"#);
                }
                // The means to ask another agent here in the middle of the turn (`serve_asks`): one tool, served by
                // this program from the turn's own folder, allowed without asking in every mode, because the turn
                // it starts looks and does not touch. Not for a turn in its own copy, which hears no one.
                let mut allowed: Vec<String> = if self.mode != Mode::Read { self.allowed.iter().cloned().collect() } else { Vec::new() };
                if can_ask {
                    let mut args = vec!["ask-server".to_string(), dir.to_string_lossy().into_owned()];
                    args.extend(others.iter().map(|a| a.name().to_string()));
                    cmd.arg("--mcp-config")
                        .arg(serde_json::json!({"mcpServers": {"ringo": {"command": self.asker, "args": args}}}).to_string());
                    allowed.push("mcp__ringo__ask_agent".into());
                    self.jobs[id - 1].can_ask = true;
                }
                if !allowed.is_empty() {
                    cmd.arg("--allowedTools").args(&allowed);
                }
                // the owner's choice of model (/model): an alias or a full name, as Claude Code's --model takes it.
                // With none, a specialist runs on the model its file names, as it does in Claude Code.
                if let Some(model) = self.models.get(&agent).or(own_model.as_ref()) {
                    cmd.args(["--model", model]);
                }
                // A specialist's file may list the tools it uses: Claude Code then offers it those of its own and no
                // others. It can only narrow: what this mode denies above stays denied.
                if let Some(tools) = &own_tools {
                    cmd.arg("--tools").arg(tools.join(",")); // an empty list is Claude Code's "" for no tools
                }
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["--effort", effort]);
                }
                // A subscription login runs Opus with a 1M window, where Claude Code compacts a conversation only near
                // 967k: every call re-reads all of it, so one long turn here read 3.9M tokens with its last calls at
                // 140k each. A 200k window is where Claude Code compacts on a 200k model, by itself, mid-turn as well
                // as on resuming; the system prompt (with the files appended below) stays, and it re-reads the five
                // files last changed. An owner who sets the variable keeps their own.
                if std::env::var_os("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none() {
                    cmd.env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", COMPACT_AT.to_string());
                }
                if let Some(session) = &resume {
                    cmd.args(["--resume", session]);
                }
                // The owner's own rules (`rules_path`), which safe mode would otherwise drop with their CLAUDE.md, go
                // first; with the repository's file as well, the two go in one file of the job's, as bytes, so neither
                // needs to be UTF-8. Nothing here stops the turn: a file that cannot be read or written is said in its log.
                let doc = ["CLAUDE.md", "AGENTS.md"].iter().map(|d| cwd.join(d)).find(|p| p.is_file());
                let rules = match fs::read(self.rules_path()) {
                    Ok(bytes) => Some(bytes).filter(|b| !b.trim_ascii().is_empty()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        self.jobs[id - 1].push_log(format!("your rules.md is left out of this turn: {e}"));
                        None
                    }
                };
                let append = match (rules, &own, doc) {
                    (None, None, doc) => doc,
                    (Some(_), None, None) => Some(self.rules_path()),
                    (None, Some(_), None) => Some(dir.join("agent.md")),
                    // the owner's rules, then what makes this agent a specialist, then the project's own file
                    (rules, own, doc) => {
                        let both = dir.join("instructions.md");
                        let joined = doc.as_ref().map(fs::read).transpose().map(|text| {
                            [rules.as_deref().map(<[u8]>::trim_ascii_end), own.as_deref(), text.as_deref()]
                                .into_iter().flatten().collect::<Vec<_>>().join(&b"\n\n"[..])
                        });
                        match joined.and_then(|bytes| fs::write(&both, bytes)) {
                            Ok(()) => Some(both),
                            // a specialist's turn still carries what makes it one
                            Err(e) if own.is_some() => {
                                self.jobs[id - 1].push_log(format!("your rules.md and the project's instructions are left out of this turn: {e}"));
                                Some(dir.join("agent.md"))
                            }
                            Err(e) => {
                                self.jobs[id - 1].push_log(format!("your rules.md is left out of this turn: {e}"));
                                doc
                            }
                        }
                    }
                };
                if let Some(file) = append {
                    cmd.arg("--append-system-prompt-file").arg(file);
                }
                input = Some(format!("{}\n", serde_json::json!({"type": "user", "message": {"role": "user", "content": sent}})));
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
                // agy's default model has low, medium and high only: given xhigh, every turn failed at once (agy 1.4.12,
                // gemini-3.8-flash). ceiling: a model of agy's that has xhigh still gets high.
                if let Some(effort) = self.tier.effort() {
                    cmd.args(["--effort", if effort == "xhigh" { "high" } else { effort }]);
                }
                if let Some(conversation) = &resume {
                    cmd.args(["--conversation", conversation]);
                }
                input = Some(format!("{}\n", serde_json::json!({"event": "user", "message": {"content": sent}})));
                Style::Agy
            }
            Adapter::Plain => {
                // The entry's own command line, e.g. grok's documented headless flags (docs.x.ai/build/cli/
                // headless-scripting; unverified here). Grok's --always-approve runs every tool call unsandboxed
                // inside its worktree.
                for arg in &spec.args { // the prompt goes in last, so its own text is never filled in
                    cmd.arg(arg.replace("{cwd}", &cwd.to_string_lossy()).replace("{prompt}", &sent));
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
        if let Err(e) = self.spawn(id, cmd, parser, input, None) {
            // the agent never got this turn's handoff, so its next turn is told those turns again
            match heard {
                Some(heard) => self.heard.insert(agent, heard),
                None => self.heard.remove(&agent),
            };
            if let Some(untold) = untold {
                self.told.insert(agent, untold);
            }
            return Err(e);
        }
        // A turn that starts can still fail before its agent reads a word (`end_with` gives these back then).
        let before = heard.unwrap_or_default();
        let added = self.heard.get(&agent).map_or(Vec::new(), |now| now.difference(&before).copied().collect());
        self.jobs[id - 1].told_now = (added, untold.filter(|_| !self.told.contains_key(&agent)));
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
    /// no branch), reviewed by Ringo on `backend` with the task as the change description.
    pub fn review_job(&mut self, of: usize, backend: &str) -> Result<usize, String> {
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
            // Outside a git project the trees are in Ringo's undo store, and the review reads only git objects
            // (ls-tree, cat-file, diff), so it reads them from there.
            (_, _, Some(before), Some(after)) if !job.changed.is_empty() => {
                let base = self.commit(&before, None, &format!("ringo job {of}: before"))?;
                let head = self.commit(&after, Some(&base), &format!("ringo job {of}"))?;
                (self.store.clone().unwrap_or_else(|| self.repo.clone()), base, head)
            }
            _ => return Err(format!("#{of} changed no files to review")),
        };
        let job = self.job(of)?;
        let prompt = job.title_full.clone(); // the task with every line sent while it worked
        let dir = self.next_dir()?;
        let pr = dir.join("pr.txt");
        fs::write(&pr, format!("The task given to the agent:\n\n{prompt}\n")).map_err(|e| e.to_string())?;
        let args = self.review_args(&wt, &base, &head, &dir, Some(&pr), backend);
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
        let args = self.review_args(&self.repo, base, head, &dir, None, &self.backend);
        self.launch_ringo(&format!("review {base}..{head}"), args, dir.join("trace"))
    }

    fn review_args(&self, repo: &Path, base: &str, head: &str, dir: &Path, pr: Option<&Path>, backend: &str) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["review".into(), "--repo".into(), repo.into()];
        args.extend(["--base", base, "--head", head, "--backend", backend, "--timeout", "180"].map(Into::into));
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
        let asked = self.jobs[self.start - 1..].iter().rev() // since /clear: an earlier answer is not this conversation's
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
        let gate = self.jobs[id - 1].gate.clone();
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
                if parser.completed() && closer.is_some() {
                    // the result is in and every line of the owner's taken: nothing more will be asked, so the agent
                    // may exit
                    let mut gate = gate.lock().unwrap_or_else(|e| e.into_inner());
                    if parser.settled(gate.sent) && let Some(close) = closer.take() {
                        gate.closed = true;
                        let _ = close.send(None);
                    }
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
            // Its output ended: a line sent now goes next, and one it never took in (it stopped or failed first)
            // goes back.
            let (sent, taken) = {
                let mut gate = gate.lock().unwrap_or_else(|e| e.into_inner());
                gate.closed = true;
                (gate.sent, parser.taken())
            };
            for n in taken.min(sent)..sent {
                let _ = tx.send(Event::Note(id, Note::Refused(n)));
            }
            for note in parser.leftover() {
                let _ = tx.send(Event::Note(id, note));
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
        // A turn another agent asked for: what it said is that agent's answer. A turn that ends takes the turns it
        // asked for with it: no one is waiting for them.
        if let Some((_, answer)) = self.jobs[id - 1].asked_by.clone() {
            let who = match self.jobs[id - 1].kind { Kind::Agent { agent, .. } => agent.name(), _ => "it" };
            match &status {
                Status::Done if !reply.trim().is_empty() => answer_ask(&answer, true, reply.trim()),
                Status::Done => answer_ask(&answer, false, &format!("{who} gave no answer")),
                Status::Failed(why) => answer_ask(&answer, false, &format!("{who} could not answer: {why}")),
                _ => answer_ask(&answer, false, &format!("{who} was stopped before it answered")),
            }
        }
        let left: Vec<usize> = self.jobs.iter().filter(|j| j.running() && j.asked_by.as_ref().is_some_and(|(by, _)| *by == id))
            .map(|j| j.id).collect();
        for asked in left {
            let _ = self.kill(asked);
        }
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
        // A job that finished without reporting a count used tokens nobody counted: recorded as unknown, not left
        // out. One that reported counts and added none (a resumed conversation's unchanged total) leaves no line, and
        // neither does a pipeline that made no call.
        let called = match job.kind { Kind::Ringo { .. } => job.calls > 0, Kind::Agent { .. } => job.pid.is_some() };
        let unknown = !job.counted && used.total() == 0 && status == Status::Done && called;
        if used.total() > 0 || unknown {
            let who = match job.kind { Kind::Agent { agent, .. } => agent.name().to_string(), Kind::Ringo { .. } => "ringo".into() };
            let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            // What the counts are of: the tier (it sets the effort), the model when the agent names it, a pipeline's
            // command, and the job's folder, where its events (a pipeline's trace: each call's model and effort) are.
            let mut record = serde_json::json!({"at": at, "agent": who, "tier": job.tier.name(), "job": job.dir});
            if let Some(model) = &job.model {
                record["model"] = model.as_str().into();
            }
            if let Kind::Ringo { command } = &job.kind {
                record["command"] = command.as_str().into();
            }
            if job.uncounted > 0 {
                record["uncounted_calls"] = job.uncounted.into(); // the counts below are of its other calls alone
            }
            match unknown {
                true => record["unknown"] = true.into(),
                false => for (key, n) in [("input", used.input), ("cache_read", used.cache_read),
                                          ("cache_write", used.cache_write), ("output", used.output)] {
                    record[key] = n.into();
                },
            }
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(self.root.join("usage.jsonl")) {
                let _ = writeln!(f, "{record}");
            }
        }
        // A turn that failed before its agent counted a token, took a step or said a word never took its message
        // in: what its start marked as told goes back, so the agent's next turn is told it again. Before, only a
        // turn whose program could not start gave it back, and one that died while starting lost it for good.
        let unread = matches!(status, Status::Failed(_)) && !job.counted && used.total() == 0 && job.steps.is_empty()
            && job.reply().trim().is_empty();
        let (added, untold) = if unread { std::mem::take(&mut job.told_now) } else { Default::default() };
        if let (Kind::Agent { agent, .. }, true) = (job.kind.clone(), unread && (!added.is_empty() || untold.is_some())) {
            job.push_log(format!("{} failed before it read its message, so its next turn is told the other turns again",
                                 agent.name()));
            if let Some(heard) = self.heard.get_mut(&agent) {
                added.iter().for_each(|id| { heard.remove(id); });
            }
            if let Some(untold) = untold {
                self.told.entry(agent).or_insert(untold);
            }
        }
        let job = &mut self.jobs[id - 1];
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
            let mut changed: Vec<String> = changed.lines().map(str::to_string).collect();
            // Another agent's turn here at the same time (/separate) is in this diff too: the turn keeps only the
            // files it said it edited and the other did not, so /undo never takes back the other's work. A command
            // the other ran may have written any file without saying so: then the turn keeps none.
            // ceiling: a file only a command wrote, or both edited, stays out of /undo; an edit the other turn made
            // just before this one ended may not have been reported yet.
            let job = &self.jobs[id - 1];
            let others: Vec<&Job> = self.jobs.iter()
                .filter(|o| o.id != id && o.before.is_some() && (o.running() || o.started + o.age() > job.started)).collect();
            if !others.is_empty() {
                let theirs: BTreeSet<&String> = others.iter().flat_map(|o| &o.touched).collect();
                let commands = others.iter().any(|o| o.ran);
                let (mine, left): (Vec<String>, Vec<String>) = changed.into_iter()
                    .partition(|f| !commands && job.touched.contains(f) && !theirs.contains(f));
                if !left.is_empty() {
                    let at = others.iter().map(|o| format!("#{}", o.id)).collect::<Vec<_>>().join(", ");
                    let how = if commands { "ran commands" } else { "worked" };
                    self.jobs[id - 1].push_log(format!("{} changed while {at} {how} here too, so /undo leaves {}",
                                                       left.join(", "), if left.len() == 1 { "it" } else { "them" }));
                }
                changed = mine;
            }
            let job = &mut self.jobs[id - 1];
            job.changed = changed;
            job.after = Some(after);
        }
    }

    /// Folds pending events into the jobs; true when anything changed.
    pub fn pump(&mut self) -> bool {
        let mut changed = self.take_asks();
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
                    let so_far = matches!(note, Note::TokensSoFar(_));
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
                        Note::Looked { kind, text } => job.push(Said::Looked(kind, text)),
                        Note::Subagent { id: sub, label } => {
                            job.activity = format!("started subagent {label}");
                            job.subs.push(Sub { id: sub, label, activity: "starting".into(), ended: None });
                        }
                        Note::SubagentEnd { id: sub, ok } => {
                            if let Some(s) = job.subs.iter_mut().find(|s| s.id == sub) {
                                s.ended = Some(ok);
                            }
                        }
                        Note::Model(model) => job.model = Some(model),
                        Note::Uncounted => job.uncounted += 1,
                        Note::Tokens(t) | Note::TokensSoFar(t) => {
                            // A resumed Codex or agy turn reports its conversation's usage so far (Codex 16,829
                            // then 34,835 input tokens for a one-word follow-up; agy 6,981 then 14,260), so only
                            // what this turn added is counted. Claude reports each run on its own, and where it
                            // cannot (`TokensSoFar`) its total is counted the same way. A running total never
                            // falls, so a result without usage (zero) keeps the last one.
                            job.counted = true;
                            let t = match job.kind {
                                Kind::Agent { agent, write: false }
                                    if so_far || matches!(agent.spec().adapter, Adapter::Codex | Adapter::Agy) =>
                                {
                                    let key = (agent, self.threads.get(&agent).cloned().unwrap_or_default());
                                    let before = self.usage_so_far.get(&key).copied().unwrap_or_default();
                                    if t.total() >= before.total() {
                                        self.usage_so_far.insert(key, t);
                                    }
                                    t.since(before)
                                }
                                // a job with a conversation of its own (a write job) can still report its total
                                // more than once: a line the owner sent while it worked ends in a result of its own
                                _ if so_far => {
                                    let before = job.so_far;
                                    if t.total() >= before.total() {
                                        job.so_far = t;
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
                        Note::Live(t) => {
                            job.counted = true;
                            job.live = t;
                        }
                        Note::Context(n) => {
                            if let Kind::Agent { agent, write: false } = job.kind {
                                self.context.insert(agent, n);
                            }
                        }
                        Note::Call => job.calls += 1,
                        Note::Touched(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path);
                            job.touched.insert(rel.to_string_lossy().into_owned());
                        }
                        Note::Ran => job.ran = true,
                        Note::Connector => job.reached = true,
                        Note::Turn { thread, turn } => job.codex_turn = Some((thread, turn)),
                        Note::Refused(n) => {
                            if let (Some(text), Kind::Agent { agent, .. }) = (job.steered.get(n).cloned(), job.kind.clone()) {
                                // it is the next turn's message, not this one's
                                job.title_full = job.title_full.replacen(&format!("\n\nthen, while it worked: {text}"), "", 1);
                                job.push_log(format!("{} ended before taking in your line, so it is not part of this turn: {}",
                                                     agent.name(), parse::short(&text, 60)));
                                self.returned.push((id, agent, text));
                            }
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
        // Their last events, so every stopped job ends here and is in the usage record.
        let until = Instant::now() + Duration::from_millis(500);
        while self.jobs.iter().any(Job::running) && Instant::now() < until {
            self.pump();
            thread::sleep(Duration::from_millis(10));
        }
        self.pump();
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

/// Takes the owner's entry `name` out of `root`/agents.json, and out of the agents set up there. This process keeps
/// knowing the name until it ends (`Agent::register`). False when there was no such entry.
pub fn remove_custom_agent(root: &Path, name: &str) -> Result<bool, String> {
    with_agents_lock(root, || {
        let mut file = read_agents_file(root);
        let named = |v: &serde_json::Value| v.as_str().or(v["name"].as_str()) == Some(name);
        let Some(custom) = file["custom"].as_array_mut().filter(|c| c.iter().any(named)) else { return Ok(false) };
        custom.retain(|v| !named(v));
        if let Some(agents) = file["agents"].as_array_mut() {
            agents.retain(|v| !named(v));
        }
        write_agents_file(root, &file).map(|_| true)
    })
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
    save_agents_in(&state_root(), agents)
}

/// `save_agents` for the state folder `root`.
pub fn save_agents_in(root: &Path, agents: &[Agent]) -> Result<(), String> {
    with_agents_lock(root, || {
        let mut file = read_agents_file(root);
        file["agents"] = agents.iter().map(|a| a.name()).collect::<Vec<_>>().into();
        write_agents_file(root, &file)
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
            // a success with no limits in it: the plan read failed inside Claude (a busy usage endpoint, a login
            // without the scope), so the shell's fallback is used
            Some(claude_plan(&v["response"]["response"])).filter(|p| !p.windows.is_empty())
                .ok_or("Claude could not read the plan's limits just now".into())
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

/// Whether the owner's message names one of `agents`, as a word of its own in any case ("ask codex", "Codex:").
fn names_agent(prompt: &str, agents: &[Agent]) -> bool {
    let prompt = prompt.to_lowercase();
    let mut words = prompt.split(|c: char| !(c.is_alphanumeric() || "-_.".contains(c))).map(|w| w.trim_matches('.'));
    words.any(|w| agents.iter().any(|a| a.name() == w))
}

/// Asks one turn may make of other agents: with its own turn, an owner's message costs at most `1 + MAX_ASKS` turns
/// this way (a turn that was asked for cannot ask in its turn).
pub const MAX_ASKS: usize = 3;

/// How long a turn waits for the agent it asked, at most.
const ASK_WAIT: Duration = Duration::from_secs(900);

/// `ringo ask-server DIR PEER...`: the MCP server (stdio, one JSON-RPC message a line) a turn is given so its agent
/// can ask another agent here a question in the middle of its work. A call writes the ask into the turn's own folder
/// (`DIR/asks/N.json`) and waits for the answer beside it (`N.answer`): the shell that runs the turn starts the
/// other agent's and writes what it said (`Session::take_asks`). So the server reaches nothing by itself, and a
/// turn asks only whom its shell lets it.
pub fn serve_asks(dir: &Path, peers: &[String], input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    use serde_json::json;
    let mut asked = 0;
    for line in input.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line?) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // a notification: nothing to answer
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({"protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                                      "capabilities": {"tools": {}},
                                      "serverInfo": {"name": "ringo", "version": env!("CARGO_PKG_VERSION")}})),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": [{
                "name": "ask_agent",
                "description": format!("Ask another AI agent that works in this folder a question while you work, and get \
                                        its answer: {}. It reads the files itself and changes nothing. Each ask is a whole \
                                        turn of that agent's, so ask when a second view would change what you do.",
                                       peers.join(", ")),
                "inputSchema": {"type": "object", "required": ["agent", "question"],
                                "properties": {"agent": {"type": "string", "enum": peers}, "question": {"type": "string"}}}}]})),
            "tools/call" => {
                let args = &msg["params"]["arguments"];
                let said = match (msg["params"]["name"].as_str(), args["agent"].as_str(), args["question"].as_str()) {
                    (Some("ask_agent"), Some(agent), Some(question)) if !question.trim().is_empty() =>
                        ask(&dir.join("asks"), &mut asked, agent, question),
                    _ => Err("ask_agent takes an agent and a question".to_string()),
                };
                let failed = said.is_err();
                Ok(json!({"content": [{"type": "text", "text": said.unwrap_or_else(|e| e)}], "isError": failed}))
            }
            other => Err(json!({"code": -32601, "message": format!("no method {other}")})),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        writeln!(output, "{reply}")?;
        output.flush()?;
    }
    Ok(())
}

/// One ask, from the server's side: written whole, then waited on. It is counted (`asked`) once it is written: the
/// session looks for the asks in order, so one that never reached the folder must not take a number.
fn ask(asks: &Path, asked: &mut usize, agent: &str, question: &str) -> Result<String, String> {
    private_dir(asks)?;
    let n = *asked + 1;
    let (file, part) = (asks.join(format!("{n}.json")), asks.join(format!("{n}.part")));
    fs::write(&part, serde_json::json!({"agent": agent, "question": question}).to_string())
        .and_then(|_| fs::rename(&part, &file)).map_err(|e| format!("the ask could not be written: {e}"))?;
    *asked = n;
    let (answer, until) = (asks.join(format!("{n}.answer")), Instant::now() + ASK_WAIT);
    loop {
        if let Some(v) = fs::read(&answer).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
            let text = v["text"].as_str().unwrap_or("").to_string();
            return if v["ok"] == true { Ok(text) } else { Err(text) };
        }
        if Instant::now() > until {
            return Err(format!("{agent} did not answer in time: go on without it"));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// An ask's answer, written whole and renamed into place, so the server never reads half of one.
fn answer_ask(file: &Path, ok: bool, text: &str) {
    let part = file.with_extension("answer.part");
    let _ = fs::write(&part, serde_json::json!({"ok": ok, "text": text}).to_string()).and_then(|_| fs::rename(&part, file));
}

/// A model an agent's plan offers: the name its program takes, what its maker says of it, and whether it is the one
/// the agent runs when none is chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub what: String,
    pub default: bool,
}

/// Whether an agent's turns can be given a model (`Session::models`): Claude Code and Codex take one; agy and a
/// plain agent choose their own.
pub fn takes_model(agent: Agent) -> bool {
    matches!(agent.spec().adapter, Adapter::Claude | Adapter::Codex)
}

/// A model's name as it can go to an agent's program: one word of letters, digits and `. _ - : / [ ]`.
pub fn valid_model(name: &str) -> bool {
    !name.is_empty() && name.len() <= 80 && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-:/[]".contains(c))
        && !name.starts_with('-')
}

/// The models an agent's plan offers, asked of the agent with no model call. Codex's app-server lists them
/// (`model/list`, about 0.3 s); Claude Code has no such list, so its models are named by alias or full name.
pub fn models(agent: Agent, program: &Path) -> Result<Vec<Model>, String> {
    if agent.spec().adapter != Adapter::Codex {
        return Err("has no list of its models to read".into());
    }
    let mut cmd = Command::new(program);
    cmd.args(["app-server", "-c", "forced_login_method=\"chatgpt\"", "-c", "model_provider=\"openai\"",
              "-c", "analytics.enabled=false"]);
    let hello = serde_json::json!({"id": 1, "method": "initialize",
                                   "params": {"clientInfo": {"name": "ringo", "version": env!("CARGO_PKG_VERSION")}}});
    let ask = serde_json::json!({"id": 2, "method": "model/list", "params": {}});
    let v = ask_once(cmd, &[hello, serde_json::json!({"method": "initialized"}), ask], |v| v["id"] == 2,
                     Duration::from_secs(15))?;
    if let Some(e) = v.get("error") {
        return Err(parse::short(e["message"].as_str().unwrap_or("an error"), 120));
    }
    Ok(codex_models(&v["result"]))
}

/// Codex's `model/list` answer (codex-cli 0.167.0): the models it shows, the default first as it sends them.
pub fn codex_models(r: &serde_json::Value) -> Vec<Model> {
    r["data"].as_array().into_iter().flatten().filter(|m| m["hidden"] != true)
        .filter_map(|m| Some(Model { name: m["model"].as_str().or(m["id"].as_str())?.to_string(),
                                     what: m["description"].as_str().unwrap_or("").to_string(),
                                     default: m["isDefault"] == true }))
        .collect()
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
    // A window with no figure for how much is used is left out (here and in the readers below): shown as 0% it would
    // read as a limit with all of its room left.
    for l in &limits {
        if let Some(percent) = l["percent"].as_f64() {
            plan.windows.push(Window { name: claude_window(l["kind"].as_str().unwrap_or("")), percent,
                                       resets: l["resets_at"].as_str().and_then(iso_secs) });
        }
    }
    if limits.is_empty() {
        for key in ["five_hour", "seven_day"] {
            if let Some((w, percent)) = r["rate_limits"].get(key).and_then(|w| Some((w, w["utilization"].as_f64()?))) {
                plan.windows.push(Window { name: claude_window(key), percent, resets: w["resets_at"].as_str().and_then(iso_secs) });
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
        let window = info.pointer(&format!("/unifiedWindows/{key}")).and_then(|w| Some((w, w["utilization"].as_f64()?)));
        if let Some((w, used)) = window {
            plan.windows.push(Window { name: claude_window(key), percent: used * 100.0, resets: w["resetsAt"].as_u64() });
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
        let Some(percent) = w["usedPercent"].as_f64() else { continue };
        let name = match w["windowDurationMins"].as_u64() {
            Some(300) => "5 hours".to_string(),
            Some(10_080) => "week".to_string(),
            Some(m) if m % 1440 == 0 => format!("{} days", m / 1440),
            Some(m) => format!("{} hours", m / 60),
            None => key.to_string(),
        };
        plan.windows.push(Window { name, percent, resets: w["resetsAt"].as_u64() });
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
    fn a_carried_record_keeps_its_newest_whole_lines() {
        let record = "#1 · the owner to claude: é\nclaude: done\n#2 · the owner to codex: ü\ncodex: done\n";
        assert_eq!(newest(record, record.len()), record, "it fits");
        assert_eq!(newest(record, 31), "(the earliest turns before the restart are cut)\ncodex: done\n");
        // a cut inside a two-byte letter moves past it instead of splitting it
        let at = record.find('ü').unwrap() + 1;
        assert!(newest(record, record.len() - at).ends_with("codex: done\n"));
        assert_eq!(newest(record, 0), "(the earliest turns before the restart are cut)\n");
    }

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
        // its own read of the plan failed: a success with no limits, which plan() turns into an error
        assert!(claude_plan(&serde_json::json!({"subscription_type": "max", "rate_limits_available": true, "rate_limits": null}))
            .windows.is_empty());
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
        // a window with no figure for how much is used is left out: unknown is not 0% used
        let json = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap();
        assert_eq!(claude_plan(&json(r#"{"rate_limits": {"limits": [{"kind": "session"}, {"kind": "weekly_all", "percent": 59}]}}"#))
                       .windows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["week"]);
        assert!(claude_plan(&json(r#"{"rate_limits": {"five_hour": {"resets_at": "2026-11-02T04:30:00+00:00"}}}"#)).windows.is_empty());
        assert!(claude_plan_from_event(&json(r#"{"unifiedWindows": {"five_hour": {"resetsAt": 1793332800}}}"#)).windows.is_empty());
        assert!(codex_plan(&json(r#"{"rateLimits": {"primary": {"usedPercent": null, "windowDurationMins": 300}}}"#)).windows.is_empty());
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

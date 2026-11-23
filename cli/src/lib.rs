//! Ringo's conductor: launches agent turns and Ringo pipelines as background jobs, follows their events, and keeps
//! agent edits in private git worktrees until the owner applies them. The terminal shell (`ringo`) and, later,
//! the IDE drive a `Session` and render its jobs.

pub mod awake;
pub mod context_policy;
pub mod evidence;
pub mod framing;
pub mod gateway;
pub mod parse;
pub mod routing;
pub mod rt;
pub mod task;
pub mod task_assignment;
pub mod task_check;

pub mod images;
pub mod turn_policy;
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
/// The last line of a raw `events.jsonl` that reached `framing::MAX_EVENTS_LOG`: nothing after it was logged.
const EVENTS_LOG_MARKER: &str = r#"{"ringo":"events.jsonl reached its size cap; later events were not logged"}"#;

/// A turn's raw event log under an aggregate byte budget. The first record that would pass the budget ends the log
/// with one marker line (room is kept for it), so a capped log never has a silent gap that reads like a whole
/// stream; nothing is written after it. The turn itself goes on either way.
struct EventLog<W: Write> {
    out: Option<W>,
    logged: u64,
    cap: u64,
}

impl<W: Write> EventLog<W> {
    fn new(out: Option<W>, cap: u64) -> Self {
        EventLog { out, logged: 0, cap }
    }

    fn write(&mut self, line: &str) {
        let Some(out) = self.out.as_mut() else { return };
        let need = line.len() as u64 + 1;
        if self.logged + need + EVENTS_LOG_MARKER.len() as u64 + 1 <= self.cap {
            self.logged += need;
            let _ = writeln!(out, "{line}");
        } else {
            let _ = writeln!(out, "{EVENTS_LOG_MARKER}");
            let _ = out.flush();
            self.out = None;
        }
    }
}
/// Maximum UTF-8 bytes of the inline record, including separators and cut notices. This is a text-size bound,
/// not a token estimate or limit. The current owner's message and the handoff envelope are separate.
const RECORD_BYTES: usize = 60_000;
/// The most of the owner's first message the inline record pins at its top when a long session leaves turn #1 out.
const OWNER_PIN_BYTES: usize = 2_000;
/// Files a job remembers it read (the working set the record names), the oldest dropped.
const READ_FILES: usize = 200;
/// The inline record's one line of files read: at most this many paths and bytes (the rest are in the whole record).
const READ_LINE_PATHS: usize = 40;
const READ_LINE_BYTES: usize = 3_000;
/// How the inline record's line of files read opens.
const READ_LINE_PREFIX: &str = "files read (newest first; re-open what you need, they may have changed since): ";
/// How much of one step's text a record keeps (`Session::record`): the head says what ran; the step's whole text is
/// in the job's events file either way.
const STEP_CHARS: usize = 160;
/// What a shell keeps in its folder for a later one to go on from (`Session::keep`, `ringo --resume`).
pub const KEPT: &str = "resume.json";
/// Native compaction during a running turn, independent of the between-turn record handoff. These control the
/// context re-read by each call, including cached input, not cumulative token consumption or a hard size cap.
const CODEX_COMPACT_AT: u64 = 64_000;
/// Claude Code clamps its auto-compact window to at least 100k; its output reserve and compaction headroom are
/// subtracted from this window. Passing Codex's 64k here would not produce a 64k window.
const CLAUDE_COMPACT_WINDOW: u64 = 100_000;
/// A running turn is warned in its log when its tokens so far pass `LIVE_WARN_AT`, then at each doubling (400k, 800k,
/// …), with `/kill N` named: the owner decides (no turn is stopped behind the owner's back). Doubling, not every
/// 200k: a turn's sum is mostly cache reads (each model call re-reads the conversation; 95% of the shell's ledger)
/// and half of all real jobs pass 200k (the median Claude job, 391k, does), so a warning each 200k would have said
/// 1,975 things over 292 real jobs, 97 in one; doubling says 480, at most 7 in a job (ledger replay, 2026-11-14).
const LIVE_WARN_AT: u64 = 200_000;

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
    /// The last warning line the turn's tokens so far passed (`LIVE_WARN_AT`, doubled each time); 0 before the first.
    warned: u64,
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
    /// Files the job read (`Note::Read`), relative to its working folder like `touched`, oldest read first: a file
    /// read again moves to the end. At most `READ_FILES`, the oldest dropped.
    pub read: Vec<String>,
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
    pub counted: bool,
    /// How many of its model calls reported no count (a pipeline's): its counts are then of the others alone.
    pub uncounted: u32,
    /// The running total its agent last reported inside this job, where a turn's own count is not reported and the
    /// job has a conversation of its own (`Note::TokensSoFar` in a write job).
    so_far: Tokens,
    /// What starting this turn marked as told to its agent: the other agents' turns (job ids) and the turns before a
    /// restart. A turn that fails before its agent takes its message in gives them back (`end_with`).
    told_now: (Vec<usize>, Option<String>),
    /// A persisted task assignment awaiting provider-backed delivery acknowledgement. Process creation is insufficient.
    task_assignment: Option<task_assignment::PreparedAssignment>,
    /// The native provider thread observed for this job; task cursors are scoped to it, not just to the agent.
    task_thread: Option<String>,
    /// The native thread this turn resumed, when it resumed one. Only that thread may stand in for an
    /// unobserved thread id: a fresh turn's activity must never acknowledge an older conversation's cursor.
    task_resume: Option<String>,
    /// The native conversation state a fresh handoff replaced this turn. Restored when the turn fails before any
    /// provider call, so a refused launch leaves the conversation available for retry.
    fresh_rollback: Option<(String, Option<Tokens>, Option<u64>)>,
    /// A read turn that began a new native conversation (no thread was resumed), and the bytes of the text sent to
    /// it: its first context report, less this text, measures the agent's fixed base (`Session::bases`).
    fresh_thread: bool,
    sent_bytes: u64,
    measured: bool,
    /// The turn began from Ringo's record (a handoff), which the next turn's dwell guard reads.
    started_fresh: bool,
    /// Each model call's input side, in order (`Note::CallInput`): the fixed/carry/growth split of the usage record.
    call_inputs: Vec<u64>,
    /// The wall-clock time the job started: `started` is monotonic and does not run while the computer sleeps, so
    /// the two clocks disagree by the length of a sleep (see `awake::slept_secs`).
    wall_start: SystemTime,
    /// A turn that started fresh from Ringo's record: the inline handoff text and record.md, in bytes.
    record_bytes: Option<(u64, Option<u64>)>,
}

/// Per-call contexts split against the agent's fixed base `b` (decompose.py's definitions): fixed = min(c, b),
/// carry = max(0, min(c, first) - b), growth = max(0, c - first). The parts can exceed the total when a turn's first
/// call is below b (fixed and growth then share b - first).
pub fn attribute(contexts: &[u64], b: u64) -> Option<(u64, u64, u64)> {
    let first = *contexts.first()?;
    Some(contexts.iter().fold((0, 0, 0), |(fixed, carry, growth), &c| {
        (fixed + c.min(b), carry + c.min(first).saturating_sub(b), growth + c.saturating_sub(first))
    }))
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

/// What a finished job's usage record says, found by its durable private folder. No record is unknown, never zero: a
/// turn whose adapter reported no counts, or one from a shell that did not reach `usage.jsonl`, stays unaccounted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JobUsage {
    pub recorded: bool,
    pub unknown: bool,
    pub uncounted_calls: u64,
    pub tokens: Tokens,
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
    /// The explicitly selected persistent task, if any. Ordinary conversations leave this None.
    pub selected_task: Option<String>,
    /// Why a task named by a resumed conversation could not be selected; chat still resumes normally.
    pub selected_task_error: Option<String>,
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
    /// Whether the turn policy (or turnrun) set `handoff_at`: the owner's number is then used exactly, for every agent.
    pub handoff_pinned: bool,
    /// `RINGO_CONTEXT`, read once when the session opens (tests set the field).
    pub context_mode: context_policy::Mode,
    /// A line for the first read turn's log: the variable's value was not a mode.
    context_note: Option<String>,
    /// The context mode came from the turn policy (an evaluation arm), not from RINGO_CONTEXT.
    context_from_policy: bool,
    /// Each agent's fixed base, as its last three fresh conversations measured it.
    bases: HashMap<Agent, VecDeque<u64>>,
    turn_policy: turn_policy::TurnPolicy,
    /// Evaluation-only transport to a controller-owned isolated native broker.
    #[cfg(feature = "turnrun")]
    trial_broker: Option<PathBuf>,
    /// /separate: each agent keeps a conversation of its own, and they can take turns here at the same time.
    pub separate: bool,
    /// The owner's lines a turn ended before taking in (`steer`): the job and the line, for the shell to send next
    /// (or, the turn stopped, to put back in the input).
    pub returned: Vec<(usize, Agent, String)>,
    /// The first job of the conversation (/clear starts a new one).
    start: usize,
    /// The running total each resumable conversation last reported, by agent and conversation id.
    usage_so_far: HashMap<(Agent, String), Tokens>,
    /// Conversations whose `usage_so_far` this process has actually observed a provider total for. A watermark
    /// restored from a kept file can be behind a turn that crashed after the provider advanced, so it is not evidence
    /// of this turn's work until a note reaches this process and re-syncs it.
    usage_synced: HashSet<(Agent, String)>,
    /// How far agents act without asking.
    pub mode: Mode,
    /// How hard they think (see `Tier`).
    pub tier: Tier,
    /// The model each agent's turns run on, where the owner chose one (`/model`): a name the agent's own program
    /// takes (Claude Code's `--model`, Codex's thread `model`). None for an agent: its own default.
    pub models: HashMap<Agent, String>,
    thread_models: HashMap<Agent, Option<String>>,
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
    /// Holds the computer awake while a model call runs (`awake`); off in tests and with RINGO_KEEP_AWAKE=0.
    awake: Option<awake::KeepAwake<awake::System>>,
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
    let marker = "(the earliest turns before the restart are cut)\n";
    if room < marker.len() {
        return String::new();
    }
    let at = (record.len() - (room - marker.len())..record.len())
        .find(|&i| record.is_char_boundary(i)).unwrap_or(record.len());
    let kept = record[at..].split_once('\n').map_or("", |(_, rest)| rest);
    format!("{marker}{kept}")
}

/// The inline record's line of files the turns read, newest first (`files` are in that order, none repeated): at most
/// `READ_LINE_PATHS` of them and `max` bytes in all, stopping before the path that would cross either, and ending
/// with how many more are in the whole record when some are cut. Empty when nothing fits.
fn read_line(files: &[&str], max: usize) -> (String, usize) {
    let prefix = READ_LINE_PREFIX;
    let tail = |more: usize| if more == 0 { String::new() } else { format!(" and {more} more (in the whole record)") };
    let mut shown = String::new();
    let mut count = 0;
    for f in files.iter().take(READ_LINE_PATHS) {
        let next = format!("{shown}{}{f}", if count == 0 { "" } else { ", " });
        // the line, its newline and the tail it would then need
        if prefix.len() + next.len() + tail(files.len() - count - 1).len() + 1 > max {
            break;
        }
        shown = next;
        count += 1;
    }
    match count {
        0 => (String::new(), 0),
        _ => (format!("{prefix}{shown}{}\n", tail(files.len() - count)), count),
    }
}

/// How many bytes of detail a fresh start's record may add to its lean form (see `Session::record_with`).
///
/// The cost model counts `0.75 * (fills - base)` tokens saved on every call by a fresh start. The record may spend at
/// most half of that (`3/8 * (fills - base)` tokens, 4 bytes each). The decision is made on the lean record and the
/// handoff takes this cap less the text it adds around the record, so the whole text sent stays within it and cannot
/// trip the half-size guard (`about * 2 <= fills`) or the cost model's cover condition. When the cost model decided,
/// the handoff also keeps the cap to `context_policy::record_room` (its margin), so the text sent is fresh by the same
/// rule. With no measured base, half of the conversation is assumed to be the fixed base.
fn record_cap(fills: u64, base: Option<u64>) -> usize {
    4 * (fills.saturating_sub(base.unwrap_or(fills / 2)) * 3 / 8) as usize
}

/// Add one row to a JSON-lines file as a single locked write. Several shells (and a shell and its detached
/// pipelines) share one `usage.jsonl`; `writeln!` on an unbuffered file is two writes (the text, then the newline) and
/// a long row can be split by the OS, so two writers could interleave and leave one unreadable line that every
/// reader then skips. The row is formatted whole (newline included), written with one `write_all` under an exclusive
/// advisory lock (`File::lock`: flock on Unix, LockFileEx on Windows) and unlocked. A failed write is dropped: a
/// usage row is a record, never a reason to fail a job.
fn append_row(path: &Path, record: &serde_json::Value) {
    let line = format!("{record}\n");
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    if f.lock().is_err() {
        return;
    }
    let _ = f.write_all(line.as_bytes());
    let _ = f.unlock();
}

/// A prefix within a byte budget, with the notice inside that budget and no split UTF-8 codepoint.
fn record_head(text: &str, room: usize) -> String {
    if text.len() <= room {
        return text.to_string();
    }
    let marker = "… (cut; the whole is in the record)";
    if room < marker.len() {
        return String::new();
    }
    let mut at = room - marker.len();
    while !text.is_char_boundary(at) { at -= 1; }
    format!("{}{marker}", &text[..at])
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

/// Whether the shell runs under WSL (the Windows Subsystem for Linux): Windows' own tools like `clip.exe` are then
/// reachable on PATH, and the shell uses them where Linux's own clipboard tools are missing.
pub fn is_wsl() -> bool {
    wsl_from(std::env::var_os("WSL_DISTRO_NAME").as_deref(), std::env::var_os("WSLENV").is_some(),
            std::fs::read_to_string("/proc/version").ok().as_deref())
}

/// The testable core of `is_wsl`: WSL sets `WSL_DISTRO_NAME` for every process; its `/proc/version` carries
/// `microsoft` in some casing; `WSLENV` alone is only a hint, but nothing else sets it with that name.
fn wsl_from(distro: Option<&std::ffi::OsStr>, wslenv: bool, proc_version: Option<&str>) -> bool {
    distro.is_some() || wslenv || proc_version.is_some_and(|v| v.to_ascii_lowercase().contains("microsoft"))
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
/// so an agent cannot start another shell; bounded reviews/designs remain available.
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

/// Stops a job's whole process group: on Unix, TERM then KILL after a grace period. A Ringo pipeline gets INT first:
/// its model calls run in sessions of their own, out of the group's reach, and Python's interrupt handling ends them.
/// Windows force-stops the tree in one pass; a graceful pass can lose descendants when their parent exits.
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
        // Without job objects, taskkill can only find the tree while its parent still exists. Do not first ask
        // the parent to exit or wait out a grace period: either can orphan descendants before the forced pass.
        // Run the single forced pass off the UI thread, so a slow taskkill cannot block the shell.
        thread::spawn(move || {
            let mut kill = Command::new("taskkill");
            kill.args(["/T", "/F", "/PID", &pid.to_string()]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                kill.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the helper must not flash a console
            }
            let _ = kill.stdout(Stdio::null()).stderr(Stdio::null()).status();
        });
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
        let (context_mode, context_note) = context_policy::Mode::parse(std::env::var("RINGO_CONTEXT").ok().as_deref());
        let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
        Ok(Session { repo, git: project.is_some(), index, branch, store, no_undo, at_home, codex_home, home: home.clone(), peers: Vec::new(), asker: std::env::current_exe().unwrap_or_default(), dir, jobs: Vec::new(),
                     selected_task: None, selected_task_error: None, max_jobs: 3,
                     backend: "codex-chatgpt".into(), models: HashMap::new(), thread_models: HashMap::new(),
                     ringo_home: ringo_home(), pipeline: None, programs, hold: None, threads: HashMap::new(), told: HashMap::new(), before: HashMap::new(), source: None, about: String::new(), reached_before: HashSet::new(),
                     context: HashMap::new(), heard: HashMap::new(), handoff_at: HANDOFF_AT, handoff_pinned: false, context_mode, context_note, context_from_policy: false,
                     bases: HashMap::new(), turn_policy: Default::default(), separate: false,
                     #[cfg(feature = "turnrun")]
                     trial_broker: None,
                     returned: Vec::new(), start: 1,
                     usage_so_far: HashMap::new(), usage_synced: HashSet::new(), mode: Mode::Ask, tier: Tier::Standard, root: resolved(root), claude_limits: None, always_ask, allowed: BTreeSet::new(),
                     routed: VecDeque::new(), tx, rx,
                     awake: (!cfg!(test) && !awake::disabled(std::env::var("RINGO_KEEP_AWAKE").ok().as_deref()))
                         .then(|| awake::KeepAwake::new(awake::System::new(is_wsl()))) })
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

    /// The persistent task store for this exact repository. It is opened only for explicit task operations (or when a
    /// resumed conversation names a task), so damaged task state never prevents ordinary chat.
    pub fn task_store(&self) -> Result<task::TaskStore, String> {
        task::TaskStore::new(&self.root, self.repo.clone(), self.git).map_err(|e| e.to_string())
    }

    pub fn select_task(&mut self, id: &str) -> Result<task::Task, String> {
        let task = self.task_store()?.load(id).map_err(|e| e.to_string())?;
        self.selected_task = Some(task.id.clone());
        self.selected_task_error = None;
        Ok(task)
    }

    pub fn selected_task(&self) -> Result<Option<task::Task>, String> {
        self.selected_task.as_deref().map(|id| self.select_task_read(id)).transpose()
    }

    fn select_task_read(&self, id: &str) -> Result<task::Task, String> {
        self.task_store()?.load(id).map_err(|e| e.to_string())
    }

    /// The program an agent runs: as set (tests point it at a fake), else its entry's (an agent added after the
    /// session opened).
    pub fn program(&self, agent: Agent) -> PathBuf {
        self.programs.get(&agent).cloned().unwrap_or_else(|| PathBuf::from(&agent.spec().program))
    }

    /// Select an experimental policy explicitly. Normal sessions retain the exact default behavior.
    pub fn set_turn_policy(&mut self, policy: turn_policy::TurnPolicy) {
        self.handoff_at = policy.handoff_at.unwrap_or(HANDOFF_AT);
        self.handoff_pinned = policy.handoff_at.is_some();
        if let Some(mode) = policy.context {
            // a policy names its context mode so a matched evaluation records and pins it (RINGO_CONTEXT does not)
            self.context_mode = mode;
            self.context_from_policy = true;
        }
        self.turn_policy = policy;
    }

    /// Only the dedicated scenario binary uses this; normal shell startup never sets it.
    #[cfg(feature = "turnrun")]
    pub fn set_trial_broker(&mut self, socket: PathBuf) {
        self.trial_broker = Some(socket);
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
        let Ok(content) = images::content(agent.spec().adapter, text, text) else { return false };
        let line = match agent.spec().adapter {
            Adapter::Claude => serde_json::json!({"type": "user", "message": {"role": "user", "content": content}}),
            Adapter::Codex => match &job.codex_turn {
                Some((thread, turn)) => serde_json::json!({"id": parse::STEER_ID + job.steered.len() as u64,
                    "method": "turn/steer", "params": {"threadId": thread, "expectedTurnId": turn,
                                                       "input": content}}),
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
                let room = RECORD_BYTES.saturating_sub(record.len() + 1);
                let older = newest(before.trim_end(), room);
                if older.is_empty() { older } else { format!("{older}\n") }
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
            // lean: the next shell gives the turns before the restart only the room its own record leaves, and the
            // owner's earlier words are worth more there than later paragraphs (the uncut file is linked below)
            let (record, whole) = self.fresh_record(agent, &self.lane(agent, self.jobs.len() + 1), 0);
            let file = self.dir.join(format!("record-{}.md", agent.name()));
            let record = match record.is_empty() || fs::write(&file, whole).is_err() {
                true => record,
                false => {
                    let pointer = format!("\n(these turns uncut: {})", file.display());
                    format!("{}{pointer}", newest(&record, RECORD_BYTES.saturating_sub(pointer.len())))
                }
            };
            (fills > 0 || !record.is_empty()).then(|| (agent.name().to_string(), fills, record))
        }).collect()
    }

    /// Goes on with what the shell before a /restart handed on: each agent's conversation, what it had yet to be told,
    /// and how full that conversation is with Ringo's record of it (agents no longer set up are left out).
    pub fn resume(&mut self, threads: &[(String, String)], told: &[(String, String)], records: &[(String, u64, String)]) {
        // Restored conversations carry no observation of this process: a watermark handed on from a kept file is
        // untrusted until a provider note reaches here (see `usage_synced`).
        self.usage_synced.clear();
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
        self.selected_task = None;
        self.selected_task_error = None;
        if let Some(id) = kept["task"].as_str() {
            if let Err(why) = self.select_task(id) {
                self.selected_task_error = Some(why);
            }
        }
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
                                      "conversation": conversation, "folder": self.repo, "about": about, "saved": saved,
                                      "task": self.selected_task});
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
        self.usage_so_far.remove(&(agent, old.clone()));
        self.usage_synced.remove(&(agent, old));
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

    /// The between-turn size past which `agent`'s next turn starts fresh from the record: `handoff_at`, for every
    /// agent, as before the cost model. For Codex that is at its in-turn compaction limit, so in the default mode it
    /// does not start a resumed thread fresh between turns: a fresh start turns cached carry (0.05x in plan credits)
    /// into an uncached record, and the offline replay of the owner's traces found an earlier Codex threshold (48k)
    /// cutting raw input by about 3% while raising credit-weighted cost. A turn policy's `handoff_at` sets an earlier
    /// one; `RINGO_CONTEXT=adaptive` uses the cost model.
    fn static_threshold(&self, _agent: Agent) -> u64 {
        self.handoff_at
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
        let threshold = self.static_threshold(agent);
        let mut notes: Vec<String> = Vec::new();
        if let Some(bad) = self.context_note.take() {
            notes.push(format!("RINGO_CONTEXT={bad} is not static, adaptive or off: the default mode is used"));
        }
        let (mode, pinned) = (self.context_mode, self.handoff_pinned);
        let eligible = matches!(agent.spec().adapter, Adapter::Codex | Adapter::Claude);
        let lane = self.lane(agent, id);
        let unheard = self.unheard(agent, &lane);
        // The agent's measured base (what the cost model uses too), and what a conversation being replaced lets its
        // record add: a brand-new conversation saves nothing, so its record stays lean.
        let base = self.base_median(agent);
        let mut cap = if resume.is_some() { record_cap(fills, base) } else { 0 };
        // The text a start from the record would send (`fresh`), or the turns the agent missed (not), before the
        // guard that a fresh start must be at most half the conversation it replaces. `rich` is the detail cap its
        // record may fill (0: the lean record); the fourth field is what the text adds around the record, counting
        // the pointer to the whole record even when this build needs none (a richer one may).
        let build = |fresh: bool, rich: usize| -> Option<(bool, String, u64, usize, String, String, Vec<usize>)> {
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
            let (record, whole, complete) = if fresh { self.fresh_record_full(agent, turns, rich) } else { self.record_full(turns, 0) };
            let head = match fresh {
                true => "Ringo's record of this conversation so far, which you start fresh from",
                false => "While you were away, other agents took turns in this conversation. Ringo's record of them",
            };
            // A complete inline record needs no second read. A cut one keeps a recovery path; agy can only open
            // that path in read mode, where --add-dir grants the job folder (never in an edits turn).
            let pointer = format!(" The whole record, uncut, is in {}. Read only the parts needed to recover a missing \
                                   constraint or detail.", self.jobs[id - 1].dir.join("record.md").display());
            let whole_at = if complete
                || (agent.spec().adapter == Adapter::Agy && self.mode != Mode::Read) {
                String::new()
            } else {
                pointer.clone()
            };
            let mut text = format!("{head} (the owner's words are exact; the replies and steps are the agents' own account \
                                and can be wrong; the files here are the truth).{whole_at}\n\n{}\n\n", record.trim_end());
            if !self.turn_policy.handoff_instruction.is_empty() {
                text = format!("{}\n\n{text}", self.turn_policy.handoff_instruction);
            }
            let about = text.len() as u64 / 4; // the owner's message goes either way
            let wrap = text.len() - record.trim_end().len() + if whole_at.is_empty() { pointer.len() } else { 0 };
            let ids: Vec<usize> = turns.iter().map(|j| j.id).collect();
            Some((fresh, text, about, wrap, span, whole, ids))
        };
        let mut fresh_built = None;
        let mut verdict: Option<context_policy::Verdict> = None;
        let mut shadow = false;
        let want = if resume.is_none() {
            true
        } else if pinned && !(mode == context_policy::Mode::Adaptive && self.context_from_policy) {
            // the owner's pinned number, exactly, whatever RINGO_CONTEXT says (a policy that also names adaptive
            // asks for the cost model, with its number as the fallback)
            if mode == context_policy::Mode::Adaptive {
                verdict = Some(context_policy::Verdict {
                    action: if fills > threshold { context_policy::Action::Fresh } else { context_policy::Action::Resume },
                    reason: format!("owner handoff_at {}k", self.handoff_at / 1000), saving: None });
            }
            fills > threshold
        } else if mode == context_policy::Mode::Off {
            false
        } else if !eligible {
            fills > threshold
        } else {
            {
                {
                    let mine: Vec<&&Job> = lane.iter().filter(|j| matches!(j.kind, Kind::Agent { agent: a, .. } if a == agent)).collect();
                    let last = mine.last();
                    // a turn that made no call (it failed first) says nothing about how long turns run
                    let mut calls: Vec<u64> = mine.iter().rev().filter(|j| j.calls > 0).take(5).map(|j| j.calls as u64).collect();
                    calls.sort_unstable();
                    let calls = calls.get(calls.len() / 2).copied();
                    let idle_s = last.and_then(|j| j.elapsed.map(|e| j.started.elapsed().saturating_sub(e).as_secs()));
                    let ttl_s = if agent.spec().adapter == Adapter::Claude { context_policy::CLAUDE_TTL_S } else { context_policy::CODEX_TTL_S };
                    // The record is built (every turn of the lane rendered, and a git diff) only when it can matter:
                    // the size test would start fresh, or the cost model could. Otherwise its size is not needed.
                    let cold = idle_s.is_some_and(|i| i > ttl_s);
                    if fills > threshold || context_policy::could_start_fresh(fills, base, calls, cold) {
                        fresh_built = build(true, 0);
                    }
                    let signals = context_policy::Signals {
                        fills,
                        base,
                        // not built only when even an empty record could not pay (or the size test decides)
                        record: fresh_built.as_ref().map(|b| b.2).or((base.is_none()).then_some(0)),
                        calls,
                        idle_s,
                        ttl_s,
                        previous_ended_badly: last.is_some_and(|j| matches!(j.status, Status::Failed(_) | Status::Cancelled)),
                        continuation: context_policy::is_continuation(prompt),
                        fresh_last_turn: last.is_some_and(|j| j.started_fresh),
                    };
                    let v = context_policy::decide(&signals, threshold);
                    let adaptive = mode == context_policy::Mode::Adaptive;
                    // a fresh start the cost model chose gets no more detail than keeps it fresh by the same rule
                    if adaptive && v.saving.is_some() && let Some(base) = base {
                        cap = cap.min(4 * context_policy::record_room(fills, base, calls, cold) as usize);
                    }
                    shadow = !adaptive;
                    let action = v.action;
                    verdict = Some(v);
                    if adaptive { action == context_policy::Action::Fresh } else { fills > threshold }
                }
            }
        };
        // the half-size guard below applies to any fresh start, so a verdict is compared after it, like the rule's
        let fits = fresh_built.as_ref().is_some_and(|b| b.2 * 2 <= fills);
        let picked = [want, false].into_iter().find_map(|fresh| {
            let built = if fresh { fresh_built.take().or_else(|| build(true, 0)) } else { build(false, 0) }?;
            (!fresh || resume.is_none() || built.2 * 2 <= fills).then_some(built)
        });
        // Decided on the lean record, a fresh start of a conversation in use then carries the detail its saving pays
        // for: the record rebuilt with the cap less what the text adds around it, so the whole text sent stays within
        // `record_cap` (3/8 of fills less base, in tokens) and can neither undo the decision nor trip the half-size
        // guard, which the verdict and shadow lines above saw on the lean size.
        let mut picked = picked;
        if resume.is_some() && let Some((true, _, about, wrap, ..)) = picked.as_ref() && cap > *wrap {
            let lean = *about;
            if let Some(rich) = build(true, cap - wrap).filter(|r| r.2 >= lean && r.2 * 4 <= cap as u64 && r.2 * 2 <= fills) {
                picked = Some(rich);
            }
        }
        if resume.is_some() && let Some(v) = verdict {
            let did_fresh = matches!(picked, Some((true, ..)));
            let who = agent.name();
            if shadow {
                // only when it would have done otherwise than the static rule did
                let would = v.action == context_policy::Action::Fresh && fits;
                if would != did_fresh {
                    let word = if would { "fresh" } else { "resume" };
                    notes.push(format!("ctx {who} (shadow): adaptive would {word} — {}", v.reason));
                }
            } else if v.action == context_policy::Action::Fresh && !did_fresh {
                notes.push(format!("ctx {who}: resume — the record is over half of {}; {}", fmt_k(fills), v.reason));
            } else {
                notes.push(format!("ctx {who}: {} — {}", v.action.word(), v.reason));
            }
        }
        if matches!(picked, Some((true, ..))) {
            self.jobs[id - 1].started_fresh = true;
        }
        for note in notes {
            self.jobs[id - 1].push_log(note);
        }
        let busy = self.in_flight(id, agent);
        let heard = self.heard.entry(agent).or_default();
        match &picked {
            Some((true, .., ids)) => *heard = ids.iter().copied().collect(),
            Some((.., ids)) => heard.extend(ids),
            None if resume.is_none() => heard.clear(), // a new conversation, with nothing to tell it
            None => {}
        }
        let Some((fresh, text, about, _, span, whole, _)) = picked else {
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
        let record_inline = text.len() as u64;
        let record_whole = whole.len() as u64;
        let text = format!("{text}{}The owner's new message:\n{prompt}", busy.unwrap_or_default());
        let write_ok = match fs::write(&file, whole) {
            Ok(()) => true,
            Err(e) => {
                self.jobs[id - 1].push_log(format!("the whole record could not be kept: {e}"));
                false
            }
        };
        if fresh {
            self.jobs[id - 1].record_bytes = Some((record_inline, write_ok.then_some(record_whole)));
            self.told.remove(&agent); // the turns before the restart it had not heard are in the record (`carried`)
        }
        if fresh && let Some(old) = resume.take() {
            let usage = self.usage_so_far.remove(&(agent, old.clone()));
            // The replacement conversation has not been observed here: its watermark, if restored, is untrusted again.
            self.usage_synced.remove(&(agent, old.clone()));
            let context = self.context.remove(&agent);
            self.threads.remove(&agent);
            // Stashed on the job so a failure before dispatch can put the conversation back.
            self.jobs[id - 1].fresh_rollback = Some((old, usage, context));
        }
        Some((text, note))
    }

    /// Undoes this turn's optimistic state after a failure before dispatch: the generic-handoff markers, and a
    /// fresh handoff's replaced conversation, so a refused launch leaves everything retryable.
    fn roll_back_turn(&mut self, id: usize, agent: Agent, heard: Option<BTreeSet<usize>>, untold: Option<String>) {
        match heard {
            Some(heard) => { self.heard.insert(agent, heard); }
            None => { self.heard.remove(&agent); }
        };
        if let Some(untold) = untold {
            self.told.insert(agent, untold);
        }
        if let Some((old, usage, context)) = self.jobs[id - 1].fresh_rollback.take() {
            if let Some(usage) = usage {
                self.usage_so_far.insert((agent, old.clone()), usage);
            }
            self.threads.insert(agent, old);
            if let Some(context) = context {
                self.context.insert(agent, context);
            }
        }
    }

    /// The median of the agent's last measured bases (what its first call of a new conversation read beyond the
    /// message), or None before one is measured.
    fn base_median(&self, agent: Agent) -> Option<u64> {
        let mut bases: Vec<u64> = self.bases.get(&agent).map(|b| b.iter().copied().collect()).unwrap_or_default();
        bases.sort_unstable();
        bases.get(bases.len() / 2).copied()
    }

    /// The turns before a /restart that `agent`'s fresh start begins with: the record of its conversation there, or,
    /// from a shell that handed on no records, the turns it had yet to be told (its own were never recorded).
    fn carried(&self, agent: Agent) -> Option<&String> {
        self.before.get(&agent).or_else(|| self.told.get(&agent))
    }

    /// Ringo's record for `agent`'s fresh start: the turns before a /restart that its conversation held, if any, then
    /// `turns`, as `record` gives them; the older turns are cut first.
    fn fresh_record(&self, agent: Agent, turns: &[&Job], cap: usize) -> (String, String) {
        let (record, whole, _) = self.fresh_record_full(agent, turns, cap);
        (record, whole)
    }

    /// `fresh_record`, and whether the inline record holds everything the whole one does (no pointer needed).
    fn fresh_record_full(&self, agent: Agent, turns: &[&Job], cap: usize) -> (String, String, bool) {
        let Some(before) = self.carried(agent) else { return self.record_full(turns, cap) };
        let (lean, whole, lean_complete) = self.record_full(turns, 0);
        let separator = "\n\n(Ringo restarted here; the turns since are numbered from #1 again)\n\n";
        let join = |a: &str, b: &str| match (a.is_empty(), b.is_empty()) {
            (true, _) => b.to_string(),
            (_, true) => a.trim_end().to_string(),
            _ => format!("{}{separator}{b}", a.trim_end()),
        };
        // The turns before the restart keep the room the lean record leaves them (the owner's earlier words are
        // worth more than the later paragraphs of newer replies); the rich tier gets only what is left of the cap.
        let kept = newest(before, RECORD_BYTES.saturating_sub(lean.len() + separator.len()));
        let reserved = if kept.trim_end().is_empty() { 0 } else { kept.trim_end().len() + separator.len() };
        let rich = cap.min(RECORD_BYTES - 128).saturating_sub(reserved);
        let (record, complete) = if rich > lean.len() {
            let (record, _, complete) = self.record_full(turns, rich);
            (record, complete)
        } else {
            (lean, lean_complete)
        };
        let complete = complete && kept.trim_end() == before.trim_end();
        (join(&kept, &record), join(before, &whole), complete)
    }

    /// Ringo's record of `turns`, oldest first, and the same uncut: each turn's message from the owner, word for
    /// word; the agent's reply (the newest whole, older ones' first paragraph); its failed steps and its last ones;
    /// the files it changed and, in the whole record, the files it read; then the files the turns read (one line,
    /// newest first, without the ones already listed as changed) and the change the turns made, as a diff. Newest
    /// first into RECORD_BYTES; an older turn past it is left to the whole record.
    fn record(&self, turns: &[&Job]) -> (String, String) {
        self.record_with(turns, 0)
    }

    /// `record` with a rich tier: when the lean record (as `record` gives it) is smaller than `cap` bytes, kept older
    /// turns are given their whole form (the reply to 6000 characters and the last steps), newest first, each if its
    /// extra bytes fit in what is left of `min(cap, RECORD_BYTES - 128)`; a turn that does not fit is skipped and an
    /// older one tried. Turns already whole are never touched, the diff and the files-read line stay, and the record
    /// never passes RECORD_BYTES. The whole record is the same for any cap; cap 0 is exactly `record`.
    fn record_with(&self, turns: &[&Job], cap: usize) -> (String, String) {
        let (record, whole, _) = self.record_full(turns, cap);
        (record, whole)
    }

    /// `record_with`, and whether the inline record holds everything the whole one does: no turn left out or shown
    /// short of its whole form, the files-read line naming every file read, and the diff uncut. Decided from what was
    /// built, not by comparing the texts (the files read are one line inline and a line per turn in the whole record).
    fn record_full(&self, turns: &[&Job], cap: usize) -> (String, String, bool) {
        let cut = |text: &str, n: usize| match text.char_indices().nth(n) {
            Some((at, _)) => format!("{}… (cut; the whole is in the record)", &text[..at]),
            None => text.to_string(),
        };
        // A step as a record keeps it: to STEP_CHARS, marked "…" where cut. A failed step ends in what became of it
        // (` · exit 2`, or the tool's error), and that end is kept inside the bound: a cut never hides why it failed.
        let step_head = |step: &str| -> String {
            let upto = |text: &str, n: usize| match text.char_indices().nth(n) {
                Some((at, _)) => format!("{}…", &text[..at]),
                None => text.to_string(),
            };
            if step.chars().count() <= STEP_CHARS {
                return step.to_string();
            }
            match step.rsplit_once(" · ").filter(|_| step.starts_with(parse::FAILED)) {
                Some((body, end)) => {
                    let end = upto(end, STEP_CHARS / 2);
                    format!("{} · {end}", upto(body, STEP_CHARS - 3 - end.chars().count()))
                }
                None => upto(step, STEP_CHARS),
            }
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
            // the failed steps, each told once: five different ones, not one failure run five times
            let mut failed: Vec<&str> = Vec::new();
            for s in j.steps.iter().map(String::as_str).filter(|s| s.starts_with(parse::FAILED)) {
                if !failed.contains(&s) {
                    failed.push(s);
                }
            }
            let picked: Vec<&str> = failed.iter().copied().take(5)
                .chain(j.steps.iter().rev().take(if whole { 5 } else { 0 }).rev().map(String::as_str)
                    .filter(|s| !s.starts_with(parse::FAILED))).collect();
            // when the pick holds every step, they are told in the order they ran, as the whole record tells them
            let every = j.steps.iter().all(|s| picked.contains(&s.as_str()));
            let steps: Vec<&str> = match full || every {
                true => j.steps.iter().map(String::as_str).collect(),
                false => picked,
            };
            if !steps.is_empty() {
                // The steps are the agents' own account, and in real records they are most of it (71-75% of the
                // chars in two real 11-turn sessions, measured 2026-11-14). So each keeps a head (`step_head`) —
                // what a step did is told by the changed files and the diff below, and its whole text stays in the
                // job's events file — and a step run more than once is told once, with how many times the turn ran
                // it: "(3 times)" names the step it means, where a bare "same again" read as the one before it.
                let mut once: Vec<&str> = Vec::new();
                for s in steps {
                    if !once.contains(&s) {
                        once.push(s);
                    }
                }
                let steps: Vec<String> = once.iter().map(|s| match j.steps.iter().filter(|t| t.as_str() == *s).count() {
                    1 => step_head(s),
                    n => format!("{} ({n} times)", step_head(s)),
                }).collect();
                part += &format!("steps ({} in all): {}\n", j.steps.len(), steps.join(" | "));
            }
            if !j.changed.is_empty() {
                let names = if full { j.changed.join(", ") } else { files(&j.changed.iter().cloned().collect()) };
                part += &format!("changed: {names}{}\n", if j.undone { " (taken back with /undo)" } else { "" });
            } else if !j.touched.is_empty() {
                // nothing git saw as its own: no snapshot of the folder (no /undo here: the home folder, a large one),
                // or another turn at work here at the same time ran commands; the files it said it edited
                part += &format!("edited (as it reported): {}\n", files(&j.touched));
            }
            // the files it read: all of them, in the whole record only (the inline record has one line for all turns)
            if full && !j.read.is_empty() {
                part += &format!("read: {}\n", j.read.join(", "));
            }
            part
        };
        let is_whole = |i: usize, j: &Job| i + 1 == turns.len() || j.talk || j.asked_by.is_some();
        // Fill the inline record newest first within `limit`. With `pinned` the first turn is not a candidate (its
        // owner words are pinned by the caller), so the omission notice names #2 to #k. Returns the pieces, the turn
        // each renders (None: a notice), the bytes used, and whether a turn was left out.
        let fill = |limit: usize, pinned: bool| {
            let mut kept: Vec<String> = Vec::new();
            let mut kept_turn: Vec<Option<usize>> = Vec::new();
            let mut used = 0;
            let mut left_out = false;
            for (i, j) in turns.iter().enumerate().rev() {
                if pinned && i == 0 {
                    break;
                }
                let part = render(j, is_whole(i, j), false);
                // Reserve space for an omission notice; count the join separators too. Even one turn can overflow
                // through a huge file list, failure message or multibyte text, so the newest turn must also be bounded.
                let part = record_head(&part, RECORD_BYTES - 128);
                if used + part.len() + usize::from(!kept.is_empty()) > limit && !kept.is_empty() {
                    let first = if pinned { turns[1].id } else { turns[0].id };
                    let count = if pinned { i } else { i + 1 };
                    let omitted = format!("(#{first} to #{}, {count} earlier turns, are in the whole record)\n", j.id);
                    used += omitted.len() + 1;
                    kept.push(omitted);
                    kept_turn.push(None);
                    left_out = true;
                    break;
                }
                used += part.len() + usize::from(!kept.is_empty());
                kept.push(part);
                kept_turn.push(Some(i));
            }
            (kept, kept_turn, used, left_out)
        };
        let (mut kept, mut kept_turn, mut used, left_out) = fill(RECORD_BYTES - 128, false);
        if left_out && turns.len() > 1 {
            // The first turn is the owner's original request and the newest-first fill leaves it out first, so a long
            // session lost the very words everything after it builds on. Pin those words (to OWNER_PIN_BYTES) at the
            // top and fill the rest in what is left. A session that fits whole never gets here, so it is unchanged.
            let who = match turns[0].kind { Kind::Agent { agent, .. } => agent.name(), Kind::Ringo { .. } => "ringo" };
            let pin = record_head(&format!("#{} · the owner to {who}: {}\n", turns[0].id, turns[0].title_full.trim()), OWNER_PIN_BYTES);
            let (mut k, mut kt, u, _) = fill((RECORD_BYTES - 128).saturating_sub(pin.len() + 1), true);
            k.push(pin.clone());
            kt.push(None);
            used = u + pin.len() + 1;
            kept = k;
            kept_turn = kt;
        }
        kept.reverse();
        kept_turn.reverse();
        let mut whole: Vec<String> = turns.iter().map(|j| render(j, true, true)).collect();
        let mut diff_cut = false;
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
                let label = "The change these turns made, as git shows it:\n";
                let room = RECORD_BYTES.saturating_sub(used + label.len() + 1).min(16_000);
                let shown = record_head(&diff, room);
                diff_cut = shown.len() != diff.len();
                if !shown.is_empty() {
                    kept.push(format!("{label}{shown}"));
                }
                whole.push(format!("{label}{diff}"));
            }
        }
        // The files the turns read, in one line before the diff: only if it fits what the turns and the diff left.
        let total = |kept: &[String]| kept.iter().map(String::len).sum::<usize>() + kept.len().saturating_sub(1);
        // only the files a changed or edited line names (`files` names 20): one past them is still told as read
        let changed: BTreeSet<&str> = turns.iter().flat_map(|j| {
            let listed: BTreeSet<&str> = match j.changed.is_empty() {
                true => j.touched.iter().map(String::as_str).collect(),
                false => j.changed.iter().map(String::as_str).collect(),
            };
            listed.into_iter().take(20)
        }).collect();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let read: Vec<&str> = turns.iter().rev().flat_map(|j| j.read.iter().rev().map(String::as_str))
            .filter(|f| !changed.contains(f) && seen.insert(f)).collect();
        let (line, named) = read_line(&read, READ_LINE_BYTES.min(RECORD_BYTES.saturating_sub(total(&kept) + 1)));
        let mut complete = named == read.len();
        if !line.is_empty() {
            let at = kept.len() - usize::from(kept.last().is_some_and(|k| k.starts_with("The change these turns made")));
            kept.insert(at, line);
            kept_turn.insert(at, None);
        }
        // The rich tier: the saving pays for the replies and steps the lean record cut.
        let mut room = cap.min(RECORD_BYTES - 128).saturating_sub(total(&kept));
        for (at, i) in kept_turn.iter().enumerate().rev() {
            let Some(i) = *i else { continue };
            if room == 0 {
                break;
            }
            let j = turns[i];
            if is_whole(i, j) {
                continue;
            }
            let richer = record_head(&render(j, true, false), RECORD_BYTES - 128);
            if let Some(extra) = richer.len().checked_sub(kept[at].len()).filter(|extra| *extra <= room) {
                room -= extra;
                kept[at] = richer;
            }
        }
        // complete: every turn kept, each as its whole form shows it (less the files-read line the whole form ends
        // with, which the inline line covers)
        complete &= !diff_cut && kept_turn.iter().flatten().count() == turns.len();
        for (at, i) in kept_turn.iter().enumerate() {
            let Some(i) = *i else { continue };
            let full = &whole[i];
            let reads = format!("read: {}\n", turns[i].read.join(", "));
            let bare = if turns[i].read.is_empty() { full.as_str() } else { full.strip_suffix(&reads).unwrap_or(full) };
            complete &= kept[at] == bare;
        }
        (kept.join("\n"), whole.join("\n"), complete)
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
                             elapsed: None, activity: "starting".into(), tokens: Tokens::default(), live: Tokens::default(), calls: 0, warned: 0,
                             subs: Vec::new(), log: VecDeque::new(), fresh: Vec::new(), dir, worktree: None, base: None, applied: false, unchanged: false,
                             touched: BTreeSet::new(), read: Vec::new(), before: None, after: None, changed: Vec::new(), undone: false,
                             asks: VecDeque::new(), lane: None, steps: Vec::new(), ran: false, input: None, pid: None,
                             cancel: Arc::new(AtomicBool::new(false)), steered: Vec::new(), gate: Arc::default(),
                             codex_turn: None, connectors: false, reached: false, model: None, talk: false, can_ask: false, asked: 0, asked_by: None, counted: false,
                             uncounted: 0, so_far: Tokens::default(), told_now: (Vec::new(), None),
                              task_assignment: None, task_thread: None, task_resume: None, fresh_rollback: None,
                              fresh_thread: false, sent_bytes: 0, measured: false, started_fresh: false,
                              call_inputs: Vec::new(), record_bytes: None, wall_start: SystemTime::now() });
        Ok(id)
    }

    /// One agent turn. Read turns run in the repository with the agent's read-only setting and continue that
    /// agent's conversation; write turns start fresh in a private worktree from HEAD.
    pub fn launch_agent(&mut self, agent: Agent, write: bool, prompt: &str) -> Result<usize, String> {
        self.launch_agent_with_model(agent, write, prompt, None)
    }

    /// One turn with an explicit model from a task assignment. The explicit model wins over the shell's /model choice;
    /// None keeps the ordinary precedence (the owner's /model, then the agent's own default). A model the agent does
    /// not take is refused here, before any job or model call.
    pub fn launch_agent_with_model(&mut self, agent: Agent, write: bool, prompt: &str,
                                   model: Option<&str>) -> Result<usize, String> {
        if let Some(model) = model && let Some(problem) = routing::model_problem(agent, model) {
            return Err(problem);
        }
        if write && !self.git {
            return Err(format!("@{}! edits in a private git worktree, so it needs a git project: start ringo inside one \
                                (or run git init here). @{} without ! works in any folder", agent.name(), agent.name()));
        }
        let (name, spec) = (agent.name(), agent.spec());
        // Validate before creating a job or taking a snapshot. A failed attachment starts no model call.
        let mut content = images::content(spec.adapter, prompt, prompt)?;
        // With the trial broker the turn runs through this process (the client holds no provider program or
        // credentials; see the Claude branch below), so the provider's own program need not be installed here.
        #[cfg(feature = "turnrun")]
        let brokered = self.trial_broker.is_some();
        #[cfg(not(feature = "turnrun"))]
        let brokered = false;
        if !brokered && !self.installed(agent) {
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
        // This turn's model: an assignment's explicit choice first, then the shell's /model (launch_agent_with_model).
        let chosen_model = model.map(str::to_string).or_else(|| self.models.get(&agent).cloned());
        if !write {
            let effective_model = chosen_model.clone().or(own_model.clone());
            if self.thread_models.get(&agent).is_some_and(|previous| previous != &effective_model) {
                self.start_over(agent);
            }
            self.thread_models.insert(agent, effective_model);
        }
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
        let selected = if prompt.trim() != "/compact" {
            match self.selected_task() {
                Ok(task) => task,
                Err(why) => {
                    // Selected-task state is required before dispatch. Roll back the optimistic generic-handoff
                    // markers and fail the job, so it is not left Running and no model call was made.
                    self.roll_back_turn(id, agent, heard.clone(), untold.clone());
                    self.end(id, Status::Failed(why.clone()));
                    return Err(why);
                }
            }
        } else { None };
        if let Some(task) = selected {
            let prepared = task_assignment::AssignmentStore::new(&self.root, &task.repository)
                .and_then(|store| store.prepare(&task, name, resume.as_deref(), task_assignment::ContextPolicy::bounded()));
            match prepared {
                Ok(Some(prepared)) => {
                    let kind = match prepared.manifest.kind {
                        task_assignment::AssignmentKind::Full => "full assignment",
                        task_assignment::AssignmentKind::Delta => "assignment delta",
                    };
                    self.jobs[id - 1].push_log(format!("prepared {} {} for task {} at revision {}", kind,
                                                       prepared.manifest.assignment_id, task.id, task.revision));
                    sent = format!("{}\n\n{sent}", prepared.text);
                    self.jobs[id - 1].task_assignment = Some(prepared);
                    // Only the thread this turn resumed may later stand in for an unobserved thread id.
                    self.jobs[id - 1].task_resume = resume.clone();
                }
                Ok(None) => {}
                Err(why) => {
                    let why = why.to_string();
                    // Assignment state is required once a task is selected. Fail before dispatch and roll back the
                    // optimistic generic-handoff markers too; no cursor has advanced and no model call was made.
                    self.roll_back_turn(id, agent, heard.clone(), untold.clone());
                    self.end(id, Status::Failed(why.clone()));
                    return Err(why);
                }
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
        if prompt.trim() != "/compact" && !self.turn_policy.instruction.is_empty() {
            sent = format!("{}\n\n{sent}", self.turn_policy.instruction);
        }
        // Keep the validated image blocks, with the handoff added to the text alone.
        if let Some(blocks) = content.as_array_mut() {
            blocks[0]["text"] = sent.clone().into();
        } else {
            content = sent.clone().into();
        }
        // `sent` is what the agent gets; the owner's message (`prompt`) stays the job's title, prompt.txt and what
        // decides the turn's settings
        if !write && prompt.trim() != "/compact" {
            let job = &mut self.jobs[id - 1];
            job.fresh_thread = resume.is_none();
            job.sent_bytes = sent.len() as u64;
        }
        let (mut input, mut turn) = (Some(sent.clone()), None);
        let style = match spec.adapter {
            #[cfg(feature = "turnrun")]
            Adapter::Codex if self.trial_broker.is_some() => {
                cmd = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
                cmd.arg("--codex-broker-client").arg(self.trial_broker.as_ref().unwrap());
                input = Some(format!("{}\n", serde_json::json!({"prompt": sent, "resume": resume})));
                Style::Codex
            }
            #[cfg(feature = "turnrun")]
            Adapter::Claude if self.trial_broker.is_some() => {
                // Session still owns policy, handoff, continuation and event accounting. The
                // client has no provider credentials or task filesystem access: it relays one
                // request to the trusted controller, which owns all execution authority.
                cmd = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
                cmd.arg("--broker-client").arg(self.trial_broker.as_ref().unwrap());
                if let Some(session) = &resume { cmd.args(["--resume", session]); }
                input = Some(format!("{}\n", serde_json::json!({"type": "user", "message": {"role": "user", "content": content}})));
                Style::Claude
            }
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
                // Compact within long turns too: between-turn handoffs cannot stop one autonomous turn from
                // repeatedly re-reading a large history. Native compaction preserves the running task and handles
                // tool boundaries itself; the owner's config.toml can still choose another threshold.
                if !self.codex_sets("model_auto_compact_token_limit") {
                    cmd.args(["-c", &format!("model_auto_compact_token_limit={CODEX_COMPACT_AT}")]);
                }
                let mut thread = serde_json::json!({"cwd": cwd, "sandbox": sandbox,
                                                    "approvalPolicy": if auto { "on-request" } else { "never" }});
                if auto {
                    thread["approvalsReviewer"] = "auto_review".into();
                }
                // the owner's choice of model (/model, or a task assignment's explicit model); given on resuming too,
                // so a conversation goes on with it
                if let Some(model) = &chosen_model {
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
                                               "params": {"input": content}}));
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
                // the owner's choice of model (/model, or a task assignment's explicit model): an alias or a full
                // name, as Claude Code's --model takes it. With none, a specialist runs on the model its file names,
                // as it does in Claude Code.
                if let Some(model) = chosen_model.as_ref().or(own_model.as_ref()) {
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
                // Use Claude Code's minimum supported window, on new and resumed turns. Its native compaction
                // summarizes history mid-turn while keeping the system prompt and task. An owner who sets the
                // variable keeps their own; native controls that disable auto-compaction are left alone too.
                if std::env::var_os("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none() {
                    cmd.env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", CLAUDE_COMPACT_WINDOW.to_string());
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
                input = Some(format!("{}\n", serde_json::json!({"type": "user", "message": {"role": "user", "content": content}})));
                Style::Claude
            }
            Adapter::Agy => {
                // Measured with agy 1.5.8 (2026-11-14): headless cannot prompt, and a tool without an allow rule is
                // auto-denied — write_file included, so the 1.4-era `--mode accept-edits` authorized nothing here and
                // an edits turn could not edit at all. The rules live in the settings now, and deny wins over allow
                // (measured): an edits turn allows file edits and sandboxed commands; in auto mode it allows the
                // owner's MCP servers too (aggressive connector actions are that mode's own promise; ask mode holds
                // them back, since agy has no prompt channel to bring a connector action to the owner). Measured on
                // the floor: only the bare `write_file(*)` wildcard matches (every scoped variant — exact path, `/*`,
                // `**`, `./*`, bare name — denied even a write inside the working folder), and `command(*)` writes
                // outside the folder too, so agy's sandbox is a network one; the real floor is the deny rules plus
                // this shell's undo trees. `unsandboxed(*)` and the browser's fetch stay denied everywhere, and a
                // read turn denies everything it always did. A denied tool returns an error the model sees and works
                // around.
                // The prompt goes in on stdin as one stream-json message (verified with agy 1.4.12, which drops a
                // flag's prompt in this mode): no argument-length limit, and the process table never shows it.
                let (allow, deny): (&[&str], &[&str]) = if !edits {
                    (&[], &["write_file(*)", "command(*)", "unsandboxed(*)", "mcp(*)", "execute_url(*)"])
                } else if self.mode == Mode::Auto {
                    (&["write_file(*)", "command(*)", "mcp(*)"], &["unsandboxed(*)", "execute_url(*)"])
                } else {
                    (&["write_file(*)", "command(*)"], &["unsandboxed(*)", "execute_url(*)", "mcp(*)"])
                };
                let permissions = if allow.is_empty() {
                    format!(r#"{{"permissions":{{"deny":{}}}}}"#, serde_json::json!(deny))
                } else {
                    format!(r#"{{"permissions":{{"allow":{},"deny":{}}}}}"#,
                            serde_json::json!(allow), serde_json::json!(deny))
                };
                cmd.args(["--input-format", "stream-json", "--output-format", "stream-json", "--settings"])
                    .arg(permissions);
                // A read turn's workspace gains this job's folder (--add-dir, measured 1.5.8), so the pointer to the
                // whole record is one it can follow. An edits turn gains none: it does not need it (its allow rules
                // are not path-scoped — measured), and the record it is handed carries its context.
                if !edits {
                    cmd.arg("--add-dir").arg(&dir);
                }
                // agy's default model has low, medium and high only: given xhigh, every turn failed at once (agy
                // 1.4.12, gemini-3.8-flash; still true on 1.5.8). ceiling: a model of agy's that has xhigh still gets high.
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
            self.roll_back_turn(id, agent, heard, untold);
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
        if images::has_images(question) {
            return Err("send images to @claude or @codex on a plain line".into());
        }
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
        // A setup failure must not leave the job Running: no child exists to end it, so a later turn would be
        // refused as busy and /kill would have no process to stop. Mark it failed here, as the spawn path below does.
        let stderr = match fs::File::create(dir.join("stderr.txt")) {
            Ok(file) => file,
            Err(e) => {
                let reason = format!("{}: {e}", cmd.get_program().to_string_lossy());
                self.end(id, Status::Failed(reason.clone()));
                return Err(reason);
            }
        };
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stderr(stderr);
        cmd.stdout(if trace.is_some() { Stdio::null() } else { Stdio::piped() });
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0); // its own group: /kill stops every descendant, and Ctrl-C in the shell does not
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NEW_PROCESS_GROUP (0x0200) is the Windows stand-in for `process_group(0)`: Ctrl-C in the shell
            // does not reach the agent, and /kill's taskkill /T still takes the whole tree. CREATE_NO_WINDOW
            // (0x0800_0000) keeps a console child from flashing a window over the shell. Both are safe here because
            // every spawn in this path pipes stdin, stdout and stderr — the interactive sign-in flow spawns on its
            // own, with the owner's console inherited.
            cmd.creation_flags(0x0800_0200);
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
            let mut events = EventLog::new((style != Style::Trace).then(|| fs::OpenOptions::new().create(true)
                .append(true).open(dir.join("events.jsonl")).ok()).flatten(), framing::MAX_EVENTS_LOG);
            let mut handle = |line: &str, parser: &mut Parser| {
                events.write(line);
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
                // Bounded framing: a frame over the cap or not UTF-8 fails the turn closed (the child is stopped), and
                // the pipe is still drained so nothing blocks on it. A cut final frame goes to the parser, which
                // rejects it unless it is whole JSON.
                let mut frames = framing::FrameReader::new(BufReader::new(out), framing::MAX_FRAME);
                if style == Style::Plain {
                    frames = frames.lossy(); // free text speaks no protocol: still capped, never refused for its bytes
                }
                let mut violated = false;
                while let Some(frame) = frames.next_frame() {
                    match frame {
                        framing::Frame::Line { text, .. } if !violated => handle(text.trim_end(), &mut parser),
                        framing::Frame::Line { .. } => {}
                        framing::Frame::Oversized { bytes } if !violated => {
                            violated = true;
                            let reason = format!("an output line over the {}-byte limit ({bytes} bytes seen before it was cut off)", framing::MAX_FRAME);
                            for note in parser.violation(reason) {
                                if style != Style::Trace {
                                    stop_group(pid, Duration::from_secs(3), false);
                                }
                                let _ = tx.send(Event::Note(id, note));
                            }
                        }
                        framing::Frame::InvalidUtf8 if !violated => {
                            violated = true;
                            for note in parser.violation("a protocol frame that is not valid UTF-8".into()) {
                                if style != Style::Trace {
                                    stop_group(pid, Duration::from_secs(3), false);
                                }
                                let _ = tx.send(Event::Note(id, note));
                            }
                        }
                        _ => {}
                    }
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
        let base = match self.jobs[id - 1].kind { Kind::Agent { agent, .. } => self.base_median(agent), _ => None };
        let job = &mut self.jobs[id - 1];
        job.elapsed = Some(job.started.elapsed());
        // The monotonic clock stops while the computer sleeps and the wall clock does not: a turn whose wall time ran
        // more than 60 s past its own elapsed time had the computer asleep under it, and a connection that broke then
        // is the sleep's doing, not the backend's.
        let slept = job.wall_start.elapsed().ok().and_then(|wall| awake::slept_secs(job.started.elapsed(), wall));
        if let Some(secs) = slept {
            job.push_log(format!("the computer slept for ~{} min during this turn", (secs + 30) / 60));
        }
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
        // out. A reported zero delta is retained; a pipeline that made no call leaves no line.
        let called = match job.kind { Kind::Ringo { .. } => job.calls > 0, Kind::Agent { .. } => job.pid.is_some() };
        let unknown = !job.counted && used.total() == 0 && status == Status::Done && called;
        // Message-level counts without a final total cover only part of the call.
        if matches!(job.kind, Kind::Agent { .. }) && job.live.total() > 0 {
            job.uncounted += 1;
        }
        if job.counted || used.total() > 0 || unknown {
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
            // What the usage is of, for the ledger (docs/TOKEN-USAGE.md): the surface and how it is billed, the model calls
            // it made when it reported them, how its context splits into fixed, carried and growth, and the sizes of the
            // record a fresh start began from. Additive: older rows lack these and every reader ignores unknown keys.
            let (surface, billing) = match &job.kind {
                Kind::Agent { agent, .. } => match agent.spec().adapter {
                    Adapter::Codex => ("cli:codex", "subscription"),
                    Adapter::Claude => ("cli:claude", "subscription"),
                    Adapter::Agy => ("cli:agy", "subscription"),
                    Adapter::Plain => ("plain", "unknown"),
                },
                Kind::Ringo { .. } => ("pipeline", "unknown"),
            };
            record["surface"] = surface.into();
            record["billing"] = billing.into();
            if job.calls > 0 {
                record["calls"] = job.calls.into();
            }
            if let (Kind::Agent { .. }, Some(b)) = (&job.kind, base) {
                if let Some((fixed, carry, growth)) = attribute(&job.call_inputs, b) {
                    record["ctx_base"] = b.into();
                    record["ctx_fixed"] = fixed.into();
                    record["ctx_carry"] = carry.into();
                    record["ctx_growth"] = growth.into();
                }
            }
            if let Some((inline, whole)) = job.record_bytes {
                record["record_inline_bytes"] = inline.into();
                if let Some(whole) = whole {
                    record["record_whole_bytes"] = whole.into();
                }
            }
            if let Some(secs) = slept {
                record["host_slept_s"] = secs.into();
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
            append_row(&self.root.join("usage.jsonl"), &record);
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

    /// Acknowledge a prepared task assignment after provider-backed activity. A process spawn, thread initialization or
    /// clean exit alone does not get here. Failed persistence leaves the previous cursor intact for the next turn.
    fn acknowledge_assignment(&mut self, id: usize) {
        let Some(prepared) = self.jobs[id - 1].task_assignment.take() else { return };
        let Kind::Agent { agent, .. } = self.jobs[id - 1].kind else { return };
        // Only the thread this turn resumed may stand in for an unobserved thread id; a fresh turn's activity must
        // never acknowledge an older conversation's cursor.
        let thread = self.jobs[id - 1].task_thread.clone().or(self.jobs[id - 1].task_resume.clone());
        let Some(thread) = thread else {
            self.jobs[id - 1].task_assignment = Some(prepared); // a native activity note can supply it later
            return;
        };
        let result = self.task_store().map_err(|e| e.to_string()).and_then(|store| {
            let task = store.load(&prepared.manifest.task_id).map_err(|e| e.to_string())?;
            task_assignment::AssignmentStore::new(&self.root, &task.repository)
                .and_then(|assignments| assignments.acknowledge(&task, agent.name(), &thread, &prepared.manifest.assignment_id))
                .map_err(|e| e.to_string())
        });
        if let Err(why) = result {
            // Do not retry every event in this turn: the durable cursor is authoritative and unadvanced. The next
            // assignment preparation will repeat the still-pending state.
            self.jobs[id - 1].push_log(format!("task assignment was not acknowledged: {why}; its state will be included again next turn"));
        }
    }

    /// Folds pending events into the jobs; true when anything changed.
    /// Hold the computer awake while a backend process runs and the owner is not the one being waited on.
    fn keep_awake(&mut self) {
        let wanted = self.jobs.iter().any(|j| j.running() && j.pid.is_some() && j.asks.is_empty());
        if let Some(line) = self.awake.as_mut().and_then(|a| a.tick(wanted, Instant::now())) {
            if let Some(job) = self.jobs.iter_mut().rev().find(|j| j.running()) {
                job.push_log(line);
            }
        }
    }

    pub fn pump(&mut self) -> bool {
        let mut changed = self.take_asks();
        self.keep_awake();
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
                    self.jobs[id - 1].task_thread = Some(session.clone());
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
                    if let Note::Turn { thread, .. } = &note {
                        self.jobs[id - 1].task_thread = Some(thread.clone());
                    }
                    // Turn and thread creation identify the conversation but are not delivery: a turn that is
                    // created and then fails before its first model request must leave the assignment pending.
                    // A model action is delivery; usage is judged below, where this turn's own increment is known —
                    // a resumed thread's unchanged cumulative total is history, not work.
                    let acted = note_acknowledges_delivery(&note);
                    let mut used = false;
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
                            // A cumulative total only proves this turn worked when this process has already seen the
                            // thread's earlier total. A watermark restored from a kept file can be behind a turn that
                            // crashed after the provider advanced, so the first report after a restart is history, not
                            // evidence; a model action or reply acknowledges that turn instead.
                            let mut trusted = true;
                            let t = match job.kind {
                                Kind::Agent { agent, write: false }
                                    if so_far || matches!(agent.spec().adapter, Adapter::Codex | Adapter::Agy) =>
                                {
                                    let key = (agent, self.threads.get(&agent).cloned().unwrap_or_default());
                                    let known = self.usage_synced.contains(&key);
                                    let before = self.usage_so_far.get(&key).copied().unwrap_or_default();
                                    if t.total() >= before.total() {
                                        self.usage_so_far.insert(key.clone(), t);
                                    }
                                    // Observed here from now on: a later delta in this turn is a real increment.
                                    self.usage_synced.insert(key);
                                    trusted = known;
                                    // agy tells each model call's own count as it goes (`Live`), and an agy turn
                                    // takes no line while it works, so one result ends it: the calls summed are the
                                    // turn's own, whatever total was kept. A total kept before agy's cache reads were
                                    // counted in full (2026-11-14) holds too few of them, and taking it off would
                                    // count the conversation's earlier cache reads in this turn again. A turn whose
                                    // calls told no count is still counted by what its total added.
                                    if matches!(agent.spec().adapter, Adapter::Agy) && job.live.total() > 0 {
                                        job.live
                                    } else {
                                        t.since(before)
                                    }
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
                            // Only usage this turn added proves the model worked. A resumed Codex or agy report
                            // that repeats the thread's earlier total adds nothing, and a first report after a
                            // restart may be history the restored watermark missed.
                            if t.total() > 0 {
                                if trusted {
                                    used = true;
                                }
                                // the turn's own count replaces what its messages said; a result without one (an
                                // error result) keeps them
                                job.live = Tokens::default();
                            }
                        }
                        Note::Live(t) => {
                            job.counted = true;
                            // Codex's live total is its thread's so far (its updates carry the earlier turns of a
                            // resumed conversation), so the turn's own is told the same way its final count is.
                            // Claude's and agy's totals are each run's own already. A live total is delivery evidence
                            // only when this process already knew the thread's earlier total; after a restart the
                            // restored watermark can lag a crashed turn, so a model action or reply settles it.
                            let mut trusted = true;
                            job.live = match job.kind {
                                Kind::Agent { agent, write: false }
                                    if matches!(agent.spec().adapter, Adapter::Codex) =>
                                {
                                    let key = (agent, self.threads.get(&agent).cloned().unwrap_or_default());
                                    trusted = self.usage_synced.contains(&key);
                                    let before = self.usage_so_far.get(&key).copied().unwrap_or_default();
                                    t.since(before)
                                }
                                _ => t,
                            };
                            // A live total that equals the thread's earlier one added nothing this turn.
                            if trusted && job.live.total() > 0 {
                                used = true;
                            }
                            // The turn's tokens so far (final counts plus live ones: a line the owner sends while
                            // it works ends in a final count of its own, and the live count starts again from 0)
                            // past the next line: a warning in its log, with /kill named.
                            let so_far = job.tokens_so_far();
                            let next = if job.warned == 0 { LIVE_WARN_AT } else { job.warned * 2 };
                            if so_far >= next {
                                let mut line = next;
                                while so_far >= line * 2 {
                                    line *= 2;
                                }
                                job.warned = line;
                                let who = match job.kind {
                                    Kind::Agent { agent, .. } => agent.name(),
                                    Kind::Ringo { .. } => "ringo",
                                };
                                job.push_log(format!("⚠ {who}'s turn has used {} tokens so far and goes on. /kill {} stops it if it is wandering; the next warning is at {}.",
                                                     fmt_k(so_far), job.id, fmt_k(line * 2)));
                            }
                        }
                        Note::Context(n) => {
                            if let Kind::Agent { agent, write: false } = job.kind {
                                self.context.insert(agent, n);
                                // the first call of a new conversation read the fixed base plus what was sent
                                if job.fresh_thread && !job.measured && n > 0 {
                                    job.measured = true;
                                    let bases = self.bases.entry(agent).or_default();
                                    bases.push_back(n.saturating_sub(job.sent_bytes / 4).max(1_000));
                                    while bases.len() > 3 {
                                        bases.pop_front();
                                    }
                                }
                            }
                        }
                        Note::Call => job.calls += 1,
                        Note::CallInput(n) => job.call_inputs.push(n),
                        Note::Touched(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path);
                            job.touched.insert(rel.to_string_lossy().into_owned());
                        }
                        Note::Read(path) => {
                            let cwd = job.worktree.clone().unwrap_or_else(|| self.repo.clone());
                            let path = Path::new(&path);
                            let rel = path.strip_prefix(&cwd).or_else(|_| path.strip_prefix(cwd.canonicalize().unwrap_or_default()))
                                .unwrap_or(path).to_string_lossy().into_owned();
                            job.read.retain(|r| *r != rel);
                            job.read.push(rel);
                            if job.read.len() > READ_FILES {
                                job.read.remove(0);
                            }
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
                    // Provider-backed delivery, not process creation: a model action, or usage this turn added.
                    if acted || used {
                        self.acknowledge_assignment(id);
                    }
                }
            }
        }
        changed
    }

    /// What finished jobs used in the last `secs` seconds, by agent, from `usage.jsonl` (every session's), and when its
    /// first record was written.
    pub fn used_since(&self, secs: u64) -> (BTreeMap<String, Tokens>, Option<u64>) {
        let (mut windows, first) = self.used_windows(&[secs]);
        (windows.remove(0), first)
    }

    /// How many non-blank lines of `usage.jsonl` are not a readable JSON row (a torn or hand-edited line). Every
    /// reader skips them, so /usage says how many there were rather than silently under-counting.
    pub fn unreadable_usage_rows(&self) -> usize {
        fs::read_to_string(self.root.join("usage.jsonl")).unwrap_or_default().lines()
            .filter(|l| !l.trim().is_empty() && !serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| v.is_object()))
            .count()
    }

    /// The same counts for each of several windows in one read of `usage.jsonl`: /usage asks for a day and a week,
    /// and the record is one short line a job (a year of daily use is a few MB).
    pub fn used_windows(&self, secs: &[u64]) -> (Vec<BTreeMap<String, Tokens>>, Option<u64>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut by: Vec<BTreeMap<String, Tokens>> = secs.iter().map(|_| BTreeMap::new()).collect();
        let mut first = None;
        for line in fs::read_to_string(self.root.join("usage.jsonl")).unwrap_or_default().lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let at = r["at"].as_u64().unwrap_or(0);
            first = first.or(Some(at));
            let age = now.saturating_sub(at);
            let n = |k: &str| r[k].as_u64().unwrap_or(0);
            for (window, &limit) in by.iter_mut().zip(secs) {
                if age <= limit {
                    let t: &mut Tokens = window.entry(r["agent"].as_str().unwrap_or("?").to_string()).or_default();
                    t.add(Tokens { input: n("input"), cache_read: n("cache_read"), cache_write: n("cache_write"), output: n("output") });
                }
            }
        }
        (by, first)
    }

    /// The live job whose durable private folder is `dir`. A job from an earlier shell has no entry: its record and
    /// evidence on disk are the only evidence left.
    pub fn job_by_dir(&self, dir: &Path) -> Option<&Job> {
        self.jobs.iter().find(|job| job.dir == dir)
    }

    /// A finished job's usage record, read from `usage.jsonl` by its durable folder. An operation with no record is
    /// unknown, never a free call.
    pub fn job_usage(&self, dir: &Path) -> JobUsage {
        let wanted = dir.to_string_lossy();
        let mut found = None;
        for line in fs::read_to_string(self.root.join("usage.jsonl")).unwrap_or_default().lines() {
            let Ok(r) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if r["job"].as_str() != Some(wanted.as_ref()) {
                continue;
            }
            let n = |key: &str| r[key].as_u64().unwrap_or(0);
            found = Some(JobUsage {
                recorded: true,
                unknown: r["unknown"].as_bool().unwrap_or(false)
                    || ["input", "cache_read", "cache_write", "output"]
                        .iter().any(|key| r[*key].as_u64().is_none())
                    || (r.get("uncounted_calls").is_some() && r["uncounted_calls"].as_u64().is_none()),
                uncounted_calls: n("uncounted_calls"),
                tokens: Tokens { input: n("input"), cache_read: n("cache_read"), cache_write: n("cache_write"),
                                 output: n("output") },
            });
        }
        found.unwrap_or_default()
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
        if let Ok(checks) = self.check_store() { checks.shutdown(); }
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

/// True when a provider note is a model action that proves the turn's prompt reached the model. Turn and thread
/// creation identify the conversation but are not delivery, and a usage note is not either: a resumed thread's
/// unchanged cumulative total is history, so the token accounting decides from the turn's own increment.
fn note_acknowledges_delivery(note: &Note) -> bool {
    matches!(note, Note::Step(_) | Note::Looked { .. } | Note::Partial(_) | Note::Reply(_))
}

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

#[derive(Clone, Debug, PartialEq)]
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
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; cmd.process_group(0); }
    let Ok(child) = cmd.spawn() else { return SignIn::No("its status command did not run".into()) };
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || { let _ = tx.send(child.wait_with_output()); });
    let out = match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(out)) => out,
        _ => {
            stop_group(pid, Duration::from_millis(100), false);
            return SignIn::No("status probe timed out or failed; retry /task capabilities".into());
        }
    };
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

    #[test]
    fn the_event_log_ends_at_its_cap_with_one_marker_and_writes_nothing_after() {
        use std::{cell::RefCell, rc::Rc};
        #[derive(Clone, Default)]
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.borrow_mut().extend_from_slice(b); Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let buf = Shared::default();
        let cap = 40 + EVENTS_LOG_MARKER.len() as u64 + 1;
        let mut log = EventLog::new(Some(buf.clone()), cap);
        log.write("first line of twenty"); // 21 bytes with its newline
        log.write("a record too long to fit the budget left");
        log.write("tiny"); // would fit, but the log has ended
        let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
        assert_eq!(text, format!("first line of twenty\n{EVENTS_LOG_MARKER}\n"));
        assert!(text.len() as u64 <= cap);
    }
    use super::*;

    #[test]
    fn delivery_needs_model_activity_not_just_a_created_turn() {
        // A created turn or session identifies the conversation; it does not prove the model received anything.
        assert!(!note_acknowledges_delivery(&Note::Turn { thread: "t".into(), turn: "u".into() }));
        assert!(!note_acknowledges_delivery(&Note::Session("t".into())));
        // A usage report is not delivery by itself: the token accounting acknowledges only what this turn added,
        // because a resumed thread repeats its earlier total.
        assert!(!note_acknowledges_delivery(&Note::Live(Tokens::default())));
        assert!(!note_acknowledges_delivery(&Note::Tokens(Tokens::default())));
        let used = Tokens { input: 12, output: 3, ..Tokens::default() };
        assert!(!note_acknowledges_delivery(&Note::Live(used)));
        assert!(!note_acknowledges_delivery(&Note::TokensSoFar(used)));
        assert!(note_acknowledges_delivery(&Note::Step("$ ls".into())));
        assert!(note_acknowledges_delivery(&Note::Looked { kind: "read", text: "read f".into() }));
        assert!(note_acknowledges_delivery(&Note::Partial("some".into())));
        assert!(note_acknowledges_delivery(&Note::Reply("done".into())));
    }

    #[test]
    fn a_fresh_handoff_that_then_fails_to_prepare_leaves_the_conversation_retryable() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        // Preparation must fail before this executable can run; do not require a provider CLI on test hosts.
        s.programs.insert(agent, std::env::current_exe().unwrap());
        let prior = record_turn(&mut s, "the earlier turn");
        fs::write(s.jobs[prior - 1].dir.join("reply.md"), "an earlier reply").unwrap();
        // A task whose required state cannot fit: preparation fails before dispatch, after the fresh handoff
        // already replaced the agent's native conversation state.
        let store = s.task_store().unwrap();
        let task = store.create("overflow-task", &"🌍".repeat(15_000)).unwrap();
        s.select_task(&task.id).unwrap();
        s.handoff_at = 0;
        s.context.insert(agent, 1_000_000);
        s.threads.insert(agent, "thread-1".into());
        s.usage_so_far.insert((agent, "thread-1".into()), Tokens { input: 11, output: 7, ..Tokens::default() });
        let error = s.launch_agent(agent, false, "the owner's message").unwrap_err();
        assert!(error.contains("context budget"), "{error}");
        // The conversation is back for a retry, and the job did not stay Running.
        assert_eq!(s.threads.get(&agent), Some(&"thread-1".to_string()));
        assert!(s.usage_so_far.contains_key(&(agent, "thread-1".to_string())));
        assert_eq!(s.context.get(&agent), Some(&1_000_000));
        assert!(matches!(s.jobs.last().unwrap().status, Status::Failed(_)));
    }

    #[test]
    fn a_spawn_setup_failure_marks_the_job_failed_instead_of_leaving_it_running() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        let id = s.new_job(Kind::Agent { agent, write: false }, "a turn that cannot start").unwrap();
        // stderr.txt cannot be created: a directory occupies its path (a full disk fails the same way). No child
        // will exist to end this job, so it must not stay Running: the next turn would be refused as busy.
        fs::create_dir(s.jobs[id - 1].dir.join("stderr.txt")).unwrap();
        let error = s.spawn(id, Command::new("true"), Parser::new(Style::Plain), None, None).unwrap_err();
        assert!(error.contains("true"), "{error}");
        assert!(matches!(s.jobs[id - 1].status, Status::Failed(_)));
        assert!(!s.jobs[id - 1].running());
    }

    /// An agent that writes a newline-free frame past the cap is stopped and the turn fails closed, with bounded memory.
    #[cfg(unix)]
    #[test]
    fn an_oversized_frame_fails_the_turn_closed() {
        let mut s = record_session();
        let agent = Agent::from_name("claude").unwrap();
        let id = s.new_job(Kind::Agent { agent, write: false }, "a runaway stream").unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 20000000 /dev/zero | tr '\\0' a; sleep 30"]);
        s.spawn(id, cmd, Parser::new(Style::Claude), None, None).unwrap();
        let start = Instant::now();
        while s.jobs[id - 1].running() && start.elapsed() < Duration::from_secs(20) {
            s.pump();
            thread::sleep(Duration::from_millis(20));
        }
        match &s.jobs[id - 1].status {
            Status::Failed(why) => assert!(why.contains("output line over the"), "{why}"),
            other => panic!("expected a failed job, got {other:?}"),
        }
    }

    #[test]
    fn wsl_shows_in_the_distro_variable_and_proc_version() {
        use std::ffi::OsStr;
        assert!(wsl_from(Some(OsStr::new("Ubuntu-22.04")), false, None));
        assert!(wsl_from(None, true, None));
        assert!(wsl_from(None, false, Some("Linux version 5.15.167.4-microsoft-standard-WSL2")));
        assert!(wsl_from(None, false, Some("Linux version 4.4.0-19041-Microsoft")));
        assert!(!wsl_from(None, false, Some("Linux version 6.6.0 (gcc)")));
        assert!(!wsl_from(None, false, None)); // plain Linux: no variable, no microsoft in /proc/version
    }

    /// A long grace must not delay the forced stop, and descendants must end as well as the parent.
    /// Keep handles to both processes so the result does not depend on PID reuse or tasklist's output.
    #[cfg(windows)]
    #[test]
    fn a_windows_stop_ends_the_tree_without_waiting_for_a_grace() {
        use std::ffi::c_void;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::os::windows::process::CommandExt;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        }

        // Redirect the descendant's output so only its PID reaches our readiness pipe. Both processes would
        // otherwise live for a minute, far longer than the stop deadline. Test agents and pipelines alike.
        let script = r#"
            $ErrorActionPreference = 'Stop'
            $s = New-Object System.Diagnostics.ProcessStartInfo
            $s.FileName = "$env:SystemRoot\System32\ping.exe"
            $s.Arguments = '-n 60 127.0.0.1'
            $s.UseShellExecute = $false
            $s.CreateNoWindow = $true
            $s.RedirectStandardOutput = $true
            $s.RedirectStandardError = $true
            $child = [System.Diagnostics.Process]::Start($s)
            [Console]::WriteLine($child.Id)
            Start-Sleep -Seconds 60
        "#;
        for pipeline in [false, true] {
            let mut parent = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", script])
                .creation_flags(0x0800_0200).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
                .spawn().expect("spawn the parent");
            let mut line = String::new();
            BufReader::new(parent.stdout.take().unwrap()).read_line(&mut line).unwrap();
            let pid: u32 = line.trim().parse().expect("parent reports its live descendant's PID");
            // SAFETY: requests only a synchronization handle to the known live descendant, with no inheritance.
            let raw = unsafe { OpenProcess(0x0010_0000, 0, pid) }; // SYNCHRONIZE
            assert!(!raw.is_null(), "open descendant: {}", std::io::Error::last_os_error());
            // SAFETY: OpenProcess returned a new, valid, exclusively owned handle.
            let descendant = unsafe { OwnedHandle::from_raw_handle(raw) };
            let started = Instant::now();
            stop_group(parent.id(), Duration::from_secs(60), pipeline);
            let returned = started.elapsed();
            // SAFETY: both handles remain owned and valid for these bounded waits. WAIT_OBJECT_0 is zero.
            let (parent_ended, descendant_ended) = unsafe {
                (WaitForSingleObject(parent.as_raw_handle(), 10_000) == 0,
                 WaitForSingleObject(descendant.as_raw_handle(), 10_000) == 0)
            };
            // Clean up even if the regression returns: taskkill the parent tree, then the descendant by PID.
            for remaining in [parent.id(), pid] {
                let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &remaining.to_string()])
                    .creation_flags(0x0800_0000).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
            let _ = parent.wait();
            assert!(returned < Duration::from_secs(1), "stop_group blocked the caller for {returned:?}");
            assert!(parent_ended && descendant_ended, "both parent and descendant must stop before the 60 s grace");
        }
    }

    #[test]
    fn a_carried_record_keeps_its_newest_whole_lines() {
        let record = format!("{}\n#1 · the owner to claude: é\nclaude: done\n#2 · the owner to codex: ü\ncodex: done\n", "old ".repeat(50));
        let record = record.as_str();
        assert_eq!(newest(record, record.len()), record, "it fits");
        let marker = "(the earliest turns before the restart are cut)\n";
        assert_eq!(newest(record, marker.len() + 31), format!("{marker}codex: done\n"));
        // a cut inside a two-byte letter moves past it instead of splitting it
        let at = record.find('ü').unwrap() + 1;
        assert!(newest(record, marker.len() + record.len() - at).ends_with("codex: done\n"));
        assert_eq!(newest(record, 0), "");
        for room in 0..record.len() {
            assert!(newest(record, room).len() <= room, "room {room}");
            assert!(record_head(record, room).len() <= room, "room {room}");
        }
    }

    /// Construct histories directly: no agents or providers, including when generating hundreds of turns.
    fn record_session() -> Session {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("ringo-record-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        fs::create_dir_all(&path).unwrap();
        // Exercise record diffs in a normal project repository; the outside-project undo store has a separate
        // Windows extended-path constraint that is unrelated to record sizing.
        git(&path, &["init", "-q"]).unwrap();
        Session::new(&path).unwrap()
    }

    fn record_turn(s: &mut Session, task: &str) -> usize {
        let agent = Agent::from_name("codex").unwrap();
        let id = s.new_job(Kind::Agent { agent, write: false }, task).unwrap();
        s.jobs[id - 1].status = Status::Done;
        id
    }

    #[test]
    fn long_histories_and_oversized_single_turns_stay_inside_the_inline_byte_budget() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        for n in 0..200 {
            let id = record_turn(&mut s, &format!("constraint-{n}: {}", "界🌍".repeat(700)));
            let job = &mut s.jobs[id - 1];
            job.talk = true;
            fs::write(job.dir.join("reply.md"), "result🌍".repeat(1000)).unwrap();
        }
        let (inline, whole) = s.fresh_record(agent, &s.lane(agent, s.jobs.len() + 1), 0);
        assert!(inline.len() <= RECORD_BYTES, "{} bytes", inline.len());
        assert!(inline.contains("constraint-199:") && inline.contains("earlier turns"));
        assert!(whole.contains("constraint-0:") && whole.contains("constraint-199:"));

        // Metadata alone used to escape the bound even when there was only one turn.
        s.jobs.truncate(1);
        let job = &mut s.jobs[0];
        job.changed = (0..5000).map(|n| format!("file-{n}-{}.py", "界".repeat(100))).collect();
        job.status = Status::Failed(format!("failure {} END-OF-FAILURE", "🌍".repeat(30_000)));
        let (inline, whole) = s.record(&s.lane(agent, 2));
        assert!(inline.len() <= RECORD_BYTES);
        assert!(inline.starts_with("#1 · the owner to codex: constraint-0:"));
        assert!(inline.contains("(cut; the whole is in the record)"));
        assert!(whole.contains("END-OF-FAILURE") && whole.contains("file-4999-"));
    }

    #[test]
    fn a_long_session_keeps_the_owners_first_request_and_names_the_range_it_omitted() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        for n in 0..60 {
            let task = if n == 0 { format!("ORIGINAL-REQUEST {}", "x".repeat(5000)) } else { format!("turn-{n}: {}", "界".repeat(700)) };
            let id = record_turn(&mut s, &task);
            s.jobs[id - 1].talk = true;
            fs::write(s.jobs[id - 1].dir.join("reply.md"), "r".repeat(3000)).unwrap();
        }
        let (inline, whole) = s.fresh_record(agent, &s.lane(agent, s.jobs.len() + 1), 0);
        assert!(inline.len() <= RECORD_BYTES, "{} bytes", inline.len());
        assert!(inline.contains("#1 · the owner to codex: ORIGINAL-REQUEST"), "turn #1 is pinned");
        let pin_end = inline.find("\n(#2 to #").expect("the notice follows the pin, naming #2 to #k");
        assert!(inline[..pin_end].len() <= OWNER_PIN_BYTES, "the pin is bounded");
        let k: usize = inline[pin_end..].split("to #").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
        assert!(inline.contains(&format!("#{}", k + 1)) && !inline.contains(&format!("#{k} · the owner")));
        assert!(inline.contains(&format!(", {} earlier turns", k - 1)), "the count is #2..=#k: {}", &inline[pin_end..pin_end + 80]);
        assert!(inline.contains("turn-59:"));
        assert!(whole.contains(&"x".repeat(5000)), "the whole record keeps all of it");
    }

    #[test]
    fn a_short_session_record_has_no_pin_and_is_unchanged() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        for n in 0..3 {
            let id = record_turn(&mut s, &format!("small task {n}"));
            s.jobs[id - 1].talk = true;
        }
        let (inline, _) = s.fresh_record(agent, &s.lane(agent, 4), 0);
        assert!(inline.starts_with("#1 · the owner to codex: small task 0\n"));
        assert_eq!(inline.matches("#1 ·").count(), 1, "told once, not pinned again");
        assert!(!inline.contains("earlier turns"));
    }

    #[test]
    fn unicode_diffs_and_repeated_restarts_share_the_record_budget() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        fs::write(s.repo.join("large.txt"), "old\n").unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let before = git(&s.repo, &["write-tree"]).unwrap();
        fs::write(s.repo.join("large.txt"), format!("{}\nEND-OF-DIFF\n", "🌍".repeat(30_000))).unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let after = git(&s.repo, &["write-tree"]).unwrap();
        let id = record_turn(&mut s, "keep this exact constraint");
        s.jobs[id - 1].before = Some(before);
        s.jobs[id - 1].after = Some(after);
        s.jobs[id - 1].changed = vec!["large.txt".into()];
        let (inline, whole) = s.record(&s.lane(agent, 2));
        assert!(inline.len() <= RECORD_BYTES);
        assert!(inline.contains("+🌍") && !inline.contains("END-OF-DIFF"));
        assert!(whole.contains("END-OF-DIFF"));
        // Each restart has a small new turn. Bound the saved pointer, restart separators and cut notice together.
        s.before.insert(agent, "old constraint\n".repeat(20_000));
        s.told.insert(agent, "old unheard constraint\n".repeat(20_000));
        for _ in 0..25 {
            let (inline, _) = s.fresh_record(agent, &s.lane(agent, 2), usize::MAX);
            assert!(inline.len() <= RECORD_BYTES, "fresh: {} bytes", inline.len());
            assert!(inline.contains("keep this exact constraint"));
            for (_, record) in s.carry() { assert!(record.len() <= RECORD_BYTES, "carry: {}", record.len()); }
            let records = s.carry_record();
            for (_, _, record) in &records { assert!(record.len() <= RECORD_BYTES, "saved: {}", record.len()); }
            s.resume(&[], &[], &records);
        }
    }

    /// A lane of finished turns (oldest first) with the given replies, as `record` sees them.
    fn reply_turns(s: &mut Session, replies: &[String]) {
        for (n, reply) in replies.iter().enumerate() {
            let id = record_turn(s, &format!("task-{}", n + 1));
            fs::write(s.jobs[id - 1].dir.join("reply.md"), reply).unwrap();
        }
    }

    fn two_paragraphs(tag: &str, second: usize) -> String {
        format!("{tag} first paragraph.\n\n{tag} {} {tag}-LAST-PARAGRAPH", "x".repeat(second))
    }

    #[test]
    fn the_record_names_the_files_the_turns_read_newest_first() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        reply_turns(&mut s, &["one".into(), "two".into(), "three".into()]);
        s.jobs[0].read = vec!["old.rs".into(), "shared.rs".into(), "edited.rs".into()];
        s.jobs[1].read = vec!["mid.rs".into()];
        s.jobs[2].read = vec!["shared.rs".into(), "new.rs".into()];
        s.jobs[1].changed = vec!["edited.rs".into()];
        let turns = s.lane(agent, 4);
        let (inline, whole) = s.record(&turns);
        let prefix = "files read (newest first; re-open what you need, they may have changed since): ";
        // newest turn first, and within a turn the newest read first; a file read again counts where it was read last;
        // a changed file is listed with its turn, not here
        assert!(inline.contains(&format!("\n{prefix}new.rs, shared.rs, mid.rs, old.rs\n")), "{inline}");
        assert!(!inline.contains("read: "), "the inline record has no per-turn line: {inline}");
        // each turn of the whole record says what it read, oldest first, nothing left out
        assert!(whole.contains("read: old.rs, shared.rs, edited.rs\n") && whole.contains("read: mid.rs\n")
                && whole.contains("read: shared.rs, new.rs\n"), "{whole}");
        assert!(!whole.contains("files read ("), "{whole}");
        // nothing read, nothing said
        let (mut lean, agent) = (record_session(), agent);
        reply_turns(&mut lean, &["one".into()]);
        let (inline, whole) = lean.record(&lean.lane(agent, 2));
        assert!(!inline.contains("files read") && !whole.contains("read:"), "{inline}");
    }

    #[test]
    fn the_files_read_line_is_bounded_in_paths_and_bytes() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        reply_turns(&mut s, &["one".into()]);
        // 40 paths at most, the newest first, and how many more are in the whole record
        s.jobs[0].read = (0..100).map(|n| format!("f{n:03}.rs")).collect();
        let (inline, whole) = s.record(&s.lane(agent, 2));
        let line = inline.lines().find(|l| l.starts_with("files read (")).unwrap();
        assert!(line.contains(": f099.rs, f098.rs, ") && line.contains("f060.rs") && !line.contains("f059.rs"), "{line}");
        assert!(line.ends_with(" and 60 more (in the whole record)"), "{line}");
        assert!(whole.contains("f000.rs, f001.rs") && whole.contains("f099.rs\n"));
        // 3_000 bytes at most, stopping before the path that would cross it
        s.jobs[0].read = (0..100).map(|n| format!("dir/{}-{n:03}.rs", "d".repeat(90))).collect();
        let (inline, _) = s.record(&s.lane(agent, 2));
        let line = inline.lines().find(|l| l.starts_with("files read (")).unwrap();
        assert!(line.len() <= 3_000, "{} bytes", line.len());
        let shown = line.matches(".rs").count();
        assert!((20..40).contains(&shown), "{shown} paths");
        assert!(line.ends_with(&format!(" and {} more (in the whole record)", 100 - shown)), "{line}");
        assert!(line.contains(&format!("-{:03}.rs", 100 - shown)) && !line.contains(&format!("-{:03}.rs", 99 - shown)), "{line}");
        // a record with no room left for it goes without it, and stays inside its budget: ten whole turns sized so
        // the space left runs from a few hundred bytes down to the 128 the turns leave free, less than this line
        let long = format!("src/{}.rs", "d".repeat(200));
        let (mut absent, mut present) = (0, 0);
        let mut s = record_session(); // one session: many made at once can run out of folder names
        reply_turns(&mut s, &vec![String::new(); 10]);
        for j in s.jobs.iter_mut() {
            j.talk = true;
            j.read = vec![long.clone()];
        }
        for k in (5_850..5_990).step_by(3) {
            for j in &s.jobs {
                fs::write(j.dir.join("reply.md"), "x".repeat(k)).unwrap();
            }
            let (inline, _) = s.record(&s.lane(agent, 11));
            assert!(inline.len() <= RECORD_BYTES, "{k}: {} bytes", inline.len());
            if !inline.contains("#1 · the owner") {
                break; // past here the oldest turn itself is left to the whole record
            }
            if inline.contains("files read (") { present += 1 } else { absent += 1 }
        }
        assert!(absent > 0 && present > 0, "absent {absent}, present {present}");
    }

    #[test]
    fn a_job_remembers_what_it_read_oldest_first_and_at_most_200() {
        let (mut s, agent) = ctx_session("codex", 0);
        let id = ctx_turn(&mut s, agent, Status::Running, 0);
        let repo = s.repo.clone();
        let send = |s: &Session, path: String| s.tx.send(Event::Note(id, Note::Read(path))).unwrap();
        for n in 0..205 {
            send(&s, repo.join(format!("src/f{n}.rs")).to_string_lossy().into_owned());
        }
        send(&s, repo.join("src/f100.rs").to_string_lossy().into_owned()); // read again: moves to the end
        send(&s, "/elsewhere/g.rs".into()); // outside the folder: as given
        s.pump();
        let read = &s.jobs[id - 1].read;
        // relative paths keep the platform's separator, as an edited file's do
        let rel = |n: usize| Path::new("src").join(format!("f{n}.rs")).to_string_lossy().into_owned();
        assert_eq!(read.len(), 200);
        assert_eq!(&read[..2], [rel(6), rel(7)], "the oldest five are dropped");
        assert_eq!(&read[198..], [rel(100), "/elsewhere/g.rs".to_string()]);
        assert_eq!(read.iter().filter(|r| **r == rel(100)).count(), 1);
    }

    #[test]
    fn a_cap_of_zero_is_the_lean_record_exactly() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        reply_turns(&mut s, &[two_paragraphs("a", 800), two_paragraphs("b", 800), two_paragraphs("c", 800)]);
        let turns = s.lane(agent, 4);
        let (lean, whole) = s.record(&turns);
        assert_eq!(s.record_with(&turns, 0), (lean.clone(), whole.clone()));
        // today's lean form, written out: older replies keep their first paragraph, the newest is whole
        let expected = format!("#1 · the owner to codex: task-1\ncodex: a first paragraph.\n\n\
                                #2 · the owner to codex: task-2\ncodex: b first paragraph.\n\n\
                                #3 · the owner to codex: task-3\ncodex: {}\n", two_paragraphs("c", 800));
        assert_eq!(lean, expected);
        assert!(!lean.contains("a-LAST-PARAGRAPH") && whole.contains("a-LAST-PARAGRAPH"));
        assert_eq!(s.fresh_record(agent, &turns, 0), (lean, whole));
    }

    #[test]
    fn a_cap_gives_older_replies_their_whole_form_newest_first_while_it_fits() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        // turn 2 has a long second paragraph, turn 1 a short one, turn 3 is the newest (whole already)
        reply_turns(&mut s, &[two_paragraphs("a", 300), two_paragraphs("b", 2_000), two_paragraphs("c", 10)]);
        let turns = s.lane(agent, 4);
        let (lean, whole) = s.record(&turns);
        let (rich, rich_whole) = s.record_with(&turns, 1_000_000);
        assert!(rich.contains("a-LAST-PARAGRAPH") && rich.contains("b-LAST-PARAGRAPH") && rich.contains("c-LAST-PARAGRAPH"), "{rich}");
        assert_eq!(rich_whole, whole, "the whole record does not depend on the cap");
        // the order of the turns and the newest turn are as they were
        assert!(rich.starts_with("#1 · ") && rich.find("#2 · ") < rich.find("#3 · "));
        assert!(rich.ends_with(lean.rsplit_once("#3 · ").unwrap().1), "{rich}");
        // a cap that has room for the short upgrade only: the newest brief turn (#2) does not fit, #1 does
        let cap = lean.len() + 400;
        let small = s.record_with(&turns, cap).0;
        assert!(small.contains("a-LAST-PARAGRAPH") && !small.contains("b-LAST-PARAGRAPH"), "{small}");
        assert!(small.len() <= cap && small.len() > lean.len());
        // room for the long one, not both: the newest that fits is upgraded, the older stays brief
        let cap = lean.len() + 2_100;
        let small = s.record_with(&turns, cap).0;
        assert!(small.contains("b-LAST-PARAGRAPH") && !small.contains("a-LAST-PARAGRAPH"), "{small}");
        assert!(small.len() <= cap);
        // a cap the lean record already fills adds nothing
        assert_eq!(s.record_with(&turns, lean.len()).0, lean);
        assert_eq!(s.record_with(&turns, lean.len() / 2).0, lean);
    }

    #[test]
    fn the_rich_tier_never_passes_the_inline_budget_and_keeps_the_diff_and_the_files_read() {
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        fs::write(s.repo.join("big.txt"), "old\n").unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let before = git(&s.repo, &["write-tree"]).unwrap();
        fs::write(s.repo.join("big.txt"), format!("{}\nEND-OF-DIFF\n", "diff-line\n".repeat(3_000))).unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let after = git(&s.repo, &["write-tree"]).unwrap();
        // a few turns that fit lean but not all whole, with replies near their 6000-character form
        let replies: Vec<String> = (0..30).map(|n| two_paragraphs(&format!("t{n}"), 5_000)).collect();
        reply_turns(&mut s, &replies);
        s.jobs[0].before = Some(before);
        s.jobs[29].after = Some(after);
        s.jobs[0].changed = vec!["big.txt".into()];
        s.jobs[5].read = vec!["src/a.rs".into(), "big.txt".into()];
        let turns = s.lane(agent, 31);
        let lean = s.record(&turns).0;
        for cap in [0, 1_000, 20_000, 40_000, RECORD_BYTES - 128, RECORD_BYTES, usize::MAX] {
            let rich = s.record_with(&turns, cap).0;
            assert!(rich.len() <= RECORD_BYTES && rich.len() >= lean.len(), "cap {cap}: {} bytes", rich.len());
            assert!(rich.len() <= lean.len().max(cap.min(RECORD_BYTES - 128)), "cap {cap}: {} bytes", rich.len());
            assert!(rich.contains("diff-line") && rich.contains("The change these turns made, as git shows it:"), "cap {cap}");
            assert!(rich.contains("files read (newest first; re-open what you need, they may have changed since): src/a.rs\n"), "cap {cap}");
            assert!(rich.find("files read (") < rich.find("The change these turns made"), "the line comes before the diff");
            assert!(rich.contains("#30 · "), "cap {cap}: the newest turn stays");
        }
        let rich = s.record_with(&turns, usize::MAX).0;
        assert!(rich.matches("-LAST-PARAGRAPH").count() > lean.matches("-LAST-PARAGRAPH").count());
    }

    #[test]
    fn the_cap_follows_the_cost_models_saving() {
        assert_eq!(record_cap(48_000, Some(13_000)), 4 * 13_125);
        assert_eq!(record_cap(48_000, None), 4 * 9_000, "an unknown base is half the conversation");
        assert_eq!(record_cap(10_000, Some(13_000)), 0, "no saving, no detail");
        assert_eq!(record_cap(0, None), 0);
        // half of the 0.75 x (fills - base) tokens a fresh start saves, in bytes
        let (fills, base) = (60_000u64, 13_000u64);
        assert!(record_cap(fills, Some(base)) as u64 / 4 * 2 <= (fills - base) * 3 / 4);
    }

    #[test]
    fn a_fresh_start_of_a_thread_in_use_carries_the_older_replies_whole_as_the_cap_allows() {
        for (mode, fills, base) in [(context_policy::Mode::Static, 70_000, None),
                                    (context_policy::Mode::Adaptive, 56_000, Some(13_000))] {
            let (mut s, agent) = ctx_session("codex", fills);
            s.context_mode = mode;
            if let Some(base) = base {
                s.bases.insert(agent, VecDeque::from([base]));
            }
            fs::write(s.jobs[0].dir.join("reply.md"), two_paragraphs("old", 2_000)).unwrap();
            ctx_turn(&mut s, agent, Status::Done, 5);
            s.jobs[0].read = vec!["src/old.rs".into()];
            let id = s.new_job(Kind::Agent { agent, write: false }, "fix the failing tests").unwrap();
            let mut resume = s.threads.get(&agent).cloned();
            let (text, note) = s.handoff(id, agent, &mut resume, "fix the failing tests").unwrap();
            assert!(resume.is_none() && note.starts_with("handoff: codex starts fresh"), "{mode:?}: {note}");
            assert!(text.contains("old-LAST-PARAGRAPH"), "{mode:?}: the older reply is whole: {text}");
            assert!(text.contains("files read (newest first;") && text.contains("src/old.rs"), "{mode:?}: {text}");
            // a cap of 0 (a brand-new conversation) gets the lean record
            let (mut s, agent) = ctx_session("codex", fills);
            fs::write(s.jobs[0].dir.join("reply.md"), two_paragraphs("old", 2_000)).unwrap();
            ctx_turn(&mut s, agent, Status::Done, 5);
            s.threads.remove(&agent);
            let id = s.new_job(Kind::Agent { agent, write: false }, "fix the failing tests").unwrap();
            let mut resume = None;
            let text = s.handoff(id, agent, &mut resume, "fix the failing tests").unwrap().0;
            assert!(text.contains("old first paragraph.") && !text.contains("old-LAST-PARAGRAPH"), "{mode:?}: {text}");
        }
        // a restart hands on the lean record and where the uncut one is (Ringo review: a rich one would cost the
        // next shell's oldest carried turns their room)
        let (mut s, agent) = ctx_session("codex", 50_000);
        fs::write(s.jobs[0].dir.join("reply.md"), two_paragraphs("old", 2_000)).unwrap();
        ctx_turn(&mut s, agent, Status::Done, 5);
        let records = s.carry_record();
        assert!(records.iter().any(|(name, fills, record)| name == "codex" && *fills == 50_000
            && record.contains("old first paragraph.") && !record.contains("old-LAST-PARAGRAPH")
            && record.contains("(these turns uncut: ")), "{records:?}");
    }

    #[test]
    fn a_rich_record_never_undoes_the_fresh_start_the_lean_one_decided() {
        // Review finding: a record filled to the cap, plus the text around it, read as over half the saving, so the
        // cost model kept a conversation the lean record would have replaced. Now the decision is on the lean record
        // and the detail is added after it, within the cap less the text around the record.
        // With 2 calls a turn, warm, the margin binds too (Ringo review): 2 x saving must stay >= 32k.
        for calls in [5u32, 2] {
        let (fills, base) = (48_000u64, 13_000u64);
        let (mut s, agent) = ctx_session("codex", fills);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([base]));
        s.jobs[0].calls = calls;
        for n in 0..30 {
            let id = ctx_turn(&mut s, agent, Status::Done, calls);
            fs::write(s.jobs[id - 1].dir.join("reply.md"), two_paragraphs(&format!("t{n}"), 3_000)).unwrap();
        }
        let id = s.new_job(Kind::Agent { agent, write: false }, "fix the failing tests").unwrap();
        let mut resume = s.threads.get(&agent).cloned();
        let (text, note) = s.handoff(id, agent, &mut resume, "fix the failing tests").unwrap();
        let log: Vec<String> = s.jobs[id - 1].log.iter().cloned().collect();
        assert!(resume.is_none() && note.starts_with("handoff: codex starts fresh"), "{note} {log:?}");
        assert!(log.iter().any(|l| l.starts_with("ctx codex: fresh")), "{log:?}");
        assert!(text.contains("t29-LAST-PARAGRAPH"), "the newest older reply is whole");
        assert!(!text.contains("t0-LAST-PARAGRAPH"), "the cap stops the detail before the oldest");
        // everything sent but the owner's message stays within the cap, so the decision holds on the rich size too
        let sent = text.len() - "The owner's new message:\nfix the failing tests".len();
        assert!(sent <= record_cap(fills, Some(base)), "{sent} bytes over {}", record_cap(fills, Some(base)));
        let room = context_policy::record_room(fills, base, Some(calls as u64), false);
        assert!(sent as u64 / 4 <= room, "calls {calls}: {} tokens over a room of {room}", sent / 4);
        let rich = context_policy::Signals { fills, base: Some(base), record: Some(sent as u64 / 4), calls: Some(calls as u64),
            idle_s: Some(0), ttl_s: context_policy::CODEX_TTL_S, previous_ended_badly: false, continuation: false,
            fresh_last_turn: false };
        assert_eq!(context_policy::decide(&rich, 48_000).action, context_policy::Action::Fresh, "calls {calls}: the text sent is fresh by the same rule");
        }
    }

    #[test]
    fn a_record_is_cut_by_what_it_leaves_out_not_by_its_files_read_lines() {
        // Ringo reviews: the files-read line (inline) and the per-turn read lines (whole) made every record look cut,
        // so a complete one carried the pointer and agy would read the same turns again; matching line texts instead
        // missed a dropped files-read line, a carried record's own line, and reply lines that begin "read: ".
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        reply_turns(&mut s, &["one".into(), "two".into()]);
        s.jobs[0].read = vec!["a.rs".into()];
        s.jobs[1].read = vec!["b.rs".into()];
        let turns = s.lane(agent, 3);
        let (inline, whole, complete) = s.record_full(&turns, 0);
        assert!(inline.contains(READ_LINE_PREFIX) && whole.contains("read: a.rs"));
        assert!(complete, "{inline}\n---\n{whole}");
        // with the change shown after the files read, too
        fs::write(s.repo.join("x.txt"), "old\n").unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let before = git(&s.repo, &["write-tree"]).unwrap();
        fs::write(s.repo.join("x.txt"), "new\n").unwrap();
        git(&s.repo, &["add", "-A"]).unwrap();
        let after = git(&s.repo, &["write-tree"]).unwrap();
        (s.jobs[1].before, s.jobs[1].after, s.jobs[1].changed) = (Some(before), Some(after), vec!["x.txt".into()]);
        let (inline, _, complete) = s.record_full(&s.lane(agent, 3), 0);
        assert!(inline.contains("The change these turns made") && inline.contains(READ_LINE_PREFIX) && complete);
        // a record carried over a restart, its own files-read line and all, is complete when nothing new is cut
        s.before.insert(agent, inline.clone());
        let (_, _, complete) = s.fresh_record_full(agent, &s.lane(agent, 3), 0);
        assert!(complete, "the carried record's files-read line is no cut");
        s.before.remove(&agent);
        // a cut files-read line is a cut
        s.jobs[0].read = (0..100).map(|n| format!("f{n}.rs")).collect();
        assert!(!s.record_full(&s.lane(agent, 3), 0).2);
        s.jobs[0].read = vec!["a.rs".into()];
        // so is a brief older reply, even one whose cut paragraph begins "read: "
        fs::write(s.jobs[0].dir.join("reply.md"), "Done.\n\nread: the config first, then the rest").unwrap();
        assert!(!s.record_full(&s.lane(agent, 3), 0).2);
        // Ringo review 3: a newest turn whose every step is shown is whole, failed step in the middle or not
        fs::write(s.jobs[0].dir.join("reply.md"), "one").unwrap();
        s.jobs[1].steps = vec!["$ cargo build".into(), format!("{}$ cargo test · exit 101", parse::FAILED), "$ cargo test".into()];
        let (inline, _, complete) = s.record_full(&s.lane(agent, 3), 0);
        assert!(complete && inline.contains(&format!("$ cargo build | {}$ cargo test · exit 101 | $ cargo test", parse::FAILED)), "{inline}");
        // and a file read past the 20 an edited line names is told as read (and must be, for the record to be whole)
        s.jobs[1].steps.clear();
        (s.jobs[1].before, s.jobs[1].after, s.jobs[1].changed) = (None, None, Vec::new());
        s.jobs[1].touched = (1..=25).map(|n| format!("f{n:02}.rs")).collect();
        s.jobs[1].read = (21..=25).map(|n| format!("f{n:02}.rs")).chain(["f01.rs".to_string()]).collect();
        let (inline, _, complete) = s.record_full(&s.lane(agent, 3), 0);
        let line = inline.lines().find(|l| l.starts_with(READ_LINE_PREFIX)).unwrap();
        assert!(line.contains("f25.rs") && line.contains("f21.rs") && !line.contains("f01.rs"), "{line}");
        assert!(complete, "every file read is named, on the edited line or the files-read line");
        s.jobs[1].touched.clear();
        s.jobs[1].read = vec!["b.rs".into()];
        // and a files-read line left out for lack of room
        let long = format!("src/{}.rs", "d".repeat(200));
        let mut seen_dropped = false;
        let mut s = record_session(); // one session: many made at once can run out of folder names
        reply_turns(&mut s, &vec![String::new(); 10]);
        for j in s.jobs.iter_mut() {
            j.talk = true;
            j.read = vec![long.clone()];
        }
        for k in (5_850..5_990).step_by(3) {
            for j in &s.jobs {
                fs::write(j.dir.join("reply.md"), "x".repeat(k)).unwrap();
            }
            let (inline, _, complete) = s.record_full(&s.lane(agent, 11), 0);
            if !inline.contains("#1 · the owner") {
                break;
            }
            if !inline.contains(READ_LINE_PREFIX) {
                seen_dropped = true;
                assert!(!complete, "{k}: a dropped files-read line is a cut");
            } else {
                assert!(complete, "{k}");
            }
        }
        assert!(seen_dropped);
    }

    #[test]
    fn a_rich_record_keeps_every_turn_from_before_a_restart_the_lean_one_kept() {
        // Review finding: the rich tier took the room the turns before a /restart had (the owner's earlier words).
        let mut s = record_session();
        let agent = Agent::from_name("codex").unwrap();
        let replies: Vec<String> = (0..20).map(|n| two_paragraphs(&format!("r{n}"), 3_000)).collect();
        reply_turns(&mut s, &replies);
        s.before.insert(agent, (0..3_000).map(|n| format!("PRE-RESTART owner line {n}\n")).collect());
        let turns = s.lane(agent, 21);
        let count = |text: &str| text.matches("PRE-RESTART").count();
        let (lean, _) = s.fresh_record(agent, &turns, 0);
        for cap in [record_cap(50_000, None), record_cap(100_000, Some(13_000)), usize::MAX] {
            let (rich, _) = s.fresh_record(agent, &turns, cap);
            assert_eq!(count(&rich), count(&lean), "cap {cap}");
            assert!(rich.len() <= RECORD_BYTES, "cap {cap}: {} bytes", rich.len());
        }
        assert!(count(&lean) > 0);
        // with room to spare after the carried turns, the detail is still added
        s.before.insert(agent, "PRE-RESTART a short earlier turn\n".into());
        let turns = s.lane(agent, 21);
        let (rich, _) = s.fresh_record(agent, &turns, record_cap(100_000, Some(13_000)));
        assert!(count(&rich) == 1 && rich.contains("r18-LAST-PARAGRAPH"), "{rich}");
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

    // --- the boundary context policy (see context_policy.rs) ---

    /// A session with `agent`'s one finished read turn behind it, a thread to resume and `fills` in its window.
    fn ctx_session(name: &str, fills: u64) -> (Session, Agent) {
        let mut s = record_session();
        s.context_mode = context_policy::Mode::Static;
        s.codex_home = s.dir.join("codex-home");
        fs::create_dir_all(&s.codex_home).unwrap();
        let agent = Agent::from_name(name).unwrap();
        ctx_turn(&mut s, agent, Status::Done, 5);
        s.context.insert(agent, fills);
        s.threads.insert(agent, "thread-1".into());
        (s, agent)
    }

    fn ctx_turn(s: &mut Session, agent: Agent, status: Status, calls: u32) -> usize {
        let id = s.new_job(Kind::Agent { agent, write: false }, "the earlier turn").unwrap();
        fs::write(s.jobs[id - 1].dir.join("reply.md"), "an earlier reply").unwrap();
        let job = &mut s.jobs[id - 1];
        job.status = status;
        job.calls = calls;
        job.elapsed = Some(Duration::from_secs(1));
        id
    }

    /// Runs the handoff of a new turn: whether the thread started fresh, and the job's log.
    fn ctx_next(s: &mut Session, agent: Agent, prompt: &str) -> (bool, String) {
        let jobs = s.jobs.len();
        let id = s.new_job(Kind::Agent { agent, write: false }, prompt).unwrap();
        let mut resume = s.threads.get(&agent).cloned();
        let _ = s.handoff(id, agent, &mut resume, prompt);
        assert_eq!(s.jobs.len(), jobs + 1, "the decision starts no job and calls no model");
        let job = &mut s.jobs[id - 1]; // it ends, a turn behind the next
        (job.status, job.calls, job.elapsed) = (Status::Done, 5, Some(Duration::from_secs(1)));
        (resume.is_none(), s.jobs[id - 1].log.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    #[test]
    fn codex_in_the_default_mode_resumes_below_handoff_at_as_before_the_cost_model() {
        // 50k is past the 48k an earlier build used, and under handoff_at: the thread is resumed
        let (mut s, agent) = ctx_session("codex", 50_000);
        assert_eq!(s.static_threshold(agent), HANDOFF_AT);
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").0);
        // the owner's own in-turn limit does not move it either
        let (mut s, agent) = ctx_session("codex", 35_000);
        fs::write(s.codex_home.join("config.toml"), "model_auto_compact_token_limit = 40_000\n").unwrap();
        assert_eq!(s.static_threshold(agent), HANDOFF_AT);
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").0);
        // past handoff_at it starts fresh, as it always did
        let (mut s, agent) = ctx_session("codex", HANDOFF_AT + 1);
        assert!(ctx_next(&mut s, agent, "fix the failing tests").0);
    }

    #[test]
    fn a_pinned_policy_handoff_is_used_exactly() {
        let (mut s, agent) = ctx_session("codex", 50_000);
        s.set_turn_policy(turn_policy::TurnPolicy::from_json(&serde_json::json!({"handoff_at": 48000})).unwrap());
        assert!(s.handoff_pinned);
        assert_eq!(s.static_threshold(agent), 48_000);
        s.context.insert(agent, 40_000);
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").0, "40k is under the owner's 48k");
        s.context.insert(agent, 50_000);
        assert!(ctx_next(&mut s, agent, "fix the failing tests").0, "50k is past the owner's 48k");
        // a policy without handoff_at pins nothing
        s.set_turn_policy(turn_policy::TurnPolicy::from_json(&serde_json::json!({"instruction": "x"})).unwrap());
        assert!(!s.handoff_pinned);
        assert_eq!(s.static_threshold(agent), HANDOFF_AT);
    }

    #[test]
    fn claude_and_agy_keep_the_static_64k_in_the_default_mode() {
        let (mut s, agent) = ctx_session("claude", 50_000);
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").0);
        s.context.insert(agent, 64_001);
        assert!(ctx_next(&mut s, agent, "fix the failing tests").0);
        for mode in [context_policy::Mode::Static, context_policy::Mode::Adaptive] {
            let (mut s, agent) = ctx_session("agy", 0); // agy never reports its context
            s.context_mode = mode;
            s.context.remove(&agent);
            let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
            assert!(!fresh && !log.contains("ctx agy"), "{log}");
        }
    }

    #[test]
    fn adaptive_mode_decides_by_the_cost_model_and_logs_the_inputs() {
        let (mut s, agent) = ctx_session("codex", 56_000);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && log.contains("ctx codex: fresh — fills 56.0k base 13.0k record "), "{log}");
        assert!(log.contains("calls~5") && log.contains("save~") && log.contains("idle 0m (warm)"), "{log}");
        // a short turn behind a small conversation resumes, and the line says why
        let (mut s, agent) = ctx_session("codex", 30_000);
        s.jobs[0].calls = 1;
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(!fresh && log.contains("ctx codex: resume — fills 30.0k base 13.0k"), "{log}");
        // adaptive acts below the static threshold too: 40k, a long turn behind it, cold
        let (mut s, agent) = ctx_session("codex", 40_000);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        if age(&mut s, 3 * 3600) { // a machine up for less than that cannot hold an Instant so old (Windows)
            let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
            assert!(fresh && log.contains("(cold)"), "{log}");
        }
        // without a measured base it is the static rule, and says so
        let (mut s, agent) = ctx_session("codex", 70_000);
        s.context_mode = context_policy::Mode::Adaptive;
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && log.contains("ctx codex: fresh — no base yet: static 64k"), "{log}");
        // Claude too
        let (mut s, agent) = ctx_session("claude", 56_000);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && log.contains("ctx claude: fresh"), "{log}");
    }

    #[test]
    fn adaptive_mode_keeps_the_guards() {
        for (status, prompt, why) in [(Status::Cancelled, "fix the failing tests", "previous turn stopped or failed"),
                                      (Status::Failed("x".into()), "fix the failing tests", "previous turn stopped or failed"),
                                      (Status::Done, "continue", "owner continues")] {
            let (mut s, agent) = ctx_session("codex", 120_000);
            s.context_mode = context_policy::Mode::Adaptive;
            s.bases.insert(agent, VecDeque::from([13_000]));
            s.jobs[0].status = status;
            let (fresh, log) = ctx_next(&mut s, agent, prompt);
            assert!(!fresh && log.contains(&format!("ctx codex: resume — guard: {why}")), "{log}");
        }
        // a fresh start does not follow a fresh start
        let (mut s, agent) = ctx_session("codex", 120_000);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        assert!(ctx_next(&mut s, agent, "fix the failing tests").0);
        let id = s.jobs.len();
        s.jobs[id - 1].status = Status::Done;
        s.jobs[id - 1].elapsed = Some(Duration::from_secs(1));
        s.threads.insert(agent, "thread-2".into());
        s.context.insert(agent, 120_000);
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(!fresh && log.contains("guard: fresh start last turn"), "{log}");
    }

    #[test]
    fn a_pinned_handoff_overrides_adaptive_mode_exactly() {
        let (mut s, agent) = ctx_session("claude", 90_000);
        s.context_mode = context_policy::Mode::Adaptive;
        s.bases.insert(agent, VecDeque::from([13_000]));
        s.handoff_at = 96_000;
        s.handoff_pinned = true;
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(!fresh && log.contains("ctx claude: resume — owner handoff_at 96k"), "{log}");
    }

    #[test]
    fn a_pinned_handoff_beats_off_and_a_policy_naming_adaptive_uses_the_cost_model() {
        // RINGO_CONTEXT=off does not override the owner's pinned number
        let (mut s, agent) = ctx_session("codex", 70_000);
        s.context_mode = context_policy::Mode::Off;
        s.handoff_pinned = true;
        assert!(ctx_next(&mut s, agent, "fix the failing tests").0, "fills 70k > the pinned 64k");
        // an evaluation arm's policy names adaptive with its own handoff_at: the cost model decides
        let (mut s, agent) = ctx_session("codex", 56_000);
        s.set_turn_policy(turn_policy::TurnPolicy::from_json(&serde_json::json!({"handoff_at": 64000, "context": "adaptive"})).unwrap());
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && log.contains("ctx codex: fresh — fills 56.0k base 13.0k"), "{log}");
        // without a base, the policy's number is the fallback
        let (mut s, agent) = ctx_session("codex", 56_000);
        s.set_turn_policy(turn_policy::TurnPolicy::from_json(&serde_json::json!({"handoff_at": 64000, "context": "adaptive"})).unwrap());
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(!fresh && log.contains("no base yet: static 64k"), "{log}");
        assert!(turn_policy::TurnPolicy::from_json(&serde_json::json!({"context": "sometimes"})).is_err());
    }

    #[test]
    fn off_never_starts_an_existing_thread_fresh_but_a_new_one_gets_the_record() {
        let (mut s, agent) = ctx_session("codex", 1_000_000);
        s.context_mode = context_policy::Mode::Off;
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").0);
        s.threads.remove(&agent);
        let id = s.new_job(Kind::Agent { agent, write: false }, "next").unwrap();
        let mut resume = None;
        let told = s.handoff(id, agent, &mut resume, "next").unwrap().1;
        assert!(told.contains("told codex the conversation so far"), "{told}");
    }

    #[test]
    fn an_unknown_mode_is_the_default_with_one_log_line() {
        let (mut s, agent) = ctx_session("codex", 70_000);
        s.context_note = Some("sometimes".into());
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && log.contains("RINGO_CONTEXT=sometimes"), "{log}");
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").1.contains("RINGO_CONTEXT"));
    }

    #[test]
    fn the_shadow_line_appears_only_when_the_verdicts_differ() {
        // static resumes at 50k (under 64k); five calls per turn, warm, the cost model says fresh: differ
        let (mut s, agent) = ctx_session("codex", 50_000);
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(!fresh, "the shadow never changes what the static rule did");
        assert!(log.contains("ctx codex (shadow): adaptive would fresh — fills 50.0k base 13.0k"), "{log}");
        // both say fresh
        let (mut s, agent) = ctx_session("codex", 70_000);
        s.bases.insert(agent, VecDeque::from([13_000]));
        let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
        assert!(fresh && !log.contains("shadow"), "{log}");
        // both say resume
        let (mut s, agent) = ctx_session("codex", 30_000);
        s.jobs[0].calls = 1;
        s.bases.insert(agent, VecDeque::from([13_000]));
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").1.contains("shadow"));
        // static resumes at 40k, adaptive would start fresh (cold, long turns): differ
        let (mut s, agent) = ctx_session("codex", 40_000);
        s.bases.insert(agent, VecDeque::from([13_000]));
        if age(&mut s, 3 * 3600) {
            let (fresh, log) = ctx_next(&mut s, agent, "fix the failing tests");
            assert!(!fresh && log.contains("(shadow): adaptive would fresh"), "{log}");
        }
        // a pinned handoff_at is the owner's number: no shadow
        let (mut s, agent) = ctx_session("codex", 40_000);
        s.bases.insert(agent, VecDeque::from([13_000]));
        age(&mut s, 3 * 3600);
        s.handoff_pinned = true;
        assert!(!ctx_next(&mut s, agent, "fix the failing tests").1.contains("shadow"));
    }

    /// Makes the first job's turn `secs` older, when this machine's clock can hold such an Instant.
    fn age(s: &mut Session, secs: u64) -> bool {
        match s.jobs[0].started.checked_sub(Duration::from_secs(secs)) {
            Some(t) => { s.jobs[0].started = t; true }
            None => false,
        }
    }

    #[test]
    fn a_new_conversations_first_context_report_measures_the_base() {
        let (mut s, agent) = ctx_session("codex", 0);
        s.threads.remove(&agent);
        let id = s.new_job(Kind::Agent { agent, write: false }, "hello").unwrap();
        s.jobs[id - 1].fresh_thread = true;
        s.jobs[id - 1].status = Status::Done;
        s.jobs[id - 1].sent_bytes = 8_000; // 2k tokens of what was sent
        s.tx.send(Event::Note(id, Note::Context(15_000))).unwrap();
        s.tx.send(Event::Note(id, Note::Context(18_000))).unwrap(); // later calls do not measure
        s.pump();
        assert_eq!(s.bases.get(&agent).unwrap(), &VecDeque::from([13_000]));
        assert_eq!(s.context.get(&agent), Some(&18_000));
        // a resumed turn does not measure
        let id = s.new_job(Kind::Agent { agent, write: false }, "again").unwrap();
        s.jobs[id - 1].status = Status::Done;
        s.tx.send(Event::Note(id, Note::Context(30_000))).unwrap();
        s.pump();
        assert_eq!(s.bases.get(&agent).unwrap().len(), 1);
        // only the last three are kept, and the floor is 1k
        for n in [14_000u64, 15_000, 16_000, 500] {
            let id = s.new_job(Kind::Agent { agent, write: false }, "fresh").unwrap();
            s.jobs[id - 1].fresh_thread = true;
            s.jobs[id - 1].status = Status::Done;
            s.tx.send(Event::Note(id, Note::Context(n))).unwrap();
            s.pump();
        }
        assert_eq!(s.bases.get(&agent).unwrap(), &VecDeque::from([15_000, 16_000, 1_000]));
    }

    // --- the ledger fields of usage.jsonl ---

    /// Like `ctx_session` but with a state folder of its own, so a finished job's usage.jsonl stays in the test.
    fn ledger_session(name: &str, fills: u64) -> (Session, Agent) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("ringo-ledger-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        let repo = path.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        let mut s = Session::with_state(&repo, &path.join("state")).unwrap();
        s.context_mode = context_policy::Mode::Static;
        s.codex_home = s.dir.join("codex-home");
        fs::create_dir_all(&s.codex_home).unwrap();
        let agent = Agent::from_name(name).unwrap();
        ctx_turn(&mut s, agent, Status::Done, 5);
        s.context.insert(agent, fills);
        s.threads.insert(agent, "thread-1".into());
        (s, agent)
    }

    fn last_usage_row(s: &Session) -> serde_json::Value {
        let text = fs::read_to_string(s.root.join("usage.jsonl")).unwrap();
        serde_json::from_str(text.lines().last().unwrap()).unwrap()
    }

    #[test]
    fn concurrent_usage_rows_never_interleave_and_unreadable_ones_are_counted() {
        let (s, _) = ledger_session("codex", 0);
        let path = s.root.join("usage.jsonl");
        let _ = fs::remove_file(&path);
        let writers: Vec<_> = (0..8).map(|w| {
            let path = path.clone();
            thread::spawn(move || for n in 0..50 {
                // a long row (past one pipe-sized write) is the one a plain writeln! could split
                append_row(&path, &serde_json::json!({"at": n, "agent": format!("w{w}"), "pad": "x".repeat(6000)}));
            })
        }).collect();
        for w in writers {
            w.join().unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 400);
        assert!(text.ends_with('\n'));
        assert_eq!(s.unreadable_usage_rows(), 0, "every row is whole");
        // a torn line and a blank line: the torn one is counted, the blank one is not
        fs::write(&path, format!("{text}{{\"at\": 1, \"agent\"\n\n[1]\n")).unwrap();
        assert_eq!(s.unreadable_usage_rows(), 2);
    }

    #[test]
    fn attribution_matches_decompose_definitions() {
        assert_eq!(attribute(&[12_000, 15_000, 9_000], 10_000), Some((29_000, 4_000, 3_000)));
        // a first call below the base: fixed and growth share the difference (decompose.py's surplus)
        assert_eq!(attribute(&[8_000, 20_000], 10_000), Some((18_000, 0, 12_000)));
        assert_eq!(attribute(&[], 1), None);
    }

    #[test]
    fn a_turn_the_computer_slept_through_says_so_in_its_row_and_log() {
        let (mut s, agent) = ledger_session("codex", 0);
        let tokens = Tokens { input: 100, cache_read: 0, cache_write: 0, output: 10 };
        let id = s.new_job(Kind::Agent { agent, write: false }, "slept").unwrap();
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = tokens;
        // the wall clock ran 1,900 s while the monotonic one ran about none
        s.jobs[id - 1].wall_start = SystemTime::now() - Duration::from_secs(1900);
        s.end_with(id, Status::Done, "done".into());
        let row = last_usage_row(&s);
        assert_eq!(row["host_slept_s"].as_u64().map(|n| n / 60), Some(31));
        assert!(s.jobs[id - 1].log.iter().any(|l| l.contains("the computer slept for ~32 min")), "{:?}", s.jobs[id - 1].log);
        // an ordinary turn has no such field
        let id = s.new_job(Kind::Agent { agent, write: false }, "awake").unwrap();
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = tokens;
        s.end_with(id, Status::Done, "done".into());
        assert!(last_usage_row(&s).get("host_slept_s").is_none());
    }

    #[test]
    fn a_finished_turn_records_its_context_split() {
        let (mut s, agent) = ledger_session("codex", 0);
        s.bases.insert(agent, VecDeque::from([10_000]));
        let tokens = Tokens { input: 100, cache_read: 50, cache_write: 0, output: 10 };
        let id = s.new_job(Kind::Agent { agent, write: false }, "ledger").unwrap();
        for n in [12_000u64, 15_000, 9_000] {
            s.tx.send(Event::Note(id, Note::Call)).unwrap();
            s.tx.send(Event::Note(id, Note::CallInput(n))).unwrap();
        }
        s.pump();
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = tokens;
        s.end_with(id, Status::Done, "done".into());
        let row = last_usage_row(&s);
        assert_eq!((row["ctx_base"].as_u64(), row["ctx_fixed"].as_u64(), row["ctx_carry"].as_u64(), row["ctx_growth"].as_u64()),
                   (Some(10_000), Some(29_000), Some(4_000), Some(3_000)));
        assert_eq!((row["calls"].as_u64(), row["surface"].as_str(), row["billing"].as_str()), (Some(3), Some("cli:codex"), Some("subscription")));
        assert_eq!(row["input"].as_u64(), Some(100));
        // no measured base: no context fields, and the calls are still there
        let (mut s, agent) = ledger_session("codex", 0);
        let id = s.new_job(Kind::Agent { agent, write: false }, "ledger").unwrap();
        for n in [12_000u64, 15_000] {
            s.tx.send(Event::Note(id, Note::Call)).unwrap();
            s.tx.send(Event::Note(id, Note::CallInput(n))).unwrap();
        }
        s.pump();
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = tokens;
        s.end_with(id, Status::Done, "done".into());
        let row = last_usage_row(&s);
        assert!(["ctx_base", "ctx_fixed", "ctx_carry", "ctx_growth"].iter().all(|k| row.get(*k).is_none()), "{row}");
        assert_eq!(row["calls"].as_u64(), Some(2));
    }

    #[test]
    fn a_fresh_start_records_its_record_sizes() {
        let (mut s, agent) = ledger_session("codex", HANDOFF_AT + 1);
        let jobs = s.jobs.len();
        let id = s.new_job(Kind::Agent { agent, write: false }, "fix the failing tests").unwrap();
        let mut resume = s.threads.get(&agent).cloned();
        let sent = s.handoff(id, agent, &mut resume, "fix the failing tests").map(|(text, _)| text.len() as u64);
        assert!(resume.is_none() && s.jobs[id - 1].started_fresh, "past handoff_at the turn starts fresh");
        assert_eq!(s.jobs.len(), jobs + 1);
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = Tokens { input: 10, cache_read: 0, cache_write: 0, output: 1 };
        s.end_with(id, Status::Done, "done".into());
        let row = last_usage_row(&s);
        let whole = fs::metadata(s.jobs[id - 1].dir.join("record.md")).unwrap().len();
        assert_eq!(row["record_whole_bytes"].as_u64(), Some(whole));
        let inline = row["record_inline_bytes"].as_u64().unwrap();
        assert!(inline > 0 && inline <= sent.unwrap() - "fix the failing tests".len() as u64, "{inline} of {sent:?}");
        // a resumed turn has neither
        let (mut s, agent) = ledger_session("codex", 50_000);
        let id = s.new_job(Kind::Agent { agent, write: false }, "again").unwrap();
        let mut resume = s.threads.get(&agent).cloned();
        let _ = s.handoff(id, agent, &mut resume, "again");
        assert!(resume.is_some());
        s.jobs[id - 1].counted = true;
        s.jobs[id - 1].tokens = Tokens { input: 10, cache_read: 0, cache_write: 0, output: 1 };
        s.end_with(id, Status::Done, "done".into());
        let row = last_usage_row(&s);
        assert!(row.get("record_inline_bytes").is_none() && row.get("record_whole_bytes").is_none(), "{row}");
    }
}

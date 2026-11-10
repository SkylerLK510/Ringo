//! The `ringo` shell: a full-screen app, as Codex's is. A plain line goes to the agent you are talking to, which works
//! in this folder (each turn's change can be undone) and asks before it runs a command. The conversation fills the
//! screen above the input; the mouse wheel and PgUp/PgDn scroll it, and a drag selects its text alone and copies it.
//! Below it sit the running jobs, the lines queued behind the current turn, a permission request or the input box,
//! and a status line. The screen redraws only when something changed (an event, a key), and for the spinner (every
//! SPIN_MS) while jobs run.

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
                                EnableMouseCapture, Event as Term, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
                                KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
                                PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
                                   EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout, Rect};
use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::{DefaultTerminal, Frame, Terminal};
use ringo_cli::{Agent, Decision, Kind, Mode, Said, Session, Status, Tier};
use std::collections::VecDeque;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

/// The user guide, printed by /usrman as markdown: the same file as docs/USER-GUIDE.md, so the two never differ.
const MANUAL: &str = include_str!("../../docs/USER-GUIDE.md");

/// What the menu offers as a line starts with `/`: each command as it reads, and what it does.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "the guide (also /usrman)"),
    ("/undo", "take back the last change an agent made here"),
    ("/copy", "copy the last answer"),
    ("/clear", "start a new conversation"),
    ("/compact", "shrink a long conversation so later turns cost less"),
    ("/separate", "a conversation of its own for each agent, so none is told what the others do"),
    ("/together", "the agents back in one conversation, still working side by side"),
    ("/discuss TOPIC", "the agents talk it over among themselves, in turn, while you read (alone: the last turn)"),
    ("/ask TEXT", "a quick answer from one cheap call, without your files"),
    ("/mode ask|auto|read", "what agents may do without asking"),
    ("/tier quick|standard|deep|ultra", "how hard agents think; ultra also reviews each change and fixes what it finds"),
    ("/model", "which model each agent runs on, and the models your plans offer"),
    ("/model AGENT NAME", "that agent's turns on that model (default: its own again)"),
    ("/agent", "your own agents: a specialist made from a file of instructions"),
    ("/agent add NAME FILE", "make one, on claude (or: codex after FILE; skills: your Claude Code skills too)"),
    ("/agent remove NAME", "take one of your own agents away"),
    ("/usage", "what you have used, and how much of your plans is left"),
    ("/auto", "auto mode on"),
    ("/review #N", "review job N's change"),
    ("/review BASE [HEAD]", "review BASE...HEAD of this repository"),
    ("/design TEXT", "the one-call design pipeline"),
    ("/research TOPIC", "live web search, every quoted source checked"),
    ("/work SPEC.json", "the checked work loop on a task spec"),
    ("/show N", "print job N's log"),
    ("/diff N", "job N's change"),
    ("/apply N", "put job N's change in your tree"),
    ("/drop N", "remove job N's worktree"),
    ("/kill N", "stop job N"),
    ("/setup", "add an agent and sign in to it"),
    ("/update", "update the agents found out of date, and check the pipelines' release"),
    ("/update approve", "move the pipelines to the release /update checked"),
    ("/restart", "start Ringo again (an update applies), the conversations going on"),
    ("/theme NAME", "colours"),
    ("/backend SPEC", "backend for the pipelines"),
    ("/quit", "leave (or Ctrl-C twice)"),
];

/// The menu under the input for the word before the cursor: commands while the line's first word starts with `/`,
/// agents for a word that starts with `@` (`agents`: each set-up agent's name, and whether it has a read-only mode).
/// Each entry: what it completes to, how it reads, what it does.
fn menu(before: &str, agents: &[(&str, bool)]) -> Vec<(String, String, String)> {
    let word = before.rsplit([' ', '\n']).next().unwrap_or("");
    if word.starts_with('/') && word.len() == before.len() {
        return COMMANDS.iter()
            .filter(|(c, _)| c.split(' ').next().is_some_and(|head| head.starts_with(word)))
            .map(|(c, what)| {
                let head = c.split(' ').next().unwrap_or(c);
                (format!("{head}{}", if c.contains(' ') { " " } else { "" }), c.to_string(), what.to_string())
            })
            .collect();
    }
    if !word.starts_with('@') {
        return Vec::new();
    }
    let mut entries = Vec::new();
    for &(name, here) in agents {
        let tags = [(format!("@{name}"), format!("talk to {name}: it works in this folder")),
                    (format!("@{name}!"), format!("{name} works in its own copy; /apply keeps its change"))];
        for (tag, what) in tags {
            if tag.starts_with(word) && (here || tag.ends_with('!')) {
                entries.push((format!("{tag} "), tag, what));
            }
        }
    }
    if "@all".starts_with(word) {
        entries.push(("@all ".into(), "@all".into(), "every agent, each in its own copy, to compare".into()));
    }
    entries
}

/// Whether the terminal reports Shift-Enter apart from Enter (the kitty keyboard protocol is on).
static SHIFT_ENTER: AtomicBool = AtomicBool::new(false);

/// Set when the shell is told to end from outside: `kill` (SIGTERM) or its terminal closing (SIGHUP). The loop then
/// ends as /quit does: running turns are stopped and counted, the conversation is kept and the terminal is put back.
/// Before, the shell died where it stood. Each agent is in a process group of its own, so a step it was in the middle
/// of ran on unseen (5 s of a 6 s step, in a pty with a stand-in agent), the turn never reached the usage record,
/// nothing was kept for `ringo --resume`, and a killed shell left the terminal in raw mode on the alternate screen.
static ENDED: AtomicBool = AtomicBool::new(false);

/// For a closed terminal (`watch_the_end` sets both; -1 until then): the reading end of a pipe that stays empty, and
/// the writing end of the pipe that wakes the watchdog.
#[cfg(unix)]
static IDLE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// How long the shell may take to end once it is told to (its jobs are stopped within a few seconds, then it leaves).
#[cfg(unix)]
const END_WITHIN: Duration = Duration::from_secs(20);

/// Takes SIGTERM and SIGHUP for the shell's loop (`on`), or gives them back their default: while an agent's sign-in
/// or an update has the plain terminal, the loop is not there to see `ENDED`, and a `kill` must still end the shell.
fn end_signals(on: bool) {
    #[cfg(unix)]
    {
        extern "C" fn ended(signal: libc::c_int) {
            ENDED.store(true, Ordering::SeqCst);
            let (idle, wake) = (IDLE.load(Ordering::SeqCst), WAKE.load(Ordering::SeqCst));
            // SAFETY: dup2 and write are async-signal-safe, on descriptors `watch_the_end` made.
            unsafe {
                // A closed terminal reads as ready and empty for ever, and the terminal library (crossterm 0.29) reads
                // it again without end: the loop would never come round to see ENDED. The empty pipe in its place
                // reads as "nothing yet", so the wait for a key ends at its own time limit.
                if signal == libc::SIGHUP && idle >= 0 {
                    libc::dup2(idle, libc::STDIN_FILENO);
                }
                if wake >= 0 {
                    libc::write(wake, [0u8].as_ptr().cast(), 1);
                }
            }
        }
        let action = if on { ended as extern "C" fn(libc::c_int) as libc::sighandler_t } else { libc::SIG_DFL };
        for signal in [libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: the handler stores to an atomic and makes async-signal-safe calls only.
            unsafe { libc::signal(signal, action) };
        }
    }
    #[cfg(not(unix))]
    let _ = on;
}

/// Makes what the signal handler needs when a terminal closes (see `ended`), and a watchdog: a shell told to end
/// that is still there after END_WITHIN leaves at once, so nothing can keep it turning behind a closed terminal.
#[cfg(unix)]
fn watch_the_end() {
    let pipe = || {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is the two ints pipe fills; fcntl sets a flag on the descriptors it just made (agents'
        // processes are not handed them).
        unsafe {
            (libc::pipe(fds.as_mut_ptr()) == 0).then(|| {
                fds.iter().for_each(|&fd| { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC); });
                (fds[0], fds[1])
            })
        }
    };
    let (Some((idle, _open)), Some((woken, wake))) = (pipe(), pipe()) else { return };
    // SAFETY: sets a flag on a descriptor made above. Its writing end stays open, so it never reads as ended.
    unsafe { libc::fcntl(idle, libc::F_SETFL, libc::O_NONBLOCK) };
    IDLE.store(idle, Ordering::SeqCst);
    WAKE.store(wake, Ordering::SeqCst);
    std::thread::spawn(move || {
        let mut byte = [0u8];
        // SAFETY: reads one byte into a one-byte buffer; blocks until the handler writes it.
        while unsafe { libc::read(woken, byte.as_mut_ptr().cast(), 1) } < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
        std::thread::sleep(END_WITHIN);
        std::process::exit(1);
    });
}

/// The owner's lines, kept across sessions, newest last (private, as the rest of Ringo's state is).
fn history_file() -> PathBuf {
    ringo_cli::state_root().join("shell-history.jsonl")
}

/// How many earlier lines Up reaches.
const HISTORY: usize = 1000;

/// The last HISTORY lines. The file is only ever appended to, so shells open at once never lose each other's lines;
/// ceiling: it grows by a few MB a year at heavy use, read whole at every start.
fn load_history() -> Vec<String> {
    let text = std::fs::read_to_string(history_file()).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let extra = lines.len().saturating_sub(HISTORY);
    lines.drain(..extra);
    lines
}

fn save_history(line: &str) {
    use std::io::Write;
    // a test's lines are not the owner's
    if cfg!(test) {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(history_file()) {
        let _ = writeln!(file, "{}", serde_json::json!(line));
    }
}

/// `/agent add NAME [FILE] [BASE] [skills]` and `/agent remove NAME`.
#[derive(Debug, PartialEq)]
pub enum OwnAgent {
    /// `file`: its instructions, or the name of an agent made for Claude Code (none: one called `name`); `base`: the
    /// agent it runs on (none: claude when it is set up); `skills`: the owner's Claude Code skills load in its turns.
    Add { name: String, file: Option<String>, base: Option<String>, skills: bool },
    Remove(String),
}

const AGENT_USAGE: &str = "/agent lists your own agents; /agent add NAME FILE makes one from a file of instructions (add \
                           codex to run it there, skills to load your Claude Code skills in its turns); /agent remove NAME";

#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// A plain line: for the agent the owner is talking to.
    Talk(String),
    Agents(Vec<(Agent, bool)>, String),
    /// `@codex` alone: plain lines go to that agent.
    Switch(Agent),
    /// `/ask TEXT`: one tool-free call.
    Ask(String),
    Undo,
    Copy,
    Clear,
    Compact,
    /// `/separate` (true): each agent a conversation of its own, at the same time; `/together` (false): one again.
    Separate(bool),
    /// `?`: the keys.
    Keys,
    Backend(String),
    Show(usize),
    Kill(usize),
    Diff(usize),
    Apply(usize),
    Drop(usize),
    ReviewJob(usize),
    ReviewRepo(String, String),
    Design(String),
    Research(String),
    Work(String),
    Help,
    Setup,
    Theme(Option<String>),
    /// `/update` (None) or `/update auto on|off`.
    Update(Option<bool>),
    /// `/update approve`: the pipelines move to the release `/update` checked.
    Approve,
    /// `/mode` (None: show it) or `/mode ask|auto|read`.
    Mode(Option<Mode>),
    /// `/auto`: auto mode on, or back to ask mode.
    Auto,
    /// `/tier` (None: show it) or `/tier quick|standard|deep|ultra`.
    Tier(Option<Tier>),
    /// `/model` (None: show who runs on what) or `/model AGENT NAME`.
    Model(Option<(String, String)>),
    /// `/agent` (None: the owner's own agents, and the Claude Code agent files found) or one made or taken away.
    Agent(Option<OwnAgent>),
    /// `/discuss [@AGENT ...] [TOPIC]`: those agents (none named: every one that works in this folder) talk the
    /// topic over (none: the last turn).
    Discuss(Vec<Agent>, String),
    Usage,
    /// `/restart`: this shell ends and the installed `ringo` takes over the conversations.
    Restart,
    Quit,
}

/// How many times each agent speaks in a talk (/discuss) at most, a line of the owner's aside: with `n` agents a talk
/// is at most `n * TALK_ROUNDS` turns.
const TALK_ROUNDS: usize = 2;

/// The agents' talk under way (/discuss): they take turns in the conversation they share, each answering what the
/// others said, until none has more to add or each has spoken TALK_ROUNDS times.
struct Talk {
    order: Vec<Agent>,
    /// Whose turn runs now (in `order`), and its job.
    at: usize,
    job: usize,
    /// Its turns so far, the running one included, and the most it may take: a line of the owner's adds a round.
    jobs: Vec<usize>,
    limit: usize,
    /// Replies in a row that added nothing.
    passes: usize,
    /// The owner's lines since the running turn began, which the next speaker hears.
    said: Vec<String>,
    started: Instant,
}

/// What a talk's first speaker is told of how it works, so it writes its part and leaves the carrying to Ringo.
const TALK_RELAY: &str = "Your reply is shown to them and theirs come back to you here, so write only your own part: do not \
                          try to reach them, and do not speak for them.";

/// Whether a reply in a talk adds nothing: the one word its turn was asked for.
fn agreed(reply: &str) -> bool {
    reply.trim().trim_end_matches(['.', '!']).eq_ignore_ascii_case("agreed")
}

/// Names as a sentence lists them: a, b and c.
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [most @ .., last] => format!("{} and {last}", most.join(", ")),
    }
}

/// An agent's list of models being read (/model), off the shell's own thread.
type Listing = std::thread::JoinHandle<Result<Vec<ringo_cli::Model>, String>>;

fn job_number(word: Option<&str>) -> Result<usize, String> {
    word.map(|w| w.trim_start_matches('#')).and_then(|w| w.parse().ok()).ok_or("give a job number, e.g. 3".into())
}

/// One input line as a command. A line without `/` or `@` is for the agent the owner is talking to; @all means every
/// set-up agent.
/// Commands that take nothing after them.
const BARE: [&str; 20] = ["usrman", "instructions", "manual", "help", "?", "quit", "exit", "q", "restart", "setup", "undo",
                          "copy", "clear", "new", "compact", "separate", "together", "auto", "usage", "cost"];

pub fn parse(line: &str, all: &[Agent]) -> Result<Cmd, String> {
    let line = line.trim();
    if line == "?" {
        return Ok(Cmd::Keys);
    }
    if let Some(rest) = line.strip_prefix('/') {
        let mut words = rest.split_whitespace();
        let name = words.next().unwrap_or("");
        let tail = rest[name.len()..].trim();
        // what comes after a command that takes nothing would otherwise be dropped unseen
        if BARE.contains(&name) && !tail.is_empty() {
            return Err(format!("/{name} takes nothing after it: send the rest on a line of its own"));
        }
        return match name {
            "usrman" | "instructions" | "manual" | "help" | "?" => Ok(Cmd::Help),
            "quit" | "exit" | "q" => Ok(Cmd::Quit),
            "restart" => Ok(Cmd::Restart),
            "setup" => Ok(Cmd::Setup),
            "undo" => Ok(Cmd::Undo),
            "copy" => Ok(Cmd::Copy),
            "clear" | "new" => Ok(Cmd::Clear),
            "compact" => Ok(Cmd::Compact),
            "separate" => Ok(Cmd::Separate(true)),
            "together" => Ok(Cmd::Separate(false)),
            "ask" if !tail.is_empty() => Ok(Cmd::Ask(tail.into())),
            "auto" => Ok(Cmd::Auto),
            "usage" | "cost" => Ok(Cmd::Usage),
            "tier" | "effort" => match words.next() {
                None => Ok(Cmd::Tier(None)),
                Some(t) => Tier::from_name(t).map(|t| Cmd::Tier(Some(t)))
                    .ok_or("/tier quick, /tier standard, /tier deep or /tier ultra".into()),
            },
            "model" | "models" => match (words.next(), words.next(), words.next()) {
                (None, ..) => Ok(Cmd::Model(None)),
                (Some(agent), Some(model), None) => Ok(Cmd::Model(Some((agent.trim_start_matches('@').into(), model.into())))),
                _ => Err("/model shows who runs on what; /model AGENT NAME sets one (/model claude opus), /model AGENT default \
                          goes back".into()),
            },
            "discuss" | "talk" => {
                let (mut agents, mut topic) = (Vec::new(), tail);
                while let Some(word) = topic.split_whitespace().next().filter(|w| w.starts_with('@')) {
                    agents.push(Agent::from_name(&word[1..]).ok_or(format!("no agent {}", &word[1..]))?);
                    topic = topic[word.len()..].trim_start();
                }
                Ok(Cmd::Discuss(agents, topic.to_string()))
            }
            "agent" | "agents" => match (words.next(), words.next()) {
                (None, _) => Ok(Cmd::Agent(None)),
                (Some("add"), Some(name)) => {
                    let (mut file, mut base, mut skills) = (None, None, false);
                    for w in words {
                        match w {
                            "skills" => skills = true,
                            w if base.is_none() && Agent::from_name(w).is_some() => base = Some(w.to_string()),
                            w if file.is_none() => file = Some(w.to_string()),
                            _ => return Err(AGENT_USAGE.into()),
                        }
                    }
                    Ok(Cmd::Agent(Some(OwnAgent::Add { name: name.trim_start_matches('@').into(), file, base, skills })))
                }
                (Some("remove" | "rm"), Some(name)) if words.next().is_none() =>
                    Ok(Cmd::Agent(Some(OwnAgent::Remove(name.trim_start_matches('@').into())))),
                _ => Err(AGENT_USAGE.into()),
            },
            "mode" => match words.next() {
                None => Ok(Cmd::Mode(None)),
                Some(m) => Mode::from_name(m).map(|m| Cmd::Mode(Some(m))).ok_or("/mode ask, /mode auto or /mode read".into()),
            },
            "theme" => Ok(Cmd::Theme(words.next().map(str::to_string))),
            "update" => match (words.next(), words.next()) {
                (None, _) => Ok(Cmd::Update(None)),
                (Some("auto"), Some("on")) => Ok(Cmd::Update(Some(true))),
                (Some("auto"), Some("off")) => Ok(Cmd::Update(Some(false))),
                (Some("approve"), None) => Ok(Cmd::Approve),
                _ => Err("/update, /update approve, or /update auto on|off".into()),
            },
            "backend" if !tail.is_empty() => Ok(Cmd::Backend(tail.into())),
            "show" => job_number(words.next()).map(Cmd::Show),
            "kill" => job_number(words.next()).map(Cmd::Kill),
            "diff" => job_number(words.next()).map(Cmd::Diff),
            "apply" => job_number(words.next()).map(Cmd::Apply),
            "drop" => job_number(words.next()).map(Cmd::Drop),
            "review" => match (words.next(), words.next()) {
                (Some(w), None) if w.starts_with('#') => job_number(Some(w)).map(Cmd::ReviewJob),
                (Some(base), head) => Ok(Cmd::ReviewRepo(base.into(), head.unwrap_or("HEAD").into())),
                _ => Err("/review #N, or /review BASE [HEAD]".into()),
            },
            "design" if !tail.is_empty() => Ok(Cmd::Design(tail.into())),
            "research" if !tail.is_empty() => Ok(Cmd::Research(tail.into())),
            "work" if !tail.is_empty() => Ok(Cmd::Work(tail.into())),
            _ => Err(format!("unknown or incomplete command /{name}; /help lists them")),
        };
    }
    let mut agents = Vec::new();
    let mut rest = line;
    while let Some(word) = rest.split_whitespace().next().filter(|w| w.starts_with('@')) {
        let (name, write) = match word[1..].strip_suffix('!') {
            Some(name) => (name, true),
            None => (&word[1..], false),
        };
        match name {
            "all" => agents.extend(all.iter().map(|a| (*a, true))), // each in its own copy, to compare
            _ => agents.push((Agent::from_name(name).ok_or_else(|| {
                let known: Vec<&str> = Agent::all().into_iter().map(|a| a.name()).collect();
                format!("no agent {name}: {} or all (/setup adds one)", known.join(", "))
            })?, write)),
        }
        rest = rest.trim_start()[word.len()..].trim_start();
    }
    if agents.is_empty() {
        return if rest.is_empty() { Err("type what you want, or /help".into()) } else { Ok(Cmd::Talk(rest.to_string())) };
    }
    if rest.is_empty() {
        return match agents[..] {
            [(agent, false)] if all.contains(&agent) => Ok(Cmd::Switch(agent)),
            [(agent, false)] => Err(format!("{} is not set up here; /setup adds it", agent.name())),
            _ => Err("say what the agent should do".into()),
        };
    }
    Ok(Cmd::Agents(agents, rest.to_string()))
}

/// A time to come, roughly: 40 min, 4 h 49 min, 2 d 13 h.
fn fmt_until(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{} min", s.div_ceil(60)),
        s if s < 86_400 => format!("{} h {} min", s / 3600, s % 3600 / 60),
        s => format!("{} d {} h", s / 86_400, s % 86_400 / 3600),
    }
}

/// One plan's line for /usage: each limit, how much of it is used, and when it starts over.
/// How much of a limit is used, as a colour: calm under 60%, a warning to 85%, then the failure colour.
/// How old the side panel's plan gauges may get before a turn's end reads them again.
const LIMITS_FRESH: Duration = Duration::from_secs(600);

fn used_style(percent: f64, t: &Theme) -> Style {
    match percent {
        p if p < 60.0 => t.ok,
        p if p < 85.0 => t.warn,
        _ => t.fail,
    }
}

/// An agent's name in its colour, bold, and what follows it, dim: the head of each agent's part of /usage.
fn agent_head(name: &str, style: Style, rest: &str, t: &Theme) -> Out {
    Out::Line(Line::from(vec![Span::styled(format!("  {name}"), style.add_modifier(Modifier::BOLD)),
                              Span::styled(rest.to_string(), t.dim)]), 4)
}

/// A plan as /usage shows it: the agent and plan name, then a gauge for each limit with when it starts over.
fn plan_rows(agent: &str, style: Style, plan: &ringo_cli::Plan, t: &Theme) -> Vec<Out> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = if plan.name.is_empty() { String::new() } else { format!(" · {} plan", plan.name) };
    let notes = plan.notes.iter().map(|n| format!(" · {n}")).collect::<String>();
    let none = if plan.windows.is_empty() { " · no limits reported" } else { "" };
    let mut rows = vec![agent_head(agent, style, &format!("{name}{none}{notes}"), t)];
    // one width for every plan's names ("weekly scoped" is the longest seen), so the gauges line up
    let pad = plan.windows.iter().map(|w| cols(&w.name)).max().unwrap_or(0).clamp(13, 16);
    rows.extend(plan.windows.iter().map(|w| Out::Meter {
        label: w.name.clone(), pad, percent: w.percent, value: format!("{:.0}%", w.percent),
        note: w.resets.map(|r| format!("starts over in {}", fmt_until(r.saturating_sub(now)))).unwrap_or_default(),
        fill: None,
    }));
    rows
}

/// One agent's plan, as read in the background.
type PlanRead = (Agent, Result<ringo_cli::Plan, String>);

/// Ultra's choice of reviewer: the backend and why it is not the session's (empty when it is), or why none can review.
type Pick = Result<(String, String), String>;

/// The backend ultra's review runs on, and why it is not the session's `backend` when it is not: that backend, unless
/// its plan has a limit used up or its agent is not set up; then the other subscription's (codex-chatgpt and claude
/// stand in for each other) when that one can take it. `read` gives an agent's plan, or None when it is not set up,
/// and is asked only as far as needed. A plan that could not be read counts as having room: the review then says
/// itself whether it ran. Err: why neither can review now.
fn reviewer(backend: &str, read: impl Fn(ringo_cli::Adapter) -> Option<Result<ringo_cli::Plan, String>>, now: u64) -> Pick {
    use ringo_cli::Adapter::{Claude, Codex};
    let first = match backend {
        "codex-chatgpt" | "codex" => Codex,
        "claude" => Claude,
        _ => return Ok((backend.to_string(), String::new())), // the owner's own command: which plan it bills is unknown
    };
    let (other, preset) = if first == Codex { (Claude, "claude") } else { (Codex, "codex-chatgpt") };
    let cannot = |adapter| -> Option<String> {
        let name = if adapter == Codex { "Codex" } else { "Claude" };
        let Some(plan) = read(adapter) else { return Some(format!("{name} is not set up")) };
        let full = plan.ok()?.windows.into_iter().find(|w| w.percent >= 100.0 && w.resets.is_none_or(|r| r > now))?;
        let reset = full.resets.map(|r| format!(", starts over in {}", fmt_until(r - now))).unwrap_or_default();
        Some(format!("{name} is out ({} {:.0}% used{reset})", full.name, full.percent))
    };
    let Some(why) = cannot(first) else { return Ok((backend.to_string(), String::new())) };
    match cannot(other) {
        None => Ok((preset.to_string(), why)),
        Some(also) => Err(format!("{why} and {also}")),
    }
}

pub use ringo_cli::parse::fmt_tokens;

fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 3600 { format!("{}:{:02}", s / 60, s % 60) } else { format!("{}h{:02}", s / 3600, s / 60 % 60) }
}

/// Wraps text to `width` columns (one column per character).
fn wrap(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width.max(1)).map(|c| c.iter().collect()).collect()
}

/// Text as the terminal should show it: line ends as `\n`, a tab as four spaces, and no other control character (an
/// escape sequence in an agent's output would move the cursor; a raw tab or carriage return spreads a paste out).
fn clean(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ").chars()
        .filter(|c| *c == '\n' || !c.is_control()).collect()
}

/// A reply still being written, its last line's open `code` or **bold** closed: shown as it will end up instead of
/// showing the marks and then changing when they close (two of a real reply's 81 frames did). A mark with nothing
/// after it yet is left out until its text comes. Inside a code block nothing changes. The whole reply replaces it.
fn closed(mut text: String) -> String {
    let fenced = text.lines().filter(|l| l.trim_start().starts_with("```")).count() % 2 == 1;
    let last = text.rsplit('\n').next().unwrap_or("");
    let odd = |line: &str, mark: &str| line.matches(mark).count() % 2 == 1;
    // bold is counted outside the line's code spans: `2 ** 3` opens nothing
    let mark = if fenced { None } else if odd(last, "`") { Some("`") }
        else if odd(&last.split('`').step_by(2).collect::<String>(), "**") { Some("**") } else { None };
    let Some(mark) = mark else { return text };
    let after = last.rsplit(mark).next().unwrap_or("").len();
    if text[text.len() - after..].trim().is_empty() {
        text.truncate(text.len() - after - mark.len());
    } else {
        text.insert_str(text.trim_end().len(), mark);
    }
    text
}

/// How long the word being written waits for its end before it shows anyway, and a table for its last row: a
/// reply's pieces come tens of milliseconds apart, so a pause this long is the agent stopping to think or to work.
const WORD_HOLD: Duration = Duration::from_millis(150);
const TABLE_HOLD: Duration = Duration::from_millis(1500);
/// A word, or a link, longer than this shows as it grows (a path, a URL, a `[` that opens no link).
const HELD_WORD: usize = 32;
const HELD_LINK: usize = 60;

/// What shows of a reply being written (`text`, its pieces last came `age` ago), so that a row someone is reading
/// does not change under them. Kept back until it is whole:
/// - the word being written, which could move to the next row or close a mark, until its end comes or WORD_HOLD;
/// - a link whose text has a space in it, until it closes (its marks would go when it does);
/// - a table, whose columns move with every wider cell, until a line that is not one of its rows comes or
///   TABLE_HOLD: one line stands in for it and counts its rows.
///
/// Nothing before `floor` is kept back: what has shown stays. Returns the text to show, how much of `text` it
/// is, and how long after the last piece to look again (None: nothing was kept back).
/// Measured on the 14.8k guide written in pieces of 4 to 24 characters at 120 columns: rows above the one being
/// written changed in 85 of 1,060 frames before, and the row being written took text back in 18.
fn steady(text: &str, age: Duration, floor: usize) -> (String, usize, Option<Duration>) {
    let fenced = text.lines().filter(|l| l.trim_start().starts_with("```")).count() % 2 == 1;
    let (mut keep, mut again, mut note) = (text.len(), None, String::new());
    if !fenced && age < TABLE_HOLD {
        // the lines at the end that start a table's rows; an empty line being started is not yet a line
        let mut lines: Vec<&str> = text.split('\n').collect();
        let open = lines.last().is_some_and(|l| !l.is_empty());
        if !open {
            lines.pop();
        }
        let run = lines.iter().rev().take_while(|l| l.trim_start().starts_with('|')).count();
        let start: usize = lines[..lines.len() - run].iter().map(|l| l.len() + 1).sum();
        if run > 0 && start >= floor {
            // its heading and the rule under it are not rows
            let rows = (run - open as usize).saturating_sub(2);
            note = match rows {
                0 => "*a table is being written…*".to_string(),
                1 => "*a table is being written · 1 row so far…*".to_string(),
                n => format!("*a table is being written · {n} rows so far…*"),
            };
            (keep, again) = (start, Some(TABLE_HOLD));
        }
    }
    if note.is_empty() && age < WORD_HOLD {
        let end = text.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
        let word = &text[end..];
        // wide characters (Chinese, Japanese, Korean) have no spaces between words: they show as they come
        if !word.is_empty() && end >= floor && word.chars().count() <= HELD_WORD && word.chars().all(|c| c.width() != Some(2)) {
            (keep, again) = (end, Some(WORD_HOLD));
        }
        let line = text[..keep].rfind('\n').map_or(0, |i| i + 1);
        if !fenced && let Some(open) = text[line..keep].rfind('[').map(|i| line + i) {
            let rest = &text[open..keep];
            let whole = match rest.find(']') {
                None => false,
                Some(close) => match rest[close + 1..].chars().next() {
                    None => false, // "](" may still come
                    Some('(') => rest[close..].contains(')'),
                    Some(_) => true, // "[x] done": no link
                },
            };
            let in_code = text[line..open].matches('`').count() % 2 == 1;
            if !whole && !in_code && open >= floor && rest.chars().count() <= HELD_LINK {
                (keep, again) = (open, Some(WORD_HOLD));
            }
        }
    }
    let shown = closed(text[..keep].to_string());
    let shown = match (note.is_empty(), shown.trim().is_empty()) {
        (true, _) => shown,
        (false, true) => note,
        (false, false) => format!("{}\n\n{note}", shown.trim_end()),
    };
    (shown, keep, again)
}

/// A reply being written: its place in the conversation, its pieces as they came (cleaned whole, so a line end split
/// across two pieces stays one), when the last piece came, how much of it has shown (see `steady`'s floor), and
/// when to look again at what was kept back.
struct Stream {
    at: usize,
    raw: String,
    last: Instant,
    floor: usize,
    due: Option<Instant>,
}

/// The input as rows of at most `width` columns (a newline starts a row; a wide character takes two columns), and the
/// cursor's (row, column).
fn input_rows(input: &[char], cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(2);
    let (mut rows, mut col, mut at) = (vec![String::new()], 0, (0, 0));
    for (i, &c) in input.iter().enumerate() {
        let w = if c == '\n' { 0 } else { c.width().unwrap_or(0) };
        if col + w > width {
            rows.push(String::new());
            col = 0;
        }
        if i == cursor {
            at = (rows.len() - 1, col);
        }
        if c == '\n' {
            rows.push(String::new());
            col = 0;
        } else {
            rows.last_mut().expect("at least one row").push(c);
            col += w;
        }
    }
    if cursor >= input.len() {
        if col >= width {
            rows.push(String::new()); // the cursor's own cell, past a full row
            col = 0;
        }
        at = (rows.len() - 1, col);
    }
    (rows, at)
}

/// Where the word before `at` starts (Ctrl-W, Alt-B), and where the one after it ends (Alt-D, Alt-F): words are runs
/// of anything but white space, as in a shell.
fn word_start(input: &[char], at: usize) -> usize {
    let blank = input[..at].iter().rposition(|c| !c.is_whitespace()).map_or(0, |i| i + 1);
    input[..blank].iter().rposition(|c| c.is_whitespace()).map_or(0, |i| i + 1)
}

fn word_end(input: &[char], at: usize) -> usize {
    let word = input[at..].iter().position(|c| !c.is_whitespace()).map_or(input.len(), |i| at + i);
    input[word..].iter().position(|c| c.is_whitespace()).map_or(input.len(), |i| word + i)
}

/// The start and end of the line the cursor is on, in a message of several lines.
fn line_bounds(input: &[char], at: usize) -> (usize, usize) {
    (input[..at].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1),
     input[at..].iter().position(|c| *c == '\n').map_or(input.len(), |i| at + i))
}

/// Where Up or Down moves the cursor in a message of several lines, keeping its column; None on the first line (for
/// Up) or the last (for Down), where the key goes through earlier messages instead.
fn vertical(input: &[char], cursor: usize, up: bool) -> Option<usize> {
    let start = |at: usize| input[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
    let end = |at: usize| input[at..].iter().position(|&c| c == '\n').map_or(input.len(), |i| at + i);
    let here = start(cursor);
    let col = cursor - here;
    if up {
        if here == 0 {
            return None;
        }
        let prev = start(here - 1);
        Some(prev + col.min(here - 1 - prev))
    } else {
        let stop = end(cursor);
        if stop == input.len() {
            return None;
        }
        let next = stop + 1;
        Some(next + col.min(end(next) - next))
    }
}

/// Columns a string takes (per character, as `fit` counts them).
fn cols(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// A line as rows of at most `width` columns, broken at spaces (a word longer than a row is cut), every row after the
/// first starting `indent` columns in, so a wrapped item stays under its own text. Emoji presentation selectors go:
/// terminals disagree on how wide they make a character, and a disagreement shifts the rest of the row.
fn fit(line: &Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = indent.min(width / 2);
    let cells: Vec<(char, Style)> = line.spans.iter()
        .flat_map(|s| {
            let style = line.style.patch(s.style);
            s.content.chars().filter(|c| *c != '\u{FE0F}').map(move |c| (c, style))
        })
        .collect();
    let w = |c: char| c.width().unwrap_or(0);
    let (mut rows, mut col): (Vec<Vec<(char, Style)>>, usize) = (vec![Vec::new()], 0);
    let fresh_row = |rows: &mut Vec<Vec<(char, Style)>>, col: &mut usize| {
        let row = rows.last_mut().expect("a row");
        while row.last().is_some_and(|(c, _)| *c == ' ') {
            row.pop(); // the spaces at a break
        }
        rows.push(vec![(' ', Style::default()); indent]);
        *col = indent;
    };
    let mut i = 0;
    while i < cells.len() {
        let space = cells[i].0 == ' ';
        let end = cells[i..].iter().position(|(c, _)| (*c == ' ') != space).map_or(cells.len(), |n| i + n);
        let token = &cells[i..end];
        let tw: usize = token.iter().map(|(c, _)| w(*c)).sum();
        let start = if rows.len() == 1 { 0 } else { indent };
        if col + tw <= width {
            if !(space && col == start && rows.len() > 1) {
                rows.last_mut().expect("a row").extend_from_slice(token);
                col += tw;
            }
        } else if space {
            fresh_row(&mut rows, &mut col);
        } else {
            if col > start && tw <= width - indent {
                fresh_row(&mut rows, &mut col);
            }
            for &(c, style) in token {
                if col + w(c) > width {
                    fresh_row(&mut rows, &mut col);
                }
                rows.last_mut().expect("a row").push((c, style));
                col += w(c);
            }
        }
        i = end;
    }
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, style) in row {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// An agent's markdown reply as rows of at most `width` columns: headings, emphasis and code styled, list items and
/// quotes indented with their wrapped rows under their text, code blocks as they are, and a table as aligned columns
/// when it fits, or when all but its last column fit in half the row, the last one wrapping under itself (else one
/// block per row: its first cell, then each other cell under its column's name).
fn markdown(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    use pulldown_cmark::{Options, Parser};
    let mut md = Md { t: *theme, width: width.max(8), out: Vec::new(), spans: Vec::new(), style: Vec::new(),
                      lists: Vec::new(), marker: None, quote: 0, code: None, table: None, links: Vec::new() };
    for event in Parser::new_ext(&text.replace('\u{FE0F}', ""), Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        md.event(event);
    }
    md.flush();
    while md.out.last().is_some_and(|l| l.width() == 0) {
        md.out.pop();
    }
    md.out
}

#[derive(Default)]
struct Table {
    head: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
}

struct Md {
    t: Theme,
    width: usize,
    out: Vec<Line<'static>>,
    /// The inline text of the block being read, and the styles open in it.
    spans: Vec<Span<'static>>,
    style: Vec<Style>,
    /// Open lists: the next number of an ordered one (None: bullets), and its item marker's width.
    lists: Vec<(Option<u64>, usize)>,
    /// The marker of the list item whose first row is not out yet.
    marker: Option<String>,
    quote: usize,
    code: Option<String>,
    table: Option<Table>,
    /// Open links: where each goes, and where its text starts in `spans`.
    links: Vec<(String, usize)>,
}

impl Md {
    fn cur(&self) -> Style {
        self.style.last().copied().unwrap_or_default()
    }

    fn open(&mut self, style: Style) {
        self.style.push(self.cur().patch(style));
    }

    fn event(&mut self, event: pulldown_cmark::Event) {
        use pulldown_cmark::Event as E;
        match event {
            E::Start(tag) => self.start(tag),
            E::End(tag) => self.end(tag),
            E::Text(text) => match &mut self.code {
                Some(code) => code.push_str(&text),
                None => self.spans.push(Span::styled(text.into_string(), self.cur())),
            },
            E::Code(text) => self.spans.push(Span::styled(text.into_string(), self.cur().patch(self.t.ringo))),
            E::SoftBreak => self.spans.push(Span::styled(" ", self.cur())),
            E::HardBreak => self.flush(),
            E::Rule => {
                self.blank();
                self.out.push(Line::styled("─".repeat(self.width.min(40)), self.t.dim));
                self.blank();
            }
            E::Html(text) | E::InlineHtml(text) => self.spans.push(Span::styled(text.into_string(), self.cur())),
            _ => {} // footnotes, task lists and math are not turned on
        }
    }

    fn start(&mut self, tag: pulldown_cmark::Tag) {
        use pulldown_cmark::Tag as T;
        use ratatui::style::Modifier;
        match tag {
            T::Heading { .. } => {
                self.flush();
                self.blank();
                self.open(self.t.focus.add_modifier(Modifier::BOLD));
            }
            T::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            T::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            T::List(start) => {
                self.flush();
                self.lists.push((start, 0));
            }
            T::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some((Some(n), w)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        *w = cols(&m);
                        m
                    }
                    Some((None, w)) => {
                        *w = 2;
                        "• ".to_string()
                    }
                    None => String::new(),
                };
                self.marker = Some(marker);
            }
            T::Emphasis => self.open(Style::new().add_modifier(Modifier::ITALIC)),
            T::Strong => self.open(Style::new().add_modifier(Modifier::BOLD)),
            T::Strikethrough => self.open(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            T::Link { dest_url, .. } => {
                self.open(Style::new().add_modifier(Modifier::UNDERLINED));
                self.links.push((dest_url.into_string(), self.spans.len()));
            }
            T::Table(_) => {
                self.flush();
                self.table = Some(Table::default());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: pulldown_cmark::TagEnd) {
        use pulldown_cmark::TagEnd as T;
        match tag {
            T::Paragraph => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            T::Heading(_) => {
                self.flush();
                self.style.pop();
                self.blank();
            }
            T::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.blank();
            }
            T::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                for line in code.strip_suffix('\n').unwrap_or(&code).split('\n') {
                    self.emit_under(vec![Span::styled(format!("  {line}"), self.t.ringo)], 2);
                }
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            T::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            T::Item => {
                self.flush();
                self.marker = None;
            }
            T::Emphasis | T::Strong | T::Strikethrough => {
                self.style.pop();
            }
            T::Link => {
                self.style.pop();
                if let Some((url, at)) = self.links.pop() {
                    let shown: String = self.spans[at.min(self.spans.len())..].iter().map(|s| s.content.as_ref()).collect();
                    if url.starts_with("http") && !shown.contains(&url) {
                        self.spans.push(Span::styled(format!(" ({url})"), self.t.dim));
                    }
                }
            }
            T::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            T::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            T::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            T::Table => {
                if let Some(table) = self.table.take() {
                    self.put_table(table);
                }
                self.blank();
            }
            _ => {}
        }
    }

    /// The inline text read so far, as one block's rows.
    fn flush(&mut self) {
        if !self.spans.is_empty() && self.table.is_none() {
            let spans = std::mem::take(&mut self.spans);
            self.emit(spans);
        }
    }

    /// Rows for one line of a block, under the open quotes and list items (the item's marker on its first row).
    fn emit(&mut self, spans: Vec<Span<'static>>) {
        self.emit_under(spans, 0);
    }

    /// `emit`, with wrapped rows `extra` columns further in (a table's last column wraps under itself).
    fn emit_under(&mut self, spans: Vec<Span<'static>>, extra: usize) {
        let depth: usize = self.lists.iter().map(|(_, w)| *w).sum();
        let mark_w = self.lists.last().map_or(0, |(_, w)| *w);
        let marker = self.marker.take().unwrap_or_else(|| " ".repeat(mark_w));
        let quote = "│ ".repeat(self.quote);
        let hang = cols(&quote) + depth + extra;
        let mut line = vec![Span::styled(quote.clone(), self.t.dim), Span::raw(" ".repeat(depth - mark_w)),
                            Span::styled(marker, self.t.dim)];
        line.extend(spans);
        let mut rows = fit(&Line::from(line), self.width, hang);
        for row in rows.iter_mut().skip(1).filter(|_| !quote.is_empty()) {
            // a quote's wrapped rows keep its bar
            let pad = row.spans.first().map_or("", |s| s.content.as_ref()).to_string();
            if let Some(rest) = pad.strip_prefix(&" ".repeat(cols(&quote))) {
                row.spans[0] = Span::raw(rest.to_string());
                row.spans.insert(0, Span::styled(quote.clone(), self.t.dim));
            }
        }
        self.out.extend(rows);
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| l.width() > 0) {
            self.out.push(Line::default());
        }
    }

    fn put_table(&mut self, table: Table) {
        use ratatui::style::Modifier;
        let width = |cell: &[Span]| cell.iter().map(|s| cols(&s.content)).sum::<usize>();
        let all = || std::iter::once(&table.head).chain(&table.rows);
        let n = all().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; n];
        for row in all() {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(width(cell));
            }
        }
        let total = widths.iter().sum::<usize>() + 2 * n.saturating_sub(1);
        let depth: usize = self.lists.iter().map(|(_, w)| *w).sum::<usize>() + cols(&"│ ".repeat(self.quote));
        let bold = Style::new().add_modifier(Modifier::BOLD);
        // The columns before the last, with their gaps: when they take at most half the row, the last column wraps
        // under itself (`fit` hangs a wrapped row at most half the width in).
        let before = total - widths.last().copied().unwrap_or(0);
        if total + depth <= self.width || (n > 1 && depth + before <= self.width / 2) {
            let line = |row: &[Vec<Span<'static>>], head: bool| -> Vec<Span<'static>> {
                let mut spans = Vec::new();
                for (c, w) in widths.iter().enumerate() {
                    let cell = row.get(c).cloned().unwrap_or_default();
                    let pad = if c + 1 < n { w - width(&cell) + 2 } else { 0 };
                    spans.extend(cell.into_iter().map(|s| if head { s.patch_style(bold) } else { s }));
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans
            };
            let rule = total.min(self.width - depth);
            let (head, rule) = (line(&table.head, true), vec![Span::styled("─".repeat(rule), self.t.dim)]);
            self.emit_under(head, before);
            self.emit(rule);
            for row in &table.rows {
                let spans = line(row, false);
                self.emit_under(spans, before);
            }
            return;
        }
        let names: Vec<String> = table.head.iter().map(|c| c.iter().map(|s| s.content.as_ref()).collect()).collect();
        for row in &table.rows {
            self.lists.push((None, 2));
            self.marker = Some("• ".into());
            let first: Vec<Span<'static>> = row.first().cloned().unwrap_or_default().into_iter().map(|s| s.patch_style(bold)).collect();
            self.emit(first);
            for (c, cell) in row.iter().enumerate().skip(1) {
                let name = names.get(c).filter(|n| !n.trim().is_empty()).map_or(String::new(), |n| format!("{n}: "));
                let mut spans = vec![Span::styled(name, self.t.dim)];
                spans.extend(cell.iter().cloned());
                self.emit(spans);
            }
            self.lists.pop();
        }
    }
}

/// The terminal, full screen: raw mode, the alternate screen, the mouse (the wheel scrolls, a drag selects), bracketed
/// paste, and Shift-Enter where the terminal can report it.
fn open() -> std::io::Result<DefaultTerminal> {
    enable_raw_mode()?;
    // The first frame draws only what differs from a blank screen, so a terminal that keeps what was on it (the shell
    // before a /restart, where there is no alternate screen of its own) is made blank. Terminal::clear would ask the
    // terminal where its cursor is, and one that does not answer would stop the shell from opening.
    use ratatui::crossterm::terminal::{Clear, ClearType};
    execute!(stdout(), EnterAlternateScreen, Clear(ClearType::All), EnableMouseCapture)?;
    // A paste arrives as one event, not as keys (its newlines as Enter). Not fatal: the legacy Windows console has no
    // bracketed paste, and there a paste's keys are told apart by their timing (see BURST).
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Shift-Enter reaches the shell only where the terminal speaks the kitty keyboard protocol; elsewhere it is Enter.
    let keys = supports_keyboard_enhancement().unwrap_or(false)
        && execute!(stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)).is_ok();
    SHIFT_ENTER.store(keys, Ordering::Relaxed);
    Terminal::new(CrosstermBackend::new(stdout()))
}

/// The terminal as it was: the keyboard protocol, bracketed paste and the mouse off, the alternate screen and raw mode
/// left.
fn restore() {
    if SHIFT_ENTER.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// The screen goes and the terminal is as it was (for an agent's sign-in, an update, or the end).
fn close(terminal: &mut DefaultTerminal) {
    let _ = terminal.show_cursor();
    restore();
}

/// A conversation entry. It is fitted to the screen's width when it shows (and again when the width changes).
#[derive(Clone, Debug)]
enum Out {
    /// A line; a row past the width continues this many columns in.
    Line(Line<'static>, usize),
    /// An agent's reply, as markdown, two columns in.
    Reply(String),
    /// A step an agent took, on one row under its job; an edit's changed lines follow it, one row each.
    Step(String),
    /// A run of steps that only looked, counted: files read, searches, folders listed.
    Looked([u32; 3]),
    /// A labelled gauge, four columns in: `label` (padded to `pad`), ▰ cells for `percent` of the row's room, then
    /// `value` and `note`. Its cells take `fill`, else the colour of how much is used (see `used_style`).
    Meter { label: String, pad: usize, percent: f64, value: String, note: String, fill: Option<Style> },
}

impl Out {
    fn rows(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        match self {
            Out::Line(line, indent) => fit(line, width, *indent),
            Out::Reply(text) => markdown(text, width.saturating_sub(2), theme).into_iter()
                .map(|mut line| {
                    line.spans.insert(0, Span::raw("  "));
                    line
                })
                .collect(),
            Out::Step(text) => {
                let mut lines = text.lines();
                let mut rows = vec![step_row(lines.next().unwrap_or(""), width, theme)];
                rows.extend(lines.map(|line| {
                    let style = match line.as_bytes().first() {
                        Some(b'+') => theme.ok,
                        Some(b'-') => theme.fail,
                        _ => theme.dim,
                    };
                    Line::from(Span::styled(format!("      {}", cut(line, width.saturating_sub(6))), style))
                }));
                rows
            }
            Out::Looked(n) => {
                let parts: Vec<String> = [(n[0], "read", "file"), (n[1], "searched", "time"), (n[2], "listed", "folder")]
                    .into_iter().filter(|(k, ..)| *k > 0)
                    .map(|(k, verb, what)| format!("{verb} {k} {what}{}", if k == 1 { "" } else { "s" })).collect();
                vec![Line::from(vec![Span::styled("  ✓ ", theme.ok),
                                     Span::styled(cut(&parts.join(", "), width.saturating_sub(4)), theme.dim)])]
            }
            Out::Meter { label, pad, percent, value, note, fill } => {
                // the gauge takes what the label, value and note leave, between 8 and 24 cells
                let cells = width.saturating_sub(4 + pad + 1 + 1 + cols(value).max(4) + 2 + cols(note)).clamp(8, 24);
                let on = match (percent.clamp(0.0, 100.0) / 100.0 * cells as f64).round() as usize {
                    0 if *percent > 0.0 => 1, // a little used is not nothing
                    n => n,
                };
                fit(&Line::from(vec![Span::raw(format!("    {label:<pad$} ")),
                                     Span::styled("▰".repeat(on), fill.unwrap_or_else(|| used_style(*percent, theme))),
                                     Span::styled("▱".repeat(cells - on), theme.border),
                                     Span::raw(format!(" {value:>4}")), Span::styled(format!("  {note}"), theme.dim)]),
                    width, 4)
            }
        }
    }

    /// A line of text, its wrapped rows under its own leading spaces.
    fn text(line: &str) -> Out {
        Out::Line(Line::raw(line.to_string()), line.len() - line.trim_start_matches(' ').len())
    }

    fn blank(&self) -> bool {
        matches!(self, Out::Line(line, _) if line.width() == 0)
    }
}

/// A step as one row of at most `width` columns: a long one loses the end of what it did (a search pattern, a
/// command) to "…", and keeps what came of it (after the last " · "); the whole step is in the job's log.
fn one_row(text: &str, width: usize) -> String {
    if cols(text) <= width {
        return text.to_string();
    }
    match text.rsplit_once(" · ") {
        Some((what, came)) if cols(came) + 3 <= width / 2 => format!("{} · {came}", cut(what, width - cols(came) - 3)),
        _ => cut(text, width),
    }
}

/// Text in at most `room` columns: a longer one is cut, and ends in "…".
fn cut(text: &str, room: usize) -> String {
    if cols(text) <= room {
        return text.to_string();
    }
    let (mut out, mut used) = (String::new(), 0);
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used + 1 > room {
            break;
        }
        out.push(c);
    }
    format!("{}…", out.trim_end())
}

/// A step's row: ✓ when it went through, ✗ when it failed, ◌ when the turn ended before its result, · when its agent
/// reports only that it started; the verb in lower
/// case (`run` for a command), and what came of it right-aligned at the row's end, or at 64 columns on a wide pane.
fn step_row(text: &str, width: usize, theme: &Theme) -> Line<'static> {
    use ringo_cli::parse::{FAILED, STARTED, UNFINISHED};
    let (mark, style, body) = match (text.strip_prefix(FAILED), text.strip_prefix(UNFINISHED), text.strip_prefix(STARTED)) {
        (Some(body), ..) => ("✗", theme.fail, body),
        (_, Some(body), _) => ("◌", theme.dim, body),
        (.., Some(body)) => ("·", theme.dim, body),
        _ => ("✓", theme.ok, text),
    };
    let body = match body.strip_prefix("$ ") {
        Some(command) => format!("run {command}"),
        None => match body.split_once(' ') {
            Some((verb, rest)) => format!("{} {rest}", verb.to_lowercase()),
            None => body.to_lowercase(),
        },
    };
    let room = width.saturating_sub(4);
    let line = room.min(64);
    let (what, came) = body.rsplit_once(" · ").unwrap_or((&body, ""));
    let (what, pad, came) = if !came.is_empty() && cols(what) + 2 + cols(came) <= line {
        (what.to_string(), line - cols(what) - cols(came), came.to_string())
    } else {
        (one_row(&body, room), 0, String::new())
    };
    Line::from(vec![Span::styled(format!("  {mark} "), style), Span::styled(what, theme.dim), Span::raw(" ".repeat(pad)),
                    Span::styled(came, theme.dim)])
}

/// The columns `from..to` of a row of text (a wide character counts where it starts).
fn slice_cols(text: &str, from: usize, to: usize) -> String {
    let mut col = 0;
    text.chars().filter(|c| {
        let keep = col >= from && col < to;
        col += c.width().unwrap_or(0);
        keep
    }).collect()
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Puts text on the clipboard with the system's own tool; where there is none (a server over ssh), the terminal is
/// asked to (OSC 52, which many allow). True when a tool took it. ceiling: Windows' clip garbles non-ASCII text.
fn copy(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else {
        &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "--clipboard", "--input"]]
    };
    for tool in tools {
        let Ok(mut child) = Command::new(tool[0]).args(&tool[1..]).stdin(Stdio::piped()).stdout(Stdio::null())
            .stderr(Stdio::null()).spawn() else { continue };
        let wrote = child.stdin.take().is_some_and(|mut pipe| pipe.write_all(text.as_bytes()).is_ok());
        if child.wait().is_ok_and(|s| s.success()) && wrote {
            return true;
        }
    }
    let _ = write!(stdout(), "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = stdout().flush();
    false
}

/// The Keys section of the guide, for `?`.
fn keys() -> String {
    let section = MANUAL.split_once("## Keys").map_or("", |(_, rest)| rest.split("\n## ").next().unwrap_or(rest));
    format!("## Keys{section}")
}

/// The agent a plain line goes to before the owner picks one with @: claude for code, else codex, else any that
/// works in the folder.
fn first_choice(agents: &[Agent]) -> Agent {
    ["claude", "codex"].iter().find_map(|n| agents.iter().find(|a| a.name() == *n))
        .or_else(|| agents.iter().find(|a| a.spec().read)).copied().unwrap_or(agents[0])
}

/// The one git project below `top` that every file in `touched` (relative to `top`) is in, if there is one.
fn one_project(top: &Path, touched: &std::collections::BTreeSet<String>) -> Option<PathBuf> {
    let mut found: Option<PathBuf> = None;
    for file in touched {
        let path = top.join(file);
        let project = path.ancestors().skip(1).take_while(|d| *d != top && d.starts_with(top))
            .find(|d| d.join(".git").exists())?;
        match &found {
            Some(f) if f != project => return None,
            _ => found = Some(project.to_path_buf()),
        }
    }
    found
}

/// A path under the home folder as `~/…`.
fn home_short(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// A spot in the conversation as it is fitted now: (row, column).
type Spot = (usize, usize);
/// A place in the conversation that holds while entries before it grow: an entry, a row of it, and a column.
type Mark = (usize, usize, usize);

/// How many entries the conversation keeps on screen before the oldest go (all of it is saved when the shell ends).
const KEEP: usize = 20_000;

struct App {
    s: Session,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist: usize,
    /// The conversation, and each entry fitted to `fitted_w` (fitted lazily; all again when the width changes).
    out: Vec<Out>,
    fitted: Vec<Vec<Line<'static>>>,
    fitted_w: usize,
    /// Rows scrolled up from the end (0: following it), where the conversation showed in the last frame and its
    /// first row there, and the text selected with the mouse (from, to) and whether a drag is under way.
    scroll: usize,
    /// Scrolled up: `scroll` as the last frame left it, and the row at the top then (an entry, and a row of it).
    scrolled: usize,
    anchor: Option<(usize, usize)>,
    view: Rect,
    top: usize,
    sel: Option<(Mark, Mark)>,
    dragging: bool,
    /// A short note at the input's right end (copied, could not copy) and when it appeared.
    toast: Option<(String, Instant)>,
    /// How the last job ended (true: done) and when, for the cat; and the cat's look last drawn.
    ended_at: Option<(bool, Instant)>,
    drawn_look: (&'static str, usize),
    /// Each job's reply being written.
    streams: std::collections::HashMap<usize, Stream>,
    /// The agent plain lines go to, and lines waiting for the turn running in this folder: (agent, task, as typed).
    main: Agent,
    queue: VecDeque<(Agent, String, String)>,
    /// The menu's chosen row (see `menu`), and the chosen answer to a permission request.
    pick: usize,
    choice: usize,
    /// Per job (at its id - 1): whether the line it ended with is printed.
    ended: Vec<bool>,
    /// When the last key arrived: an Enter right behind another key is part of a paste (see BURST).
    last_key: Instant,
    /// The job whose line was printed last (0: none, or the shell's own), so a run of one job's lines is named once.
    last: usize,
    quit_armed: bool,
    quit: bool,
    /// `/restart` asked: when the loop ends, the installed `ringo` starts in this terminal.
    restart: bool,
    /// What Ringo's own update said last, and the binary it built: put in place when the shell ends or restarts.
    updated: Option<String>,
    staged: Option<PathBuf>,
    setup: bool,
    label: String,
    agents: Vec<Agent>,
    theme: Theme,
    started: Instant,
    /// The permission request on screen and when it appeared (answer keys wait ANSWER_GRACE after that).
    asked: Option<(String, Instant)>,
    /// Agent updates found when the shell opened (None until that check reports), and `/update` asked for.
    pending: Option<Vec<crate::updates::Update>>,
    update: bool,
    /// The pipelines' release being checked or approved in the background (updates::check_release), and a checked
    /// one waiting for `/update approve`.
    releasing: Option<std::thread::JoinHandle<(String, Option<crate::updates::Release>)>>,
    release: Option<crate::updates::Release>,
    /// Plan reads under way, one thread an agent, each with whether its answer goes into the conversation (/usage)
    /// or only into `limits` (the side panel's gauges).
    plans: Vec<(bool, std::thread::JoinHandle<PlanRead>)>,
    /// /model's read of the models an agent's plan offers, under way (see `ringo_cli::models`).
    listing: Option<(Agent, Listing)>,
    /// Each plan's limits as last read, and when the last read began (None: never, as in tests).
    limits: Vec<(Agent, ringo_cli::Plan)>,
    limits_at: Option<Instant>,
    /// Whether the owner has said anything yet: until then the conversation shows where to start (see `splash`).
    spoke: bool,
    /// Open in the home folder: whether the shell has pointed to the project a turn's edits all went to (once).
    pointed: bool,
    /// Ultra: Ringo's review under way of a turn here (review job, turn, its agent), during which lines wait; the
    /// revision it asked for (its task, while queued) and that revision's job, which no review follows.
    checking: Option<(usize, usize, Agent)>,
    /// The agents' talk under way (/discuss).
    talking: Option<Talk>,
    revision: Option<String>,
    revision_id: Option<usize>,
    /// Ultra: the plan read a turn here started (the turn, the read), then its answer (see `reviewer`).
    quota: Option<(usize, std::thread::JoinHandle<Pick>)>,
    reviewer: Option<(usize, Pick)>,
}

/// What the bottom of the screen holds in one frame: the running jobs' rows, the queued lines, the permission
/// request, the input's rows (when it shows), and the menu.
struct Live {
    /// The side panel shows (a wide enough screen); without it the jobs and the queue sit above the input.
    panel: bool,
    jobs: Vec<Line<'static>>,
    queue: Vec<Line<'static>>,
    ask: Option<(String, Vec<Line<'static>>)>,
    ask_h: u16,
    typing: bool,
    rows: Vec<String>,
    at: (usize, usize),
    /// The first input row shown, and how many are.
    top: usize,
    shown: usize,
    menu: Vec<Line<'static>>,
    /// What the input starts with: " claude ❯ ", the agent the line goes to (see `target`), and its width.
    mark: Vec<Span<'static>>,
    mark_w: usize,
}

impl Live {
    fn input_h(&self) -> u16 {
        if self.typing { self.shown as u16 } else { 0 }
    }
}

/// The running mark: a braille spinner, one frame per SPIN_MS while anything runs (the only redraws then).
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPIN_MS: u128 = 120;
/// An Enter this soon after the key before it came with it, as a paste's keys do in a terminal without bracketed
/// paste (a person's keystrokes are tens of milliseconds apart): it starts a new line instead of sending.
const BURST: Duration = Duration::from_millis(15);
/// How long after a permission request appears its answer keys are ignored.
const ANSWER_GRACE: Duration = Duration::from_millis(300);
/// How long a note stays at the input's right end.
const TOAST: Duration = Duration::from_millis(2500);
/// The side panel shows from this width, this wide.
const PANEL_AT: u16 = 100;
const PANEL_W: u16 = 32;
/// How long the cat shows each frame, and how long it shows how the last job ended.
const MASCOT_MS: u128 = 240;
const MOOD: Duration = Duration::from_secs(4);

/// The cat's frames for a state, from mascot.txt (idle's when the file has none for it); each frame its rows.
fn mascot(state: &str) -> Vec<Vec<&'static str>> {
    const ART: &str = include_str!("mascot.txt");
    let read = |want: &str| {
        let (mut frames, mut inside): (Vec<Vec<&'static str>>, bool) = (Vec::new(), false);
        for line in ART.lines() {
            if let Some(name) = line.strip_prefix("== ") {
                inside = name.trim() == want;
                if inside {
                    frames.push(Vec::new());
                }
            } else if inside && line == "--" {
                frames.push(Vec::new());
            } else if inside && let Some(frame) = frames.last_mut() {
                frame.push(line);
            }
        }
        frames.retain(|f| !f.is_empty());
        frames
    };
    let frames = read(state);
    if frames.is_empty() { read("idle") } else { frames }
}

impl App {
    fn new(s: Session, agents: Vec<Agent>, label: String, theme: Theme, history: Vec<String>) -> App {
        App { s, input: Vec::new(), cursor: 0, hist: history.len(), history, out: Vec::new(), fitted: Vec::new(),
              fitted_w: 0, scroll: 0, scrolled: 0, anchor: None, view: Rect::default(), top: 0, sel: None, dragging: false, toast: None,
              ended_at: None, drawn_look: ("", 0), streams: Default::default(),
              main: first_choice(&agents), queue: VecDeque::new(), pick: 0, choice: 0, ended: Vec::new(),
              last_key: Instant::now(), last: 0, quit_armed: false, quit: false, restart: false, updated: None, staged: None, setup: false, label, agents, theme,
              started: Instant::now(), asked: None, pending: None, update: false, releasing: None, release: None, plans: Vec::new(), listing: None, limits: Vec::new(),
              limits_at: None, spoke: false, pointed: false, checking: None, talking: None,
              revision: None, revision_id: None, quota: None, reviewer: None }
    }

    /// What the shell opens with: only what this session needs said. An empty conversation shows where to start
    /// (see `splash`); the rest is in /usrman.
    fn welcome(&mut self) {
        if let Some(why) = &self.s.no_undo {
            self.say(format!("  /undo is off here: {why}."));
        }
        // Started in the home folder (or above it), a turn has no /undo and no project's instructions, and spends
        // its first steps finding the project it was asked about: one turn read 3.9M tokens that way.
        if self.s.at_home {
            self.say("  Ringo is open in your home folder (or a folder above it). For work on a project, quit (Ctrl-C \
                      twice), cd into the project and \
                      start ringo there: /undo works, the project's own instructions load, and the agent does not \
                      have to search for it.");
        }
    }

    /// The lines /usrman starts with: where Ringo is, who works here, and how to begin.
    fn intro(&self) -> String {
        let found: Vec<&str> = self.agents.iter().map(|a| a.name()).collect();
        let undo = match &self.s.no_undo {
            None => "; /undo takes back its last change.".to_string(),
            Some(why) => format!(". /undo is off here: {why}."),
        };
        format!("  Ringo in {} · agents {} (/setup adds another) · {} mode (Shift-Tab changes it)\n  \
                 Type what you want done, in your own words, and press Enter. {} works in this folder and asks before \
                 it runs a command{undo}\n  @codex or @claude picks who you talk to · / lists the commands · ? the keys",
                home_short(&self.s.repo), found.join(", "), self.s.mode.name(), self.main.name())
    }

    /// An empty conversation's middle: the cat (unless the side panel shows it), the name, and where to start.
    fn splash(&self, panel: bool) -> Vec<Line<'static>> {
        let t = &self.theme;
        let mut rows: Vec<Line<'static>> = Vec::new();
        if !panel && let Some(cat) = mascot("idle").first() {
            rows.extend(cat.iter().map(|r| Line::styled(r.to_string(), t.brand)));
        }
        rows.push(Line::from(vec![Span::styled("▲▲ ", t.brand), Span::styled("ringo", t.brand.add_modifier(Modifier::BOLD))]));
        rows.push(Line::default());
        rows.push(Line::styled("Tell Ringo what you want done, in your own words.", t.title));
        rows.push(Line::styled("/usrman how it works · / commands · ? keys", t.dim));
        rows
    }

    fn say(&mut self, text: impl AsRef<str>) {
        for line in clean(text.as_ref()).lines() {
            self.out.push(Out::text(line));
        }
        self.last = 0;
    }

    /// A blank line, unless the conversation just had one.
    fn gap(&mut self) {
        if self.out.last().is_some_and(|o| !o.blank()) {
            self.out.push(Out::Line(Line::default(), 0));
        }
    }

    /// The owner's own line in the conversation, after a blank line.
    fn echo(&mut self, line: &str) {
        self.gap();
        // the speaker as a chip in its colour, as each agent's turn starts with one
        for (i, l) in clean(line).lines().enumerate() {
            let label = match i {
                0 => vec![Span::styled(" you ", self.theme.focus.add_modifier(Modifier::BOLD | Modifier::REVERSED)), Span::raw(" ")],
                _ => vec![Span::raw("      ")],
            };
            self.out.push(Out::Line(Line::from([label, vec![Span::styled(l.to_string(), self.theme.selected)]].concat()), 6));
        }
        self.last = 0;
        self.spoke = true;
    }

    fn note(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    /// The settings in force, printed under the manual.
    fn settings(&self) -> String {
        let chosen: Vec<String> = self.agents.iter().filter_map(|a| Some(format!("{} {}", a.name(), self.s.models.get(a)?))).collect();
        let models = if chosen.is_empty() { "each agent's own".to_string() } else { chosen.join(", ") };
        format!("  now: mode {} · tier {} · talking to {} · agents {} · models {models} (/model) · questions and pipelines on {} · \
                 theme {}\n  private state: {}",
                self.s.mode.name(), self.s.tier.name(), self.main.name(), if self.s.separate { "separate" } else { "together" },
                self.s.backend, self.theme.name, self.s.dir.display())
    }

    /// An agent's reply, or the manual: markdown, between blank lines.
    fn reply(&mut self, text: &str) {
        self.gap();
        self.out.push(Out::Reply(clean(text)));
        self.gap();
    }

    fn agent_style(&self, agent: Agent) -> Style {
        match agent.name() {
            "codex" => self.theme.codex,
            "claude" => self.theme.claude,
            _ => self.theme.grok, // every other agent shares the third colour
        }
    }

    /// Who runs a job, as the shell names it, in that agent's colour.
    fn who(&self, job: &ringo_cli::Job) -> (String, Style) {
        match &job.kind {
            Kind::Agent { agent, write } => (format!("{}{}", agent.name(), if *write { " (own copy)" } else { "" }),
                                             self.agent_style(*agent)),
            Kind::Ringo { command } => (format!("ringo {command}"), self.theme.ringo),
        }
    }

    /// Queues every job's new log lines for the conversation, each run of one job's lines under its name, and one
    /// line when a job ends.
    fn feed(&mut self) {
        let mut ended = Vec::new();
        for i in 0..self.s.jobs.len() {
            if self.ended.len() == i {
                self.ended.push(false);
            }
            let fresh = std::mem::take(&mut self.s.jobs[i].fresh);
            let (id, ends) = (self.s.jobs[i].id, !self.s.jobs[i].running() && !self.ended[i]);
            let (who, style) = self.who(&self.s.jobs[i]);
            if !fresh.is_empty() && self.last != id {
                self.gap();
                // with the model at work, as its agent reports it: which model does what is on screen, not assumed
                let model = self.s.jobs[i].model.as_ref().map_or(String::new(), |m| format!(" · {m}"));
                self.out.push(Out::Line(Line::from(vec![Span::styled(format!(" {who} "), style.add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                                                        Span::styled(format!(" #{id}{model}"), self.theme.dim)]), 0));
            }
            for said in fresh {
                // The owner answers what they read last: an agent's words in this folder make it the one plain lines
                // go to, unless a line is being typed (it keeps the agent its prompt shows).
                if matches!(said, Said::Partial(_) | Said::Reply(_)) && self.input.is_empty()
                    && let Kind::Agent { agent, write: false } = self.s.jobs[i].kind {
                    self.main = agent;
                }
                // anything but another piece means the reply's pieces have stopped for now: what was kept back shows
                if !matches!(said, Said::Partial(_) | Said::Reply(_)) {
                    self.show_stream(id, Duration::MAX);
                }
                match said {
                    // A piece of the reply grows the job's open reply, or opens one; the whole reply replaces it.
                    // The growing reply is fitted again (its markdown parsed) for each frame it grows in: 0.1 to
                    // 0.4 ms a frame up to the 14.8k guide, about 1 ms at the end of a 56k reply (measured, release).
                    Said::Partial(text) => {
                        match self.streams.get_mut(&id) {
                            Some(s) if s.at < self.out.len() => {
                                s.raw.push_str(&text);
                                s.last = Instant::now();
                            }
                            _ => {
                                self.gap();
                                self.out.push(Out::Reply(String::new()));
                                self.streams.insert(id, Stream { at: self.out.len() - 1, raw: text, last: Instant::now(),
                                                                 floor: 0, due: None });
                            }
                        }
                        self.show_stream(id, Duration::ZERO);
                    }
                    Said::Reply(text) => match self.streams.remove(&id) {
                        Some(s) if s.at < self.out.len() => {
                            self.out[s.at] = Out::Reply(clean(&text));
                            self.fitted.truncate(s.at);
                            self.gap();
                        }
                        _ => self.reply(&text),
                    },
                    Said::Step(text) => {
                        self.out.push(Out::Step(clean(&text)));
                    }
                    // a run of reads and searches is one row, counting up as the agent looks
                    Said::Looked(kind, _) => {
                        let at = ["read", "search", "list"].iter().position(|k| *k == kind).unwrap_or(0);
                        match self.out.last_mut() {
                            Some(Out::Looked(n)) if self.last == id => {
                                n[at] += 1;
                                self.fitted.truncate(self.out.len() - 1);
                            }
                            _ => {
                                let mut n = [0; 3];
                                n[at] = 1;
                                self.out.push(Out::Looked(n));
                            }
                        }
                    }
                    Said::Line(text) => {
                        for line in clean(&text).lines() {
                            self.out.push(Out::text(&format!("  {line}")));
                        }
                    }
                }
                self.last = id;
            }
            if ends {
                self.show_stream(id, Duration::MAX); // a reply cut off stays as far as it got, all of it
                self.streams.remove(&id);
                self.ended_at = match self.s.jobs[i].status {
                    Status::Done => Some((true, Instant::now())),
                    Status::Failed(_) => Some((false, Instant::now())),
                    _ => None,
                };
                // a turn of the agents' talk that ended well has no row of its own: the talk's last row counts them all
                let lines = match (self.s.jobs[i].talk, &self.s.jobs[i].status) {
                    (true, Status::Done) => Vec::new(),
                    _ => self.job_end(&self.s.jobs[i]),
                };
                self.out.extend(lines.into_iter().map(|l| Out::Line(l, 2)));
                self.ended[i] = true;
                self.last = 0;
                ended.push(id);
            }
        }
        let any = !ended.is_empty();
        for id in ended {
            self.after_end(id);
        }
        if any {
            self.keep();
        }
    }

    /// Shows job `id`'s reply being written as it stands `age` after its last piece (see `steady`). True when what
    /// shows changed.
    fn show_stream(&mut self, id: usize, age: Duration) -> bool {
        let Some(s) = self.streams.get_mut(&id).filter(|s| s.at < self.out.len()) else { return false };
        let (text, kept, again) = steady(&clean(&s.raw), age, s.floor);
        s.floor = s.floor.max(kept);
        s.due = again.map(|after| s.last + after);
        if matches!(&self.out[s.at], Out::Reply(shown) if *shown == text) {
            return false;
        }
        self.out[s.at] = Out::Reply(text);
        self.fitted.truncate(s.at);
        true
    }

    /// What replies being written kept back shows once their pieces pause (the word after WORD_HOLD, a table after
    /// TABLE_HOLD), so the end of a sentence never waits on the next one. True when a row changed.
    fn settle(&mut self) -> bool {
        let now = Instant::now();
        let due: Vec<(usize, Duration)> = self.streams.iter().filter(|(_, s)| s.due.is_some_and(|at| at <= now))
            .map(|(id, s)| (*id, now - s.last)).collect();
        due.into_iter().fold(false, |changed, (id, age)| self.show_stream(id, age) | changed)
    }

    /// Keeps this conversation for `ringo --resume` (see `Session::keep`), its text with it.
    fn keep(&mut self) -> Option<PathBuf> {
        let text = self.save();
        match self.s.keep(text.as_deref()) {
            Ok(path) => Some(path),
            Err(e) => {
                self.say(format!("  this conversation could not be kept for ringo --resume: {e}"));
                None
            }
        }
    }

    /// A job's last line: how it ended, its time, tokens and Ringo model calls, its change, and any same-files warning.
    fn job_end(&self, job: &ringo_cli::Job) -> Vec<Line<'static>> {
        let t = &self.theme;
        let (mark, style, what) = match &job.status {
            Status::Done => ("✓", t.ok, "done"),
            Status::Failed(_) => ("✗", t.fail, "failed"),
            Status::Cancelled => ("■", t.cancelled, "stopped"),
            Status::Running => return Vec::new(),
        };
        let calls = match job.calls { 0 => String::new(), 1 => " · 1 call".into(), n => format!(" · {n} calls") };
        let mut tail = format!(" · {} · {} tokens{calls}", fmt_age(job.age()), fmt_tokens(job.tokens_so_far()));
        if job.unchanged {
            tail += &format!(" · no change; /drop {} removes its copy", job.id);
        } else if job.worktree.is_some() && job.status == Status::Done {
            tail += &format!(" · change ready: /diff {0} shows it, /apply {0} keeps it", job.id);
        } else if !job.changed.is_empty() {
            let files = match job.changed.len() {
                n if n <= 3 => job.changed.join(", "),
                n => format!("{} and {} more", job.changed[..2].join(", "), n - 2),
            };
            tail += &format!(" · changed {files} (/undo takes it back)");
        }
        // the job's own header above names it
        let mut lines = vec![Line::from(vec![Span::styled("── ", t.border), Span::styled(format!("{mark} {what}"), style),
                                             Span::styled(tail, t.dim)])];
        let overlaps: Vec<String> = self.s.overlaps(job.id).into_iter()
            .map(|(other, files)| format!("#{other} {}", files.join(", "))).collect();
        if !overlaps.is_empty() {
            lines.push(Line::styled(format!("  ⚠ same files as {}", overlaps.join("; ")), t.warn));
        }
        // a plan that does not include the model chosen fails the turn: say where the choice came from
        if matches!(job.status, Status::Failed(_)) && let Kind::Agent { agent, .. } = job.kind
            && let Some(model) = self.s.models.get(&agent) {
            lines.push(Line::styled(format!("  it ran on {model}, your choice for {0}: /model {0} default goes back to its own", agent.name()), t.dim));
        }
        lines
    }

    /// Sets the mode for the turns started from now on (a running turn keeps its own), and saves it.
    fn set_mode(&mut self, mode: Mode) -> String {
        self.s.mode = mode;
        let saved = crate::theme::set_setting("mode", serde_json::json!(mode.name())).err()
            .map_or(String::new(), |e| format!(" (not saved: {e})"));
        let what = match mode {
            Mode::Ask => "agents edit files here (/undo takes it back) and ask before commands, the network, pushes and PRs",
            Mode::Auto => "agents act on their own; flagged actions and always-ask rules still wait for you",
            Mode::Read => "agents look but do not touch",
        };
        format!("  mode: {}: {what}; a turn already running keeps its own{saved}", mode.name())
    }

    /// Starts the task an /ask call routed (`ROUTE: <route>`): code turns go to the agent the owner talks to, sources
    /// to /research; a review needs its range, so it is put in the input line to complete.
    fn start_routed(&mut self, ask: usize, route: &str) {
        let line = self.s.job(ask).map(|j| j.title_full.clone()).unwrap_or_default();
        match route {
            "edit" | "read" => {
                let main = self.main;
                match self.talk(main, line.clone(), line) {
                    Ok(_) => self.say(format!("  → #{ask} is a task for the files here: {} takes it", main.name())),
                    Err(e) => self.say(format!("  → #{ask} is a task for {}: {e}", main.name())),
                }
            }
            "research" => match self.s.research(&line) {
                Ok(id) => self.say(format!("  → #{ask} is a task for live sources: started /research as #{id}")),
                Err(e) => self.say(format!("  → #{ask} needs live sources, but /research did not start: {e}")),
            },
            "review" => {
                self.say(format!("  → #{ask} asks for a review: say what to review (Enter sends it)"));
                if self.input.is_empty() {
                    self.input = "/review HEAD~1".chars().collect();
                    self.cursor = self.input.len();
                }
            }
            other => self.say(format!("  → #{ask} came back as route `{other}`, which Ringo does not know; nothing started")),
        }
    }

    fn spinner(&self) -> &'static str {
        SPIN[(self.started.elapsed().as_millis() / SPIN_MS) as usize % SPIN.len()]
    }

    /// /discuss: the agents talk a topic over among themselves, in the conversation they share. One speaks at a
    /// time, and each hears what the others said since it last spoke (Ringo's record, as for any turn it was away
    /// for), so nothing is sent twice. Its turns look and do not touch, whatever the mode. It costs a turn of each
    /// agent's a round, and ends when a round adds nothing or after TALK_ROUNDS rounds.
    fn discuss(&mut self, named: Vec<Agent>, topic: String) -> Result<Option<String>, String> {
        if self.talking.is_some() {
            return Err("the agents are talking already: Esc stops them, and a line of yours joins in".into());
        }
        if self.s.separate {
            return Err("the agents work separately here, so none hears the others: /together first".into());
        }
        let mut order: Vec<Agent> = if named.is_empty() { self.agents.iter().copied().filter(|a| a.spec().read).collect() } else { Vec::new() };
        for agent in named {
            if !self.agents.contains(&agent) {
                return Err(format!("{} is not set up here; /setup adds it", agent.name()));
            }
            if !agent.spec().read {
                return Err(format!("{0} works only in its own copy of the folder, where it hears no one: a talk is for agents \
                                    that work here", agent.name()));
            }
            if !order.contains(&agent) {
                order.push(agent);
            }
        }
        if order.len() < 2 {
            return Err(format!("a talk takes two agents that work in this folder, and here there is {}: /setup adds another, \
                                /agent add makes a specialist of your own",
                               order.first().map_or("none".to_string(), |a| format!("only {}", a.name()))));
        }
        if topic.is_empty() && !order.iter().any(|a| self.s.has_conversation(*a)) {
            return Err("say what they should talk over: /discuss TOPIC (alone, it is the last turn, and there is none yet)".into());
        }
        // The agent the owner talks to opens a topic. With none it is the last turn they talk over, which that
        // agent took, so the others answer it first.
        let main = order.iter().position(|a| *a == self.main);
        let opens = match (main, topic.is_empty()) {
            (Some(at), false) => at,
            (Some(at), true) => (at + 1) % order.len(),
            (None, _) => 0,
        };
        order.rotate_left(opens);
        let first = order[0];
        if self.must_wait(first) {
            return Err(format!("{} is at work here: the talk can start when its turn ends (Esc stops it)", first.name()));
        }
        let names: Vec<&str> = order.iter().map(|a| a.name()).collect();
        let others = listed(&names[1..]);
        // Said outright that the replies are carried: asked only to "talk it over with codex", Claude went looking
        // for a way to run Codex itself (live, 11/10: 1:46 and 155k tokens for two turns, and no view of its own).
        let prompt = match topic.is_empty() {
            false => format!("I want you and {others} to talk this over; I am reading along: {topic}\n\n{TALK_RELAY} You \
                              speak first. Say what you think and what you would do, in a few short paragraphs. Look at the \
                              files if it helps; change nothing."),
            true => format!("I want you and {others} to talk the last turn above over; I am reading along.\n\n{TALK_RELAY} \
                             You speak first. Say where you agree with it, where you differ and why, and what you would do, \
                             in a few short paragraphs. Look at the files if it helps; change nothing."),
        };
        let job = self.talk_turn(first, &prompt)?;
        let limit = order.len() * TALK_ROUNDS;
        self.talking = Some(Talk { order, at: 0, job, jobs: vec![job], limit, passes: 0, said: Vec::new(), started: Instant::now() });
        Ok(Some(format!("  {} talk it over, one at a time, looking but not touching · at most {TALK_ROUNDS} turns each · a line \
                         of yours joins in · Esc stops them", listed(&names))))
    }

    /// One turn of the agents' talk: in read mode whatever the session's (a running turn keeps its own), its reply
    /// heard whole by the others. What it was told is in its log (/show N), not in the conversation.
    fn talk_turn(&mut self, agent: Agent, prompt: &str) -> Result<usize, String> {
        let prompt = self.s.expand(prompt)?;
        let mode = std::mem::replace(&mut self.s.mode, Mode::Read);
        let started = self.s.launch_agent(agent, false, &prompt);
        self.s.mode = mode;
        let id = started?;
        let job = &mut self.s.jobs[id - 1];
        job.talk = true;
        job.fresh.retain(|said| !matches!(said, Said::Line(_)));
        Ok(id)
    }

    /// The talk after its turn `id` ended: the next agent answers, or the talk is over (a turn that failed or was
    /// stopped, a round that added nothing, every round taken, or the next agent at work on something else).
    fn talk_on(&mut self, id: usize) {
        let Some(mut talk) = self.talking.take() else { return };
        let job = &self.s.jobs[id - 1];
        let (done, speaker) = (job.status == Status::Done, talk.order[talk.at]);
        talk.passes = if done && agreed(&job.reply()) { talk.passes + 1 } else { 0 };
        let rounds = talk.limit / talk.order.len();
        let mut over = match &job.status {
            Status::Done if !talk.said.is_empty() => None, // the owner spoke: the next one answers them
            Status::Done if talk.passes + 1 >= talk.order.len() => Some("no one has more to add".to_string()),
            Status::Done if talk.jobs.len() >= talk.limit =>
                Some(format!("each has spoken {}", match rounds { 1 => "once".into(), 2 => "twice".into(), n => format!("{n} times") })),
            Status::Done => None,
            Status::Cancelled => Some("stopped".into()),
            _ => Some(format!("{}'s turn failed", speaker.name())),
        };
        if over.is_none() {
            talk.at = (talk.at + 1) % talk.order.len();
            let next = talk.order[talk.at];
            let others: Vec<&str> = talk.order.iter().filter(|a| **a != next).map(|a| a.name()).collect();
            let said = std::mem::take(&mut talk.said);
            let adds = if said.is_empty() { String::new() } else { format!("I add: {}\n\n", said.join("\n")) };
            let prompt = format!("{adds}Go on with the talk: answer what {} said above{}. Write only your own part: where \
                                  you agree, where you differ and why, what you would do, in a few short paragraphs; change \
                                  nothing. If you have nothing to add, reply with the one word AGREED.", listed(&others),
                                 if said.is_empty() { "" } else { ", and what I add" });
            let started = match self.must_wait(next) {
                true => Err(format!("{} is at work on something else", next.name())),
                false => self.talk_turn(next, &prompt),
            };
            match started {
                Ok(job) => {
                    talk.job = job;
                    talk.jobs.push(job);
                    if !said.is_empty() {
                        talk.passes = 0;
                    }
                }
                Err(why) => over = Some(why),
            }
        }
        let Some(why) = over else {
            self.talking = Some(talk);
            return;
        };
        let t = self.theme;
        let tokens: u64 = talk.jobs.iter().map(|id| self.s.jobs[id - 1].tokens_so_far()).sum();
        let turns = match talk.jobs.len() { 1 => "1 turn".to_string(), n => format!("{n} turns") };
        self.gap();
        self.out.push(Out::Line(Line::from(vec![
            Span::styled("── ", t.border), Span::styled(format!("the talk is over: {why}"), t.ok),
            Span::styled(format!(" · {turns} · {} · {} tokens · /discuss goes on with it", fmt_age(talk.started.elapsed()),
                                 fmt_tokens(tokens)), t.dim)]), 2));
        self.last = 0;
    }

    /// A plain line of the owner's while the agents talk: the next one to speak hears it, and each may answer once
    /// more.
    fn join_talk(&mut self, line: String) -> String {
        let Some(talk) = &mut self.talking else { return String::new() };
        talk.said.push(line);
        talk.limit = talk.limit.max(talk.jobs.len() + talk.order.len());
        let next = talk.order[(talk.at + 1) % talk.order.len()];
        format!("  ↳ {} hears this next, and the talk goes on", next.name())
    }

    /// A line for `agent` in this folder, continuing their conversation: now, or next while a turn runs here (as it
    /// was typed, `line`, it shows when it starts). An agent that works only in its own copy takes it there.
    fn talk(&mut self, agent: Agent, task: String, line: String) -> Result<Option<String>, String> {
        if !agent.spec().read {
            let expanded = self.s.expand(&task)?;
            let id = self.s.launch_agent(agent, true, &expanded)?;
            return Ok(Some(format!("  #{id}: {} works only in its own copy; /apply {id} keeps its change", agent.name())));
        }
        if self.must_wait(agent) {
            // A line for the agent working here goes into its turn, which reads it at its next step, as Claude Code
            // and Codex take a message typed while they work. Behind a line already waiting for it, or for a turn
            // that cannot take one (agy, /compact), it waits for the next turn.
            let working = self.s.busy_for(agent).filter(|&id| self.s.jobs[id - 1].kind == Kind::Agent { agent, write: false });
            if let Some(id) = working && task != "/compact" && !self.queue.iter().any(|(a, ..)| *a == agent) {
                let expanded = self.s.expand(&task)?;
                if self.s.steer(id, &expanded) {
                    self.echo(&line);
                    return Ok(Some(format!("  ↳ #{id} takes this as it works")));
                }
            }
            self.queue.push_back((agent, task, line));
            return Ok(None);
        }
        self.turn(agent, &task)
    }

    /// Whether `agent`'s turn in this folder has to wait: one of its own runs here, a line of its own waits before
    /// it, every job slot is taken, or an agent update holds the session. Other agents' turns do not hold it.
    fn must_wait(&self, agent: Agent) -> bool {
        self.s.busy_for(agent).is_some() || self.queue.iter().any(|(a, ..)| *a == agent)
            || self.s.running() >= self.s.max_jobs || self.s.hold.is_some() || self.checking.is_some()
    }

    /// Starts the oldest waiting line whose agent is free (each agent's lines go in order); true when it did.
    fn dispatch(&mut self) -> bool {
        if self.s.running() >= self.s.max_jobs || self.s.hold.is_some() || self.checking.is_some() {
            return false;
        }
        let free = (0..self.queue.len()).find(|&i| {
            let agent = self.queue[i].0;
            self.s.busy_for(agent).is_none() && !self.queue.iter().take(i).any(|(a, ..)| *a == agent)
        });
        let Some((agent, task, line)) = free.and_then(|i| self.queue.remove(i)) else { return false };
        let revision = self.revision.as_deref() == Some(task.as_str());
        if !revision {
            self.echo(&line); // the revision is Ringo's, said by after_end, not the owner's
        }
        match self.turn(agent, &task) {
            Ok(_) if revision => self.revision_id = Some(self.s.jobs.len()), // its handoff is the review shown above
            Ok(Some(text)) => self.say(text),
            Ok(None) => {}
            Err(e) => self.say(format!("  {e}")),
        }
        if revision {
            self.revision = None;
        }
        true
    }

    /// Ultra, after a job ends: a turn here that changed files gets Ringo's review (the change's diff, its files and
    /// the task, one Codex call); a review that finds defects gives the same agent one turn to fix them. Nothing
    /// follows that revision, so a turn costs at most one review and one more turn.
    fn after_end(&mut self, id: usize) {
        // the side panel's gauges, read again after a turn when the last read is ten minutes old
        // ceiling: a plan used up mid-turn shows at the next turn's end; /usage reads it at once
        if self.limits_at.is_some_and(|at| at.elapsed() > LIMITS_FRESH) && self.plans.is_empty() {
            let asked = self.plan_agents();
            self.read_limits(&asked, false);
        }
        if self.talking.as_ref().is_some_and(|talk| talk.job == id) {
            return self.talk_on(id);
        }
        let job = &self.s.jobs[id - 1];
        // From the home folder, a turn whose edits all went to one project points there, once (11/03: a whole project
        // was built from the home folder, past the opening line that says to start in the project)
        if self.s.at_home && !self.pointed && matches!(job.kind, Kind::Agent { write: false, .. })
            && let Some(project) = one_project(&self.s.repo, &job.touched) {
            self.pointed = true;
            let at = home_short(&project);
            self.say(format!("  #{id}'s edits are all in {at}. Started there, /undo works and the project's own \
                              instructions load: quit (Ctrl-C twice), then cd {at} && ringo"));
        }
        let job = &self.s.jobs[id - 1];
        if let Some((review, turn, agent)) = self.checking.filter(|(review, ..)| *review == id) {
            self.checking = None;
            let found = ringo_cli::rt::pipelines::findings("f", &job.reply()).1.len();
            match (&job.status, found) {
                (Status::Done, 0) => self.say(format!("  ✓ review #{review} found no defects in #{turn}")),
                (Status::Done, n) => {
                    let task = format!("Ringo's review #{review} of your change in #{turn} found {n} defect{}. Fix the \
                                        ones that are real, here in this folder, and say for each what you did; if one \
                                        is wrong, say why and leave it: {{#{review}}}", if n == 1 { "" } else { "s" });
                    self.say(format!("  ultra: {} gets one turn to fix what review #{review} found", agent.name()));
                    self.revision = Some(task.clone());
                    self.queue.push_front((agent, task.clone(), task));
                }
                _ => self.say(format!("  review #{review} did not finish, so #{turn}'s change stands unreviewed")),
            }
            return;
        }
        let (Kind::Agent { agent, write: false }, Status::Done) = (&job.kind, &job.status) else { return };
        let agent = *agent;
        if job.tier != Tier::Ultra || job.changed.is_empty() || self.revision_id == Some(id) {
            return;
        }
        if let Some((review, ..)) = self.checking {
            self.say(format!("  ultra: #{id} ended while review #{review} runs; /review #{id} checks it"));
            return;
        }
        self.quota_done();
        // ceiling: a turn that ends before its plan read answers (a read takes about a second, a turn that edits
        // several) is reviewed on /backend unchecked.
        let (backend, why) = match self.reviewer.take_if(|(turn, _)| *turn == id) {
            Some((_, Ok(chosen))) => chosen,
            Some((_, Err(why))) => {
                self.say(format!("  ultra: no review of #{id}: {why}"));
                return;
            }
            None => (self.s.backend.clone(), String::new()),
        };
        match self.s.review_job(id, &backend) {
            Ok(review) => {
                self.checking = Some((review, id, agent));
                let on = if why.is_empty() { String::new() } else { format!(" on {backend}, since {why}") };
                self.say(format!("  ultra: Ringo reviews #{id}'s change as #{review}{on}; lines you send wait for it (Esc stops it)"));
            }
            Err(e) => self.say(format!("  ultra: no review of #{id}: {e}")),
        }
    }

    /// `/tier`: the effort the turns started from now on run at; it lasts for this session.
    fn set_tier(&mut self, tier: Tier) -> String {
        self.s.tier = tier;
        let what = match tier {
            Tier::Quick => "agents think less (low effort): fewer tokens, for small or plain tasks",
            Tier::Standard => "each agent's own default",
            Tier::Deep => "agents think more (xhigh effort): slower, and more tokens",
            Tier::Ultra => "xhigh effort, then Ringo reviews each change made here (one call, about 70k tokens, on \
                            Codex or, when its plan is out, Claude) and the agent gets one turn to fix what the review \
                            finds",
        };
        let mut text = format!("  tier: {}: {what}; for this session (Ringo starts in standard)", tier.name());
        if tier == Tier::Ultra && let Some(why) = &self.s.no_undo {
            text.push_str(&format!("\n  here Ringo cannot see what a turn changes ({why}), so there is nothing to review"));
        }
        text
    }

    /// `/usage`: the tokens that went through Ringo, a gauge an agent (this session, and the last day and week from
    /// Ringo's record), then each plan's limits as gauges, asked of the agents in the background (about a second;
    /// no model call).
    fn usage(&mut self) -> Option<String> {
        use std::collections::BTreeMap;
        let mut session: BTreeMap<String, u64> = BTreeMap::new();
        for job in &self.s.jobs {
            let who = match job.kind { Kind::Agent { agent, .. } => agent.name().to_string(), Kind::Ringo { .. } => "ringo".into() };
            *session.entry(who).or_default() += job.tokens_so_far();
        }
        let sums = |m: BTreeMap<String, ringo_cli::parse::Tokens>| -> BTreeMap<String, u64> {
            m.into_iter().map(|(k, t)| (k, t.total())).collect()
        };
        let (day, first) = self.s.used_since(86_400);
        let (day, week) = (sums(day), sums(self.s.used_since(7 * 86_400).0));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let kept = match first {
            Some(at) if now.saturating_sub(at) < 7 * 86_400 => format!(" (Ringo has kept this record for {})", fmt_until(now - at)),
            Some(_) => String::new(),
            None => " (Ringo keeps this record from now on)".into(),
        };
        let total = |m: &BTreeMap<String, u64>| fmt_tokens(m.values().sum());
        let t = self.theme;
        self.gap();
        self.out.push(Out::Line(Line::from(vec![
            Span::styled("  tokens through Ringo", t.title.add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · this session {} · last 24 hours {} · last 7 days {}{kept}", total(&session),
                                 total(&day), total(&week)), t.dim)]), 4));
        // a gauge an agent: its share of the week's tokens, in its own colour
        let mut names: Vec<String> = self.agents.iter().map(|a| a.name().to_string()).collect();
        for name in week.keys().chain(session.keys()) {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let all = week.values().sum::<u64>().max(1);
        let pad = names.iter().map(|n| cols(n)).max().unwrap_or(0);
        for name in &names {
            let got = |m: &BTreeMap<String, u64>| m.get(name).copied().unwrap_or(0);
            let fill = self.agents.iter().find(|a| a.name() == name.as_str()).map(|a| self.agent_style(*a)).unwrap_or(t.ringo);
            self.out.push(Out::Meter { label: name.clone(), pad, percent: got(&week) as f64 * 100.0 / all as f64,
                                       value: fmt_tokens(got(&week)), fill: Some(fill),
                                       note: format!("in 7 days · {} in 24 hours · {} this session", fmt_tokens(got(&day)),
                                                     fmt_tokens(got(&session))) });
        }
        let asked = self.plan_agents();
        let others: Vec<Agent> = self.agents.iter().copied().filter(|a| self.s.installed(*a) && !asked.contains(a)).collect();
        for agent in others {
            self.out.push(agent_head(agent.name(), self.agent_style(agent), " · its plan's limits cannot be read", &t));
        }
        if asked.is_empty() {
            return None;
        }
        self.read_limits(&asked, true);
        Some(format!("  asking {} for your plan's limits…", asked.iter().map(|a| a.name()).collect::<Vec<_>>().join(" and ")))
    }

    /// The agents set up here whose plan's limits can be read: Claude's and Codex's.
    fn plan_agents(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = self.agents.iter().copied()
            .filter(|a| matches!(a.spec().adapter, ringo_cli::Adapter::Claude | ringo_cli::Adapter::Codex) && self.s.installed(*a))
            .collect();
        // a specialist of the owner's runs on the plan of the agent it is made from: one read, one gauge, under
        // that agent's name
        agents.sort_by_key(|a| a.spec().custom);
        let mut seen = Vec::new();
        agents.retain(|a| {
            let program = self.s.program(*a);
            let new = !seen.contains(&program);
            seen.push(program);
            new
        });
        agents
    }

    /// The agent files Claude Code would find from here: this folder's, then the owner's own.
    fn claude_agent_files(&self) -> Vec<PathBuf> {
        [self.s.repo.join(".claude/agents"), self.s.home.join(".claude/agents")].iter().flat_map(|dir| {
            let mut found: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md") && p.is_file()).collect();
            found.sort();
            found
        }).collect()
    }

    /// /agent: the owner's own agents, then the agents they made for Claude Code that are not agents here yet.
    fn own_agents(&mut self) {
        let t = self.theme;
        self.gap();
        self.out.push(Out::Line(Line::from(vec![
            Span::styled("  your own agents", t.title.add_modifier(Modifier::BOLD)),
            Span::styled(" · /agent add NAME FILE makes one · /agent remove NAME takes one away", t.dim)]), 4));
        let own: Vec<Agent> = Agent::all().into_iter().filter(|a| a.spec().custom).collect();
        let pad = own.iter().map(|a| cols(a.name())).max().unwrap_or(0);
        let mut used = Vec::new();
        for agent in &own {
            let spec = agent.spec();
            let on = match spec.adapter {
                ringo_cli::Adapter::Claude => "claude",
                ringo_cli::Adapter::Codex => "codex",
                ringo_cli::Adapter::Agy => "agy",
                ringo_cli::Adapter::Plain => "a program of its own",
            };
            let what = match (&spec.instructions, ringo_cli::instructions(spec)) {
                (Some(file), Ok(Some(text))) => {
                    let file = ringo_cli::home_path(file);
                    used.push(file.canonicalize().unwrap_or_else(|_| file.clone()));
                    format!("on {on} · {} · {} characters, about {} tokens more on each of its calls{}", home_short(&file),
                            text.len(), fmt_tokens(text.len() as u64 / 4),
                            if spec.skills { " · with your Claude Code skills" } else { "" })
                }
                (Some(_), Err(e)) => format!("on {on} · {e}"),
                _ => format!("on {on} · no instructions of its own"),
            };
            let here = if self.agents.contains(agent) { "" } else { " · not set up here (/setup)" };
            self.out.push(Out::Line(Line::from(vec![
                Span::raw("    "), Span::styled(format!("{:<pad$}", agent.name()), self.agent_style(*agent)),
                Span::styled(format!("  {what}{here}"), t.dim)]), 6 + pad));
        }
        let found: Vec<PathBuf> = self.claude_agent_files().into_iter()
            .filter(|p| !used.contains(&p.canonicalize().unwrap_or_else(|_| p.clone()))).collect();
        if own.is_empty() {
            self.out.push(Out::text("    none yet. A specialist is a file of instructions: what it knows and how it works. /agent add \
                                     NAME FILE makes it an agent here, on claude (or on codex: add the word), and @NAME talks to it"));
        }
        if !found.is_empty() {
            self.out.push(Out::Line(Line::styled("  made for Claude Code, and not agents here yet", t.dim), 4));
            for file in found {
                let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                self.out.push(Out::Line(Line::from(vec![
                    Span::raw(format!("    {name}")),
                    Span::styled(format!("  {} · /agent add {name}", home_short(&file)), t.dim)]), 6));
            }
        }
        self.last = 0;
    }

    /// /agent add: a specialist of the owner's, made from a file of instructions and an agent set up here.
    fn add_own_agent(&mut self, name: String, file: Option<String>, base: Option<String>, skills: bool) -> Result<String, String> {
        use ringo_cli::Adapter;
        if !ringo_cli::valid_name(&name) || name == "all" {
            return Err(format!("`{name}` cannot be an agent's name: use letters, digits, - _ or ."));
        }
        if let Some(taken) = Agent::from_name(&name) {
            return Err(match taken.spec().custom {
                true => format!("you already have an agent called {name}: /agent remove {name} and then /restart free the name"),
                false => format!("{name} is one of Ringo's own agents: give yours another name"),
            });
        }
        let runs = |a: &Agent| matches!(a.spec().adapter, Adapter::Claude | Adapter::Codex) && a.spec().instructions.is_none();
        let base = match base {
            Some(b) => Agent::from_name(&b).filter(|a| self.agents.contains(a) && runs(a))
                .ok_or(format!("an agent of your own runs on claude or codex, set up here, and {b} is not one"))?,
            None => self.agents.iter().copied().filter(runs).min_by_key(|a| a.spec().adapter != Adapter::Claude)
                .ok_or("an agent of your own runs on claude or codex, and neither is set up here: /setup adds one")?,
        };
        if skills && base.spec().adapter != Adapter::Claude {
            return Err(format!("skills are Claude Code's: on {} a specialist goes without them", base.name()));
        }
        // the file as given, else an agent made for Claude Code by that name (this folder's first, then the owner's)
        let wanted = file.unwrap_or_else(|| name.clone());
        let given = ringo_cli::home_path(&wanted);
        let given = match std::env::current_dir() {
            Ok(cwd) if given.is_relative() => cwd.join(given),
            _ => given,
        };
        let path = match given.is_file() {
            true => given,
            false => self.claude_agent_files().into_iter().find(|p| p.file_stem().is_some_and(|s| s.to_string_lossy() == wanted))
                .ok_or(format!("no instructions file `{wanted}`: give its path, or the name of an agent you made for Claude \
                                Code (/agent lists the ones found)"))?,
        };
        let path = path.canonicalize().unwrap_or(path);
        let adapter = if base.spec().adapter == Adapter::Claude { "claude" } else { "codex" };
        let entry = serde_json::json!({"name": name, "adapter": adapter, "program": base.spec().program, "read": true,
                                       "company": format!("your own, on {}", base.name()), "login": base.spec().login,
                                       "instructions": path, "skills": skills});
        let size = ringo_cli::instructions(&ringo_cli::Spec::from_json(&entry, true)?)?.map_or(0, |text| text.len());
        let agent = ringo_cli::add_custom_agent(self.s.state(), entry)?;
        if let Some(program) = self.s.programs.get(&base).cloned() {
            self.s.programs.insert(agent, program);
        }
        self.agents.push(agent);
        let saved = ringo_cli::save_agents_in(self.s.state(), &self.agents).err().map_or(String::new(), |e| format!(" (not saved: {e})"));
        Ok(format!("  {name} is an agent here now{saved}: {} with {} as its standing instructions ({size} characters, about {} \
                    tokens more on each of its calls){}.\n  @{name} talks to it. It keeps a conversation of its own and hears \
                    what the others do, as they hear it. /model {name} NAME picks its model, an edit to the file is in its \
                    next turn, and /agent remove {name} takes it away",
                   base.name(), home_short(&path), fmt_tokens(size as u64 / 4),
                   if skills { ", with your Claude Code skills, plugins and connectors loaded in its turns" } else { "" }))
    }

    /// /agent remove: the entry goes; the instructions file is the owner's and stays.
    fn remove_own_agent(&mut self, name: &str) -> Result<String, String> {
        let agent = Agent::from_name(name).filter(|a| a.spec().custom)
            .ok_or(format!("{name} is not an agent of your own (/agent lists them)"))?;
        if let Some(job) = self.s.jobs.iter().find(|j| j.running() && matches!(j.kind, Kind::Agent { agent: a, .. } if a == agent)) {
            return Err(format!("{name}'s #{0} is still working: wait for it, or /kill {0}", job.id));
        }
        if self.talking.as_ref().is_some_and(|talk| talk.order.contains(&agent)) {
            return Err(format!("{name} is in the talk under way: Esc stops the talk first"));
        }
        if self.agents == [agent] {
            return Err(format!("{name} is the only agent set up here: /setup adds another first"));
        }
        ringo_cli::remove_custom_agent(self.s.state(), name)?;
        self.agents.retain(|a| *a != agent);
        self.queue.retain(|(a, ..)| *a != agent);
        if self.main == agent && let Some(next) = self.agents.first() {
            self.main = *next;
        }
        Ok(format!("  {name} is gone from Ringo; its instructions file is untouched. The name is free again once Ringo \
                    restarts (/restart)"))
    }

    /// Reads each agent's plan in the background (about a second each; no model call); `said`: the answers go into
    /// the conversation, else only into the side panel's gauges.
    fn read_limits(&mut self, agents: &[Agent], said: bool) {
        for &agent in agents {
            let program = self.s.program(agent);
            self.plans.push((said, std::thread::spawn(move || (agent, ringo_cli::plan(agent, &program)))));
        }
        self.limits_at = Some(Instant::now());
    }

    /// `/update`'s other half: when a release runs the pipelines, this Ringo's own is checked against it in the
    /// background.
    /// A speed check times processes, so it starts only with no job running and holds new ones until it ends.
    fn check_pipelines(&mut self) -> Option<String> {
        if !crate::updates::releases_on() {
            return None;
        }
        if self.releasing.is_some() {
            return Some("  the pipelines' release is still being checked; it reports here when done".into());
        }
        if self.s.hold.is_some() || self.s.running() > 0 || self.checking.is_some() {
            return Some("  the pipelines' speed check waits until no job runs, so it does not share the machine \
                         with one: /update then".into());
        }
        self.s.hold = Some(CHECKING.into());
        self.releasing = Some(std::thread::spawn(|| crate::updates::check_release(true)));
        Some("  checking the pipelines' release against this Ringo: an offline speed check, no model calls, up to \
              3 minutes; jobs wait until it finishes".into())
    }

    /// The pipelines' check or approval, once it has answered.
    fn release_done(&mut self) -> bool {
        if !self.releasing.as_ref().is_some_and(|h| h.is_finished()) {
            return false;
        }
        let (line, ready) = self.releasing.take().and_then(|h| h.join().ok()).unwrap_or_default();
        if self.s.hold.as_deref() == Some(CHECKING) {
            self.s.hold = None;
        }
        if !line.is_empty() {
            self.say(line);
        }
        self.release = ready;
        true
    }

    /// The plan reads that have answered: kept for the side panel, and as gauges in the conversation for /usage.
    fn plans_done(&mut self) -> bool {
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.plans).into_iter().partition(|(_, h)| h.is_finished());
        self.plans = running;
        for (said, handle) in done {
            let Ok((agent, result)) = handle.join() else { continue };
            let (style, t) = (self.agent_style(agent), self.theme);
            // Claude's own answer is experimental: a turn's last rate-limit event, when there is one, stands in
            let (plan, why) = match result {
                Ok(plan) => (Some(plan), None),
                Err(e) => match self.s.claude_limits.as_ref().filter(|_| agent.spec().adapter == ringo_cli::Adapter::Claude) {
                    Some(info) => (Some(ringo_cli::claude_plan_from_event(info)),
                                   Some(format!("from this session's last claude turn; its own answer failed: {e}"))),
                    None => (None, Some(e)),
                },
            };
            if let Some(plan) = &plan {
                self.limits.retain(|(a, _)| *a != agent);
                self.limits.push((agent, plan.clone()));
                let order = |a: &Agent| self.agents.iter().position(|x| x == a);
                let mut limits = std::mem::take(&mut self.limits);
                limits.sort_by_key(|(a, _)| order(a));
                self.limits = limits;
            }
            if !said {
                continue;
            }
            match (plan, why) {
                (Some(plan), why) => {
                    self.out.extend(plan_rows(agent.name(), style, &plan, &t));
                    if let Some(why) = why {
                        self.out.push(Out::Line(Line::styled(format!("    {why}"), t.dim), 4));
                    }
                }
                (None, why) => self.out.push(agent_head(agent.name(), style, &format!(" · {}", why.unwrap_or_default()), &t)),
            }
            self.last = 0;
        }
        self.plans.is_empty()
    }

    /// /model's list of an agent's models, once it has answered.
    fn models_done(&mut self) -> bool {
        let Some((agent, read)) = self.listing.take_if(|(_, read)| read.is_finished()) else { return false };
        let (style, t) = (self.agent_style(agent), self.theme);
        match read.join().unwrap_or_else(|_| Err("the read stopped".into())) {
            Ok(models) if !models.is_empty() => {
                self.out.push(agent_head(agent.name(), style, &format!(" · the models your plan offers (/model {} NAME)", agent.name()), &t));
                let pad = models.iter().map(|m| cols(&m.name)).max().unwrap_or(0);
                for m in models {
                    let what = if m.default { format!("its default · {}", m.what) } else { m.what };
                    self.out.push(Out::Line(Line::from(vec![Span::raw(format!("    {:<pad$}  ", m.name)), Span::styled(what, t.dim)]), 6 + pad));
                }
            }
            Ok(_) => self.out.push(agent_head(agent.name(), style, " · it listed no models", &t)),
            Err(e) => self.out.push(agent_head(agent.name(), style, &format!(" · its models could not be read: {e}"), &t)),
        }
        self.last = 0;
        true
    }

    /// Ultra's plan read, once it has answered: which backend the turn's review will run on. When no plan has room,
    /// the owner hears it while the turn still runs, so they can stop it.
    fn quota_done(&mut self) -> bool {
        let Some((id, read)) = self.quota.take_if(|(_, read)| read.is_finished()) else { return false };
        let answer = read.join().unwrap_or_else(|_| Ok((self.s.backend.clone(), String::new())));
        if let Err(why) = &answer && self.s.job(id).is_ok_and(|j| j.running()) {
            self.say(format!("  ultra: {why}, so #{id}'s change will not be reviewed (Esc stops the turn)"));
        }
        self.reviewer = Some((id, answer));
        true
    }

    fn turn(&mut self, agent: Agent, task: &str) -> Result<Option<String>, String> {
        // checked again here: a queued /compact can come up after /clear took its conversation away
        if task == "/compact" && !self.s.has_conversation(agent) {
            return Err(format!("{} has no conversation here to compact", agent.name()));
        }
        let expanded = self.s.expand(task)?;
        let id = self.s.launch_agent(agent, false, &expanded)?;
        // Ultra: the plans are read while the turn runs (about a second, no model call), so its review can go to
        // a plan with room, and when none has any the owner hears it now, not after the turn.
        if self.s.tier == Tier::Ultra && self.s.mode != Mode::Read && self.s.undoable() && task != "/compact"
            && self.revision.as_deref() != Some(task) {
            let set_up: Vec<(Agent, PathBuf)> = self.agents.iter().filter(|a| self.s.installed(**a))
                .map(|a| (*a, self.s.program(*a))).collect();
            let (backend, limits) = (self.s.backend.clone(), self.s.claude_limits.clone());
            self.quota = Some((id, std::thread::spawn(move || {
                let read = |adapter| {
                    let (agent, program) = set_up.iter().find(|(a, _)| a.spec().adapter == adapter)?;
                    // Claude's own answer is experimental: this session's last rate-limit event stands in, as in /usage
                    Some(ringo_cli::plan(*agent, program).or_else(|e| match &limits {
                        Some(info) if adapter == ringo_cli::Adapter::Claude => Ok(ringo_cli::claude_plan_from_event(info)),
                        _ => Err(e),
                    }))
                };
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                reviewer(&backend, read, now)
            })));
        }
        Ok((expanded.len() != task.len()).then(|| {
            format!("  the handoff adds {} characters to the task:\n{expanded}", expanded.len() - task.len())
        }))
    }

    /// What holds this folder now, for Esc and Ctrl-C to stop: the turn running here, or ultra's review of one.
    fn holding(&self) -> Option<usize> {
        self.s.busy().or(self.checking.map(|(review, ..)| review))
    }

    /// Stops the turn running in this folder (or ultra's review of the last one); lines queued behind it go back into
    /// the input, to send or change. With a line being typed for another agent they are shown instead.
    fn stop(&mut self, id: usize) {
        let _ = self.s.kill(id);
        // the stopped agent's lines come back, the other agents' keep waiting for their own turns; ultra's review
        // held every line, so all of them come back when it stops
        let whose = self.s.job(id).ok().and_then(|j| match j.kind { Kind::Agent { agent, .. } => Some(agent), _ => None });
        let (back, keep): (VecDeque<_>, VecDeque<_>) = self.queue.drain(..).partition(|(a, ..)| whose.is_none_or(|w| *a == w));
        self.queue = keep;
        let agents: Vec<Agent> = back.iter().map(|(a, ..)| *a).collect();
        let queued: Vec<String> = back.into_iter().map(|(_, _, line)| line).collect();
        let mut unsent = Vec::new();
        if !queued.is_empty() {
            // Back to the agent they were for, though another may have spoken since (see `feed`). A plain line being
            // typed for that agent stays, and the waiting lines go in above it (they were dropped). Typed for another
            // agent, or as a command, the line is left as it is and the waiting lines are shown instead: joined to
            // it, they would go where it goes.
            let typed: String = self.input.iter().collect();
            let one = agents.first().copied().filter(|a| agents.iter().all(|b| b == a));
            let back = queued.join("\n");
            if typed.is_empty() {
                if let Some(agent) = one {
                    self.main = agent;
                }
                self.input = back.chars().collect();
                self.cursor = self.input.len();
            } else if one == Some(self.main) && matches!(parse(&typed, &self.agents), Ok(Cmd::Talk(_))) {
                self.cursor += back.chars().count() + 1; // where it was in the line being typed
                self.input = format!("{back}\n{typed}").chars().collect();
            } else {
                unsent = agents.into_iter().zip(queued).collect();
            }
        }
        match self.checking {
            Some((review, turn, _)) if review == id => self.say(format!("  stopping review #{id}; #{turn}'s change stays as it is")),
            _ => self.say(format!("  stopping #{id}; what it changed so far stays (/undo takes it back)")),
        }
        for (agent, line) in unsent {
            self.say(format!("  a line that waited for {} was not sent: {line}", agent.name()));
        }
    }

    /// Whether a line would wait in the queue: a line for an agent in this folder while a turn runs here.
    fn waits(&self, cmd: &Result<Cmd, String>) -> bool {
        // while the agents talk, a plain line joins in at once
        if self.talking.is_some() && matches!(cmd, Ok(Cmd::Talk(_))) {
            return false;
        }
        let agent = match cmd {
            Ok(Cmd::Talk(_) | Cmd::Compact) => Some(self.main),
            Ok(Cmd::Agents(agents, _)) => match agents[..] { [(a, false)] => Some(a), _ => None },
            _ => None,
        };
        agent.is_some_and(|a| a.spec().read && self.must_wait(a))
    }

    fn submit(&mut self) {
        let line: String = self.input.iter().collect();
        self.input.clear();
        self.cursor = 0;
        self.scroll = 0;
        if line.trim().is_empty() {
            return;
        }
        if self.history.last() != Some(&line) {
            save_history(&line);
            self.history.push(line.clone());
        }
        self.hist = self.history.len();
        // "/separate @agy …": the switch, then the rest as a line of its own
        let switch = ["/separate", "/together"].into_iter().find_map(|c| {
            line.trim().strip_prefix(c).filter(|rest| rest.starts_with(char::is_whitespace)).map(|rest| (c, rest.trim()))
        });
        match switch {
            Some((c, rest)) => {
                self.send(c);
                self.send(rest);
            }
            None => self.send(&line),
        }
    }

    /// One line from the input: a command, or a message for an agent.
    fn send(&mut self, line: &str) {
        let cmd = parse(line, &self.agents.clone());
        if !self.waits(&cmd) {
            self.echo(line); // a queued line shows when it starts
        }
        match cmd.and_then(|cmd| self.run(cmd, line)) {
            Ok(Some(text)) => self.say(text),
            Ok(None) => {}
            Err(e) => self.say(format!("  {e}")),
        }
    }

    /// Runs a command (`line`: as typed); Some(text) goes to the conversation. A started job's own lines follow
    /// there as they come.
    fn run(&mut self, cmd: Cmd, line: &str) -> Result<Option<String>, String> {
        Ok(match cmd {
            Cmd::Talk(text) if self.talking.is_some() => Some(self.join_talk(text)),
            Cmd::Talk(text) => return self.talk(self.main, text, line.to_string()),
            Cmd::Discuss(agents, topic) => return self.discuss(agents, topic),
            Cmd::Help => {
                let intro = self.intro();
                self.say(intro);
                self.reply(MANUAL);
                Some(self.settings())
            }
            Cmd::Keys => {
                self.reply(&keys());
                None
            }
            Cmd::Undo => Some(format!("  {}", self.s.undo()?)),
            Cmd::Copy => {
                let last = self.out.iter().rev().find_map(|o| match o { Out::Reply(text) => Some(text.clone()), _ => None });
                let text = last.ok_or("no answer to copy yet")?;
                self.note(if copy(&text) { "copied the last answer" } else { "asked the terminal to copy the last answer" });
                None
            }
            Cmd::Clear => {
                self.s.forget();
                self.out.clear();
                self.fitted.clear();
                self.streams.clear();
                self.scroll = 0;
                self.sel = None;
                // what the agents said to each other went with it: the turn now running is the talk's last
                let talk = if self.talking.take().is_some() { "; the agents' talk ends with the turn now running" } else { "" };
                Some(format!("  a new conversation: each agent's next turn starts fresh{talk}"))
            }
            Cmd::Compact => {
                // Claude's own compaction, which the Agent SDK documents for a resumed session: `/compact` as the
                // prompt. It is one model call over the conversation (Claude reports what it held).
                let agent = self.main;
                if agent.spec().adapter != ringo_cli::Adapter::Claude {
                    return Err(format!("{0} compacts its conversation on its own when it fills up; Ringo can start \
                                        /compact only for claude, where it is documented. /clear starts {0} fresh",
                                       agent.name()));
                }
                if !self.s.has_conversation(agent) {
                    return Err(format!("{} has no conversation here to compact yet", agent.name()));
                }
                return self.talk(agent, "/compact".into(), line.to_string());
            }
            Cmd::Separate(on) if on == self.s.separate => Some(match on {
                true => "  the agents already work separately; /together puts them back in one conversation".into(),
                false => "  the agents already share one conversation; /separate gives each a task of its own".into(),
            }),
            Cmd::Separate(true) if self.talking.is_some() =>
                return Err("the agents are talking, which takes the conversation they share: Esc stops them first".into()),
            Cmd::Separate(on) => {
                self.s.separate = on;
                Some(match on {
                    true => "  separate: each agent keeps a conversation of its own, and what one does is not told to \
                             the others. They still work side by side, one turn each. /together puts them back in one \
                             conversation".into(),
                    false => "  together: one conversation again, with the agents side by side, one turn each. Each \
                              hears what the others do from now on; what they did separately stays their own".into(),
                })
            }
            Cmd::Tier(None) => Some(format!("  tier: {} (also: {})", self.s.tier.name(),
                Tier::ALL.iter().filter(|t| **t != self.s.tier).map(|t| t.name()).collect::<Vec<_>>().join(", "))),
            Cmd::Tier(Some(tier)) => Some(self.set_tier(tier)),
            Cmd::Usage => self.usage(),
            Cmd::Agent(None) => {
                self.own_agents();
                None
            }
            Cmd::Agent(Some(OwnAgent::Add { name, file, base, skills })) => Some(self.add_own_agent(name, file, base, skills)?),
            Cmd::Agent(Some(OwnAgent::Remove(name))) => Some(self.remove_own_agent(&name)?),
            Cmd::Model(None) => {
                let t = self.theme;
                self.gap();
                self.out.push(Out::Line(Line::from(vec![
                    Span::styled("  who runs on what", t.title.add_modifier(Modifier::BOLD)),
                    Span::styled(" · /model AGENT NAME changes it · /model AGENT default goes back", t.dim)]), 4));
                let pad = self.agents.iter().map(|a| cols(a.name())).max().unwrap_or(0);
                for agent in self.agents.clone() {
                    let (runs, how) = match (self.s.models.get(&agent), agent.spec().adapter) {
                        (Some(model), _) => (model.clone(), "your choice"),
                        (None, ringo_cli::Adapter::Claude) => ("its own default".into(), "takes fable, opus, sonnet, haiku or a model's full name"),
                        (None, ringo_cli::Adapter::Codex) => ("its own default".into(), "takes a model your plan offers"),
                        (None, _) => ("its own".into(), "it takes no model from Ringo"),
                    };
                    self.out.push(Out::Line(Line::from(vec![
                        Span::raw("    "), Span::styled(format!("{:<pad$}", agent.name()), self.agent_style(agent)),
                        Span::raw(format!("  {runs}")), Span::styled(format!("  · {how}"), t.dim)]), 6 + pad));
                }
                self.last = 0;
                // Codex lists what the plan offers (no model call, a fraction of a second); Claude Code has no list
                let lists = self.agents.iter().copied()
                    .find(|a| a.spec().adapter == ringo_cli::Adapter::Codex && self.s.installed(*a)).filter(|_| self.listing.is_none());
                if let Some(agent) = lists {
                    let program = self.s.program(agent);
                    self.listing = Some((agent, std::thread::spawn(move || ringo_cli::models(agent, &program))));
                }
                Some(format!("  /ask, /review, /design and /research run on {} · /backend SPEC changes it, and SPEC@MODEL names a \
                              model (claude@opus)", self.s.backend))
            }
            Cmd::Model(Some((name, model))) => {
                let agent = Agent::from_name(&name).ok_or_else(|| {
                    format!("no agent {name}: {}", self.agents.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "))
                })?;
                if !self.agents.contains(&agent) {
                    return Err(format!("{name} is not set up here; /setup adds it"));
                }
                if !ringo_cli::takes_model(agent) {
                    return Err(format!("{name} chooses its own model: it takes none from Ringo"));
                }
                let own = matches!(model.as_str(), "default" | "own" | "none");
                if !own && !ringo_cli::valid_model(&model) {
                    return Err("a model's name is one word, as its agent takes it: /model claude opus, /model codex gpt-6-astra".into());
                }
                // A conversation under way keeps the model it was last given when none is named, so going back to
                // the agent's own starts it again from Ringo's record. A named model moves it as it is.
                let was = match own {
                    true => self.s.models.remove(&agent),
                    false => self.s.models.insert(agent, model.clone()),
                };
                let again = own && was.is_some() && self.s.start_over(agent);
                let all: serde_json::Map<String, serde_json::Value> = self.s.models.iter()
                    .map(|(a, m)| (a.name().to_string(), serde_json::json!(m))).collect();
                let saved = crate::theme::set_setting("models", all.into()).err()
                    .map_or(" (saved)".to_string(), |e| format!(" (not saved: {e})"));
                Some(match own {
                    true => format!("  {name} runs on its own default again from its next turn{saved}{}", match again {
                        true => ". Its conversation would keep the model it was on, so that turn starts it again from \
                                 Ringo's record of it",
                        false => "",
                    }),
                    false => format!("  {name} runs on {model} from its next turn{saved}. If your plan does not include it, the \
                                      turn says so when it starts; /model {name} default goes back"),
                })
            }
            Cmd::Mode(None) => Some(format!("  mode: {} (Shift-Tab cycles ask, auto, read)", self.s.mode.name())),
            Cmd::Mode(Some(mode)) => Some(self.set_mode(mode)),
            // on, never a toggle: typed with auto already on (a mode is remembered), it would turn auto off unseen
            Cmd::Auto if self.s.mode == Mode::Auto => Some("  auto mode is already on (/mode ask: back to asking first)".into()),
            Cmd::Auto => Some(self.set_mode(Mode::Auto)),
            Cmd::Theme(None) => Some(format!("  theme: {} (also: {})", self.theme.name,
                crate::theme::NAMES.iter().filter(|n| **n != self.theme.name).copied().collect::<Vec<_>>().join(", "))),
            Cmd::Theme(Some(name)) => {
                let depth = crate::theme::depth(|k| std::env::var(k).ok());
                match Theme::named(&name, depth) {
                    Some(theme) => {
                        self.theme = theme;
                        self.fitted.clear(); // the conversation in the new colours
                        match crate::theme::save(theme.name) {
                            Ok(()) => Some(format!("  theme: {} (saved for next time)", theme.name)),
                            Err(e) => Some(format!("  theme: {} (not saved: {e})", theme.name)),
                        }
                    }
                    None => Some(format!("  no theme {name}: {}", crate::theme::NAMES.join(", "))),
                }
            }
            Cmd::Update(None) if self.releasing.is_some() => {
                Some("  the pipelines' release is still being checked; it reports here when done".into())
            }
            Cmd::Update(None) => {
                let agents = match &self.pending {
                    _ if self.s.hold.is_some() => {
                        self.s.hold.as_ref().map(|why| format!("  {why}; it reports here when done"))
                    }
                    None => Some("  the agents' update check is still running; try again in a moment".into()),
                    Some(p) if p.is_empty() => Some("  no agent to update (checked when the shell opened)".into()),
                    Some(_) if self.s.running() > 0 => {
                        Some("  wait for the running jobs, or /kill them: an update replaces the agent's program".into())
                    }
                    Some(_) => {
                        self.update = true;
                        None
                    }
                };
                // after an agents' update the loop starts it, so the two do not share the machine
                let pipelines = if self.update { None } else { self.check_pipelines() };
                // Ringo's own update, said again: the head points here for one that /restart does not apply
                [self.updated.clone(), agents, pipelines].into_iter().flatten().reduce(|a, b| format!("{a}\n{b}"))
            }
            Cmd::Approve if self.releasing.is_some() => Some("  the pipelines' release is still being checked".into()),
            Cmd::Approve => match self.release.take() {
                None => Some("  nothing to approve: /update checks the pipelines' release first".into()),
                Some(r) => {
                    self.releasing = Some(std::thread::spawn(move || (crate::updates::approve(&r), None)));
                    Some("  moving the pipelines to the checked release".into())
                }
            },
            Cmd::Update(Some(on)) => {
                let saved = crate::theme::set_setting("auto_update", serde_json::json!(on)).err()
                    .map_or(" (saved)".to_string(), |e| format!(" (not saved: {e})"));
                Some(format!("  automatic agent updates {} when the shell opens{saved}", if on { "on" } else { "off" }))
            }
            Cmd::Quit => {
                self.quit = true;
                None
            }
            Cmd::Restart if self.s.running() > 0 => {
                Some("  a job is running: /restart once it ends (Esc stops the turn, /kill N a job)".into())
            }
            Cmd::Restart => {
                (self.restart, self.quit) = (true, true);
                None
            }
            Cmd::Setup => {
                self.setup = true;
                None
            }
            Cmd::Backend(spec) => {
                self.s.backend = spec;
                Some(format!("  Ringo pipelines now use --backend {}", self.s.backend))
            }
            Cmd::Show(id) => {
                let job = self.s.job(id)?;
                let (who, _) = self.who(job);
                let log: Vec<String> = job.log.iter().map(|l| format!("  {l}")).collect();
                let wt = job.worktree.as_ref().map_or(String::new(), |w| format!("; its copy: {}", w.display()));
                Some(format!("#{id} {who} · {}: its last {} log lines (the whole stream is in {}{wt})\n{}", job.title_full,
                             log.len(), job.dir.display(), log.join("\n")))
            }
            Cmd::Kill(id) => {
                // a turn in this folder: its agent's waiting lines come back to the input, as Esc does
                if self.s.job(id).is_ok_and(|j| j.running() && matches!(j.kind, Kind::Agent { write: false, .. })) {
                    self.stop(id);
                    return Ok(None);
                }
                self.s.kill(id)?;
                Some(format!("  stopping #{id}"))
            }
            Cmd::Diff(id) => {
                let patch = self.s.diff(id)?;
                Some(if patch.is_empty() { format!("  #{id} changed nothing") } else { patch })
            }
            Cmd::Apply(id) => Some(format!("  applied #{id} to your files (not committed):\n{}", self.s.apply(id)?)),
            Cmd::Drop(id) => {
                self.s.drop_worktree(id)?;
                Some(format!("  removed #{id}'s copy; its logs stay in {}", self.s.job(id)?.dir.display()))
            }
            Cmd::ReviewJob(of) => {
                let backend = self.s.backend.clone();
                self.s.review_job(of, &backend)?;
                None
            }
            Cmd::ReviewRepo(base, head) => {
                self.s.review_repo(&base, &head)?;
                None
            }
            Cmd::Design(text) => {
                let brief = self.s.expand(&text)?;
                self.s.design(&brief)?;
                None
            }
            Cmd::Ask(question) => {
                let question = self.s.expand(&question)?;
                self.s.ask(&question)?;
                None
            }
            Cmd::Research(topic) => {
                let topic = self.s.expand(&topic)?;
                self.s.research(&topic)?;
                None
            }
            Cmd::Work(spec) => {
                self.s.work(Path::new(&spec))?;
                None
            }
            Cmd::Switch(agent) => {
                self.main = agent;
                Some(format!("  plain lines go to {} now", agent.name()))
            }
            Cmd::Agents(agents, prompt) => {
                // One agent: the conversation goes on with it, here. Two or more: each in its own copy, to compare.
                if let [(agent, false)] = agents[..] {
                    self.main = agent;
                    return self.talk(agent, prompt, line.to_string());
                }
                if agents.len() > 1 && !self.s.git {
                    return Err("two agents at once each work in their own copy of the folder, which needs a git \
                                project; here, ask them one at a time".into());
                }
                let expanded = self.s.expand(&prompt)?;
                let mut notes = Vec::new();
                if expanded.len() != prompt.len() {
                    notes.push(format!("  the handoff adds {} characters to the task:", expanded.len() - prompt.len()));
                    notes.push(expanded.clone());
                }
                for (agent, _) in agents {
                    if let Err(e) = self.s.launch_agent(agent, true, &expanded) {
                        notes.push(format!("  {}: {e}", agent.name()));
                    }
                }
                if notes.is_empty() { None } else { Some(notes.join("\n")) }
            }
        })
    }

    fn menu_entries(&self) -> Vec<(String, String, String)> {
        let before: String = self.input[..self.cursor].iter().collect();
        let agents: Vec<(&str, bool)> = self.agents.iter().map(|a| (a.name(), a.spec().read)).collect();
        menu(&before, &agents)
    }

    /// Ctrl-W, Alt-Backspace: the word before the cursor goes, with the spaces after it.
    fn cut_word_back(&mut self) {
        let start = word_start(&self.input, self.cursor);
        self.input.drain(start..self.cursor);
        self.cursor = start;
    }

    /// The word before the cursor becomes a menu entry.
    fn complete(&mut self, with: &str) {
        let start = self.input[..self.cursor].iter().rposition(|c| matches!(c, ' ' | '\n')).map_or(0, |i| i + 1);
        let chars: Vec<char> = with.chars().collect();
        let n = chars.len();
        self.input.splice(start..self.cursor, chars);
        self.cursor = start + n;
        self.pick = 0;
    }

    fn insert(&mut self, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        self.input.splice(self.cursor..self.cursor, chars);
        self.cursor += n;
        self.scroll = 0;
    }

    /// A paste goes into the input as it is, its lines kept; nothing is sent until Enter.
    fn paste(&mut self, text: &str) {
        self.insert(&clean(text));
    }

    fn recall(&mut self) {
        self.input = self.history.get(self.hist).map(|h| h.chars().collect()).unwrap_or_default();
        self.cursor = self.input.len();
    }

    /// The answers a permission request offers, in order: once, for the session (unless an always-ask rule covers
    /// it), no.
    fn choices(&self, p: &ringo_cli::parse::Permission) -> Vec<(Decision, String)> {
        let mut choices = vec![(Decision::Once, "Yes".to_string())];
        if !self.s.always_asks(p) {
            choices.push((Decision::Session, format!("Yes, and allow `{}` for the rest of this session",
                                                     ringo_cli::parse::short(&p.rule, 50))));
        }
        choices.push((Decision::Deny, "No, carry on without it (Esc)".into()));
        choices
    }

    /// Keys while an agent waits for an answer and the input is empty. True when the key was the dialog's.
    fn answer_key(&mut self, k: &KeyEvent) -> bool {
        let Some((id, p)) = self.s.pending().filter(|_| self.input.is_empty()) else { return false };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c' | 'd')) {
            return false; // Ctrl-C still quits
        }
        if self.asked.as_ref().is_some_and(|(_, at)| at.elapsed() < ANSWER_GRACE) {
            return true; // keys of a word being typed as the request appeared never answer it
        }
        let choices = self.choices(p);
        let pick = |d: Decision| choices.iter().position(|(c, _)| *c == d);
        let chosen = match k.code {
            KeyCode::Up => {
                self.choice = (self.choice + choices.len() - 1) % choices.len();
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.choice = (self.choice + 1) % choices.len();
                None
            }
            KeyCode::Enter => Some(self.choice.min(choices.len() - 1)),
            KeyCode::Char(c @ '1'..='9') if !ctrl => Some(c as usize - '1' as usize).filter(|i| *i < choices.len()),
            KeyCode::Char('y' | 'Y') if !ctrl => pick(Decision::Once),
            KeyCode::Char('a' | 'A') if !ctrl => pick(Decision::Session),
            KeyCode::Char('n' | 'N') if !ctrl => pick(Decision::Deny),
            KeyCode::Esc => pick(Decision::Deny),
            _ => None,
        };
        if let Some(i) = chosen {
            // the job's own lines say what was answered
            if let Err(e) = self.s.decide(id, choices[i].0) {
                self.say(format!("  #{id}: {e}"));
            }
            self.choice = 0;
        }
        true
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let burst = self.last_key.elapsed() < BURST;
        self.last_key = Instant::now();
        if !(ctrl && matches!(k.code, KeyCode::Char('c' | 'd'))) {
            self.quit_armed = false;
        }
        if !matches!(k.code, KeyCode::PageUp | KeyCode::PageDown) {
            self.sel = None;
        }
        // An agent waiting for an answer has the keys once the input is empty. A line being typed keeps its keys
        // until it is sent or cleared.
        if self.answer_key(&k) {
            return;
        }
        // The menu under the input (a / command or an @agent being typed) has Up, Down and Tab, and Enter while the
        // word is not yet a whole entry that takes nothing more (a paste's Enter is left alone).
        let entries = self.menu_entries();
        if !entries.is_empty() {
            let pick = self.pick.min(entries.len() - 1);
            let word: String = self.input[..self.cursor].iter().rev().take_while(|c| !matches!(c, ' ' | '\n')).collect();
            let word: String = word.chars().rev().collect();
            let whole = entries.iter().any(|(to, ..)| *to == word);
            let pasted = burst || event::poll(Duration::ZERO).unwrap_or(false);
            let choosing = entries.len() > 1 && !whole; // else Up and Down still reach earlier lines
            match k.code {
                KeyCode::Up if choosing => return self.pick = (pick + entries.len() - 1) % entries.len(),
                KeyCode::Down if choosing => return self.pick = (pick + 1) % entries.len(),
                KeyCode::Tab => return self.complete(&entries[pick].0),
                KeyCode::Enter if !(ctrl || alt || shift || pasted || whole) => return self.complete(&entries[pick].0),
                _ => {}
            }
        }
        self.pick = 0;
        let page = (self.view.height as usize).saturating_sub(2).max(1);
        match k.code {
            KeyCode::Char('c' | 'd') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else if let Some(id) = self.holding() {
                    self.stop(id);
                } else if self.quit_armed {
                    self.quit = true;
                } else {
                    self.quit_armed = true;
                    let n = self.s.running();
                    self.say(match n {
                        0 => "  Ctrl-C again quits".to_string(),
                        n => format!("  Ctrl-C again stops {} and quits", if n == 1 { "the job running".into() } else { format!("the {n} jobs running") }),
                    });
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(page),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            // the line-editing keys of a shell: to the line's start or end, a word back or on, and cutting either way
            KeyCode::Char('u') if ctrl => {
                let start = line_bounds(&self.input, self.cursor).0;
                self.input.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char('k') if ctrl => {
                let end = line_bounds(&self.input, self.cursor).1;
                self.input.drain(self.cursor..end);
            }
            KeyCode::Char('w') if ctrl => self.cut_word_back(),
            KeyCode::Backspace if alt || ctrl => self.cut_word_back(),
            KeyCode::Char('d') if alt => {
                let end = word_end(&self.input, self.cursor);
                self.input.drain(self.cursor..end);
            }
            KeyCode::Char('a') if ctrl => self.cursor = line_bounds(&self.input, self.cursor).0,
            KeyCode::Char('e') if ctrl => self.cursor = line_bounds(&self.input, self.cursor).1,
            // with nothing typed there is no line to move in: the conversation's own start and end
            KeyCode::Home if self.input.is_empty() => self.scroll = usize::MAX,
            KeyCode::End if self.input.is_empty() => self.scroll = 0,
            KeyCode::Home => self.cursor = line_bounds(&self.input, self.cursor).0,
            KeyCode::End => self.cursor = line_bounds(&self.input, self.cursor).1,
            KeyCode::Char('b') if alt => self.cursor = word_start(&self.input, self.cursor),
            KeyCode::Char('f') if alt => self.cursor = word_end(&self.input, self.cursor),
            KeyCode::Left if alt || ctrl => self.cursor = word_start(&self.input, self.cursor),
            KeyCode::Right if alt || ctrl => self.cursor = word_end(&self.input, self.cursor),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Char(c) if !ctrl => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Enter if alt || shift => self.insert("\n"),
            KeyCode::Enter if self.cursor > 0 && self.input[self.cursor - 1] == '\\' => self.input[self.cursor - 1] = '\n',
            // An Enter right behind another key, or with more keys queued behind it: a paste in a terminal without
            // bracketed paste. Its lines stay in the input, its last one too, instead of being sent (each a model call).
            KeyCode::Enter if burst || event::poll(Duration::ZERO).unwrap_or(false) => self.insert("\n"),
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Up => match vertical(&self.input, self.cursor, true) {
                Some(at) => self.cursor = at,
                None if self.hist > 0 => {
                    self.hist -= 1;
                    self.recall();
                }
                None => {}
            },
            KeyCode::Down => match vertical(&self.input, self.cursor, false) {
                Some(at) => self.cursor = at,
                None if self.hist < self.history.len() => {
                    self.hist += 1;
                    self.recall();
                }
                None => {}
            },
            KeyCode::BackTab | KeyCode::Tab if k.code == KeyCode::BackTab || shift => {
                let text = self.set_mode(self.s.mode.next());
                self.say(text);
            }
            KeyCode::Esc if !self.input.is_empty() => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Esc => match self.holding() {
                Some(id) => self.stop(id),
                None => self.scroll = 0,
            },
            _ => {}
        }
    }

    /// The entry a row of the fitted conversation is in, and which of its rows it is (a row past the end: past the
    /// last entry).
    fn place(&self, mut row: usize) -> (usize, usize) {
        let entry = self.fitted.iter().position(|rows| row.checked_sub(rows.len()).map(|rest| row = rest).is_none());
        (entry.unwrap_or(self.fitted.len()), row)
    }

    /// The row of the fitted conversation that a row of an entry is now; a row its entry no longer has (fitted
    /// again, it came out shorter) is its last.
    fn row_of(&self, (entry, row): (usize, usize)) -> usize {
        let before: usize = self.fitted.iter().take(entry).map(Vec::len).sum();
        before + self.fitted.get(entry).map_or(row, |rows| row.min(rows.len().saturating_sub(1)))
    }

    /// The wheel scrolls the conversation; a drag over it selects its text and copies it when the button comes up.
    /// The selection's ends are kept as places in entries, so they stay on their text while an agent writes above.
    fn mouse(&mut self, m: MouseEvent) -> bool {
        let v = self.view;
        let row = m.row.clamp(v.y, (v.y + v.height).saturating_sub(1));
        let (entry, at) = self.place(self.top + (row - v.y) as usize);
        let spot = (entry, at, m.column.saturating_sub(v.x) as usize);
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(3),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let inside = m.row >= v.y && m.row < v.y + v.height && m.column >= v.x && m.column < v.x + v.width;
                self.sel = inside.then_some((spot, spot));
                self.dragging = inside;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                if let Some((_, to)) = &mut self.sel {
                    *to = spot;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                let text = self.selected();
                if self.sel.is_some_and(|(a, b)| a == b) || text.trim().is_empty() {
                    self.sel = None; // a click, not a selection
                } else {
                    let n = text.lines().count();
                    let what = if n == 1 { "1 line".to_string() } else { format!("{n} lines") };
                    self.note(if copy(&text) { format!("copied {what}") } else { format!("asked the terminal to copy {what}") });
                }
            }
            _ => return false,
        }
        true
    }

    /// The selection as rows of the conversation as it is fitted now, first spot first.
    fn selection(&self) -> Option<(Spot, Spot)> {
        let spot = |(entry, row, col): Mark| (self.row_of((entry, row)), col);
        let (a, b) = self.sel.map(|(a, b)| (spot(a), spot(b)))?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The selected text: each row's selected columns, its trailing spaces cut.
    fn selected(&self) -> String {
        let Some((a, b)) = self.selection() else { return String::new() };
        self.fitted.iter().flatten().enumerate().skip(a.0).take(b.0 - a.0 + 1)
            .map(|(r, line)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let from = if r == a.0 { a.1 } else { 0 };
                let to = if r == b.0 { b.1 + 1 } else { usize::MAX };
                slice_cols(&text, from, to).trim_end().to_string()
            })
            .collect::<Vec<_>>().join("\n")
    }

    /// Fits the entries not yet fitted to `width` (every entry after the width changed); the conversation's rows.
    fn fit_to(&mut self, width: usize) -> usize {
        if self.out.len() > KEEP {
            let extra = self.out.len() - KEEP;
            self.out.drain(..extra);
            self.fitted.drain(..extra.min(self.fitted.len()));
            self.streams.retain(|_, s| s.at.checked_sub(extra).map(|n| s.at = n).is_some());
            self.anchor = self.anchor.and_then(|(entry, row)| Some((entry.checked_sub(extra)?, row)));
            self.sel = self.sel.and_then(|(a, b)| Some(((a.0.checked_sub(extra)?, a.1, a.2), (b.0.checked_sub(extra)?, b.1, b.2))));
        }
        if width != self.fitted_w {
            self.fitted.clear();
            self.fitted_w = width;
        }
        while self.fitted.len() < self.out.len() {
            let rows = self.out[self.fitted.len()].rows(width, &self.theme);
            self.fitted.push(rows);
        }
        self.fitted.iter().map(Vec::len).sum()
    }

    /// The conversation's rows that fit `area`, scrolled as set, with the selection shown. Scrolled up, the row at
    /// the top stays the one it was whatever is written meanwhile, below it or above it (with two agents at work, the
    /// reply of the one that started first grows above what the other writes): the place is kept as an entry and a
    /// row of it, not as a count of rows from the end.
    fn conversation(&mut self, f: &mut Frame, area: Rect, panel: bool) {
        let total = self.fit_to(area.width as usize);
        let h = area.height as usize;
        // `scroll` as the last frame left it: no key or wheel moved it since, so the place it kept still holds
        if self.scroll > 0 && self.scroll == self.scrolled && let Some(at) = self.anchor {
            self.scroll = total.saturating_sub(h + self.row_of(at));
        }
        self.scroll = self.scroll.min(total.saturating_sub(h));
        let top = total.saturating_sub(h + self.scroll);
        self.scrolled = self.scroll;
        self.anchor = (self.scroll > 0).then(|| self.place(top));
        let shown: Vec<Line<'static>> = self.fitted.iter().flatten().skip(top).take(h).cloned().collect();
        f.render_widget(Paragraph::new(shown), area);
        // until the owner says something: where to start, centred a third of the way down, under any notes
        let splash = if self.spoke { Vec::new() } else { self.splash(panel) };
        let from = (total + 1).max(h.saturating_sub(splash.len()) / 3);
        if !splash.is_empty() && from + splash.len() <= h && splash.iter().all(|l| l.width() <= area.width as usize) {
            for (i, line) in splash.into_iter().enumerate() {
                let w = line.width() as u16;
                let at = Rect { x: area.x + (area.width - w) / 2, y: area.y + (from + i) as u16, width: w, height: 1 };
                f.render_widget(Paragraph::new(line), at);
            }
        }
        if let Some((a, b)) = self.selection() {
            for y in 0..area.height {
                let r = top + y as usize;
                if r < a.0 || r > b.0 {
                    continue;
                }
                let from = if r == a.0 { a.1 } else { 0 };
                let to = if r == b.0 { b.1 } else { area.width as usize };
                for x in from..=to.min(area.width.saturating_sub(1) as usize) {
                    if let Some(cell) = f.buffer_mut().cell_mut((area.x + x as u16, area.y + y)) {
                        cell.set_style(Style::new().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
        self.view = area;
        self.top = top;
    }

    /// The live area's rows for the running jobs (mark, number, who, time and tokens, any same-files warning, what it
    /// does now) and one per running subagent; at most `max` rows.
    fn job_rows(&self, width: usize, max: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let short = ringo_cli::parse::short;
        let mut lines = Vec::new();
        for job in self.s.jobs.iter().filter(|j| j.running()) {
            let (who, who_style) = self.who(job);
            let head = format!(" {} ", self.spinner());
            let id = format!("#{} ", job.id);
            let calls = match job.calls { 0 => String::new(), 1 => " · 1 call".into(), n => format!(" · {n} calls") };
            let tokens = match job.tokens_so_far() { 0 => String::new(), n => format!(" · {} tokens", fmt_tokens(n)) };
            let stats = format!(" · {}{tokens}{calls} · ", fmt_age(job.age()));
            let room = width.saturating_sub(head.chars().count() + id.len() + who.chars().count() + stats.chars().count());
            let overlaps: Vec<String> = self.s.overlaps(job.id).into_iter()
                .map(|(other, files)| format!("#{other} {}", files.join(", "))).collect();
            let warning = if overlaps.is_empty() { String::new() } else {
                format!("{} ", short(&format!("⚠ same files as {}", overlaps.join("; ")), room / 2))
            };
            let detail = short(&job.activity, room.saturating_sub(warning.chars().count()));
            lines.push(Line::from(vec![
                Span::styled(head, t.running), Span::styled(id, t.dim), Span::styled(who, who_style),
                Span::styled(stats, t.dim), Span::styled(warning, t.warn), Span::raw(detail),
            ]));
            for sub in job.subs.iter().filter(|s| s.ended.is_none()) {
                let text = if sub.activity.is_empty() { sub.label.clone() } else { format!("{}: {}", sub.label, sub.activity) };
                lines.push(Line::from(vec![
                    Span::styled("     └ ", t.dim), Span::styled(format!("{} ", self.spinner()), t.running),
                    Span::styled(short(&text, width.saturating_sub(9)), t.dim),
                ]));
            }
        }
        if lines.len() > max {
            let cut = lines.len() - max + 1;
            lines.truncate(max.saturating_sub(1));
            lines.push(Line::styled(format!("   … {cut} more rows (/show N prints a job's log)"), t.dim));
        }
        lines
    }

    /// What the bottom of the screen holds for a terminal of `width` x `height`.
    fn live(&self, width: u16, height: u16) -> Live {
        let w = width as usize;
        let t = &self.theme;
        let panel = width >= PANEL_AT;
        // Without the side panel, the running jobs and the queue sit above the input.
        let jobs = if panel { Vec::new() } else { self.job_rows(w, (height / 3).max(2) as usize) };
        let mut queue: Vec<Line<'static>> = Vec::new();
        if !panel {
            queue = self.queue.iter().take(3).map(|(agent, task, _)| {
                let head = format!("   ↳ next for {}: ", agent.name());
                let text = one_row(&clean(task).replace('\n', " "), w.saturating_sub(cols(&head)));
                Line::from(vec![Span::styled(head, t.dim), Span::styled(text, t.selected)])
            }).collect();
            if self.queue.len() > 3 {
                queue.push(Line::styled(format!("   … {} more queued", self.queue.len() - 3), t.dim));
            }
        }
        let ask = self.prompt_lines(w.saturating_sub(2));
        let ask_h = ask.as_ref().map_or(0, |(_, lines)| (lines.len() as u16 + 1).clamp(2, (height / 2).max(2)));
        let typing = ask.is_none() || !self.input.is_empty();
        let mark: Vec<Span<'static>> = match self.target() {
            Some((name, style)) => vec![Span::styled(format!(" {name}"), style), Span::styled(" ❯ ", t.focus)],
            None => vec![Span::styled(" ❯ ", t.focus)],
        };
        let mark_w = mark.iter().map(|s| cols(&s.content)).sum::<usize>();
        let (rows, at) = input_rows(&self.input, self.cursor, w.saturating_sub(mark_w));
        let shown = rows.len().min((height / 4).clamp(1, 10) as usize);
        let top = (at.0 + 1).saturating_sub(shown);
        let entries = if typing { self.menu_entries() } else { Vec::new() };
        let room = (height / 4).clamp(1, 6) as usize;
        let pick = self.pick.min(entries.len().saturating_sub(1));
        let first = (pick + 1).saturating_sub(room);
        let label = entries.iter().map(|(_, l, _)| cols(l)).max().unwrap_or(0);
        let menu = entries.iter().enumerate().skip(first).take(room)
            .map(|(i, (_, l, what))| {
                let (mark, style) = if i == pick { ("› ", t.focus) } else { ("  ", t.dim) };
                Line::from(vec![Span::styled(format!(" {mark}{l}{} ", " ".repeat(label - cols(l))), style),
                                Span::styled(what.clone(), t.dim)])
            })
            .collect();
        Live { panel, jobs, queue, ask, ask_h, typing, rows, at, top, shown, menu, mark, mark_w }
    }

    /// The cat's state now: asking beats working beats how the last job ended (for MOOD after it) beats idle.
    fn mood(&self) -> (&'static str, Style) {
        let t = &self.theme;
        match self.ended_at {
            _ if self.s.pending().is_some() => ("asking", t.warn),
            _ if self.s.running() > 0 => ("working", t.running),
            Some((ok, at)) if at.elapsed() < MOOD => if ok { ("done", t.ok) } else { ("failed", t.fail) },
            _ => ("idle", t.title),
        }
    }

    /// What the cat shows now: its state and frame (the shell redraws when this changes).
    fn look(&self) -> (&'static str, usize) {
        let (state, _) = self.mood();
        let n = mascot(state).len().max(1);
        (state, (self.started.elapsed().as_millis() / MASCOT_MS) as usize % n)
    }

    /// The side panel: the cat, then what is going on, section by section, at most `height` rows of `width`.
    fn panel(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let head = |text: &str| Line::styled(text.to_string(), t.focus.add_modifier(Modifier::BOLD));
        let fit = |text: &str| one_row(text, width);
        let mut out: Vec<Line<'static>> = Vec::new();
        let ((_, style), (state, frame)) = (self.mood(), self.look());
        if let Some(rows) = mascot(state).get(frame) {
            out.extend(rows.iter().map(|r| Line::styled(fit(r), style)));
            out.push(Line::default());
        }
        out.push(head("AGENTS"));
        for &agent in &self.agents {
            let job = self.s.jobs.iter().rev()
                .find(|j| j.running() && matches!(j.kind, Kind::Agent { agent: a, .. } if a == agent));
            let (dot, what, style) = match job {
                Some(j) if !j.asks.is_empty() => ("◍", "asking you".to_string(), t.warn),
                Some(j) => ("◉", format!("working {}", fmt_age(j.age())), t.running),
                None if agent == self.main => ("○", "talking to you".to_string(), t.dim),
                None => ("○", "idle".to_string(), t.dim),
            };
            // a long name gives way to the state
            let name = one_row(agent.name(), width.saturating_sub(2 + 1 + cols(&what)).clamp(4, 12));
            let what = one_row(&what, width.saturating_sub(2 + cols(&name).max(8) + 1));
            out.push(Line::from(vec![Span::styled(format!("{dot} "), style),
                                     Span::styled(format!("{name:<8} "), self.agent_style(agent)),
                                     Span::styled(what, t.dim)]));
        }
        for job in self.s.jobs.iter().filter(|j| j.running() && matches!(j.kind, Kind::Ringo { .. })) {
            let (who, style) = self.who(job);
            out.push(Line::from(vec![Span::styled("◉ ", t.running), Span::styled(fit(&who), style),
                                     Span::styled(format!(" {}", fmt_age(job.age())), t.dim)]));
        }
        // each plan's limits, a gauge a window: "claude 5h ▰▱▱▱▱▱  2%"
        if !self.limits.is_empty() {
            out.push(Line::default());
            out.push(head("LIMITS"));
            let cells = width.saturating_sub(7 + 3 + 5).clamp(4, 12);
            for (agent, plan) in &self.limits {
                for (i, w) in plan.windows.iter().enumerate() {
                    // "5 hours" 5h, "week" wk, anything else its words' first letters ("weekly scoped" ws)
                    let short: String = match w.name.as_str() {
                        "5 hours" => "5h".into(),
                        "week" => "wk".into(),
                        n => n.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect(),
                    };
                    let on = ((w.percent.clamp(0.0, 100.0) / 100.0 * cells as f64).round() as usize).max((w.percent > 0.0) as usize);
                    out.push(Line::from(vec![
                        Span::styled(format!("{:<7}", if i == 0 { one_row(agent.name(), 6) } else { String::new() }), self.agent_style(*agent)),
                        Span::styled(format!("{short:<3}"), t.dim), Span::styled("▰".repeat(on), used_style(w.percent, t)),
                        Span::styled("▱".repeat(cells - on), t.border), Span::styled(format!(" {:>3.0}%", w.percent), t.dim)]));
                }
            }
        }
        // the turn in this folder: the running one, else the last
        let turn = self.s.jobs.iter().rev().find(|j| matches!(j.kind, Kind::Agent { write: false, .. }));
        if let Some(job) = turn {
            let (who, style) = self.who(job);
            out.push(Line::default());
            out.push(head(if job.running() { "THIS TURN" } else { "LAST TURN" }));
            out.push(Line::from(vec![Span::styled(format!("#{} ", job.id), t.dim), Span::styled(who, style),
                                     Span::styled(format!(" · {}", fmt_age(job.age())), t.dim)]));
            // one cell per 12,500 tokens, eight cells: past 100,000 the gauge turns to a warning
            let tokens = job.tokens_so_far();
            let cells = (tokens.div_ceil(12_500) as usize).min(8);
            let gauge = format!("{}{}", "▰".repeat(cells), "▱".repeat(8 - cells));
            out.push(Line::from(vec![Span::styled(gauge, if tokens > 100_000 { t.warn } else { t.running }),
                                     Span::styled(format!(" {} tokens", fmt_tokens(tokens)), t.dim)]));
            if job.running() {
                out.push(Line::styled(fit(&job.activity), t.dim));
                for sub in job.subs.iter().filter(|s| s.ended.is_none()) {
                    out.push(Line::styled(fit(&format!("└ {}", sub.label)), t.dim));
                }
            }
        }
        if let Some(job) = self.s.jobs.iter().rev().find(|j| !j.running() && j.after.is_some() && !j.changed.is_empty())
            .filter(|j| !j.undone) {
            out.push(Line::default());
            out.push(head("CHANGED"));
            out.extend(job.changed.iter().take(4).map(|f| Line::raw(fit(f))));
            if job.changed.len() > 4 {
                out.push(Line::styled(format!("and {} more", job.changed.len() - 4), t.dim));
            }
            out.push(Line::styled(fit("/undo takes it back"), t.dim));
        }
        let copies: Vec<&ringo_cli::Job> = self.s.jobs.iter().filter(|j| j.worktree.is_some() && !j.applied).collect();
        if !copies.is_empty() {
            out.push(Line::default());
            out.push(head("COPIES"));
            for job in copies {
                let what = match &job.status {
                    Status::Running => format!("working {}", fmt_age(job.age())),
                    Status::Done if job.unchanged => "no change".into(),
                    Status::Done => format!("ready · /apply {}", job.id),
                    Status::Failed(_) => "failed".into(),
                    Status::Cancelled => "stopped".into(),
                };
                let name = match job.kind { Kind::Agent { agent, .. } => agent.name(), Kind::Ringo { .. } => "ringo" };
                out.push(Line::from(vec![Span::styled(format!("#{} {name} ", job.id), t.dim), Span::raw(fit(&what))]));
            }
        }
        if !self.queue.is_empty() {
            out.push(Line::default());
            out.push(head("NEXT"));
            for (agent, task, _) in &self.queue {
                out.push(Line::styled(fit(&format!("↳ {}: {}", agent.name(), clean(task).replace('\n', " "))), t.selected));
            }
        }
        if out.len() > height {
            out.truncate(height.saturating_sub(1));
            out.push(Line::styled("…", t.dim));
        }
        out
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let live = self.live(area.width, area.height);
        let t = self.theme;
        let w = area.width as usize;
        let [head, rule, middle, below, base, asked, input, menu] = Layout::vertical([Constraint::Length(1),
            Constraint::Length(1), Constraint::Min(1),
            Constraint::Length(live.jobs.len() as u16 + live.queue.len() as u16), Constraint::Length(1),
            Constraint::Length(live.ask_h), Constraint::Length(live.input_h()),
            Constraint::Length(live.menu.len() as u16)]).areas(area);
        // the head: the mark and the folder; the mode and the session's tokens at the right
        let (mode, tokens) = (self.s.mode.name(), fmt_tokens(self.s.tokens()));
        let tier = match self.s.tier { Tier::Standard => String::new(), t => format!("{} · ", t.name()) };
        let tier = format!("{}{tier}", if self.s.separate { "separate · " } else { "" });
        // a tier other than standard (and /separate) is the last to go: it changes what each turn costs
        let right = [format!("{mode} mode · {tier}{tokens} tokens "), format!("{mode} · {tier}{tokens} "),
                     format!("{} ", tier.trim_end_matches(['·', ' '])), String::new()].into_iter()
            .filter(|r| r.is_empty() || !r.trim().is_empty())
            .find(|r| 10 + 8 + cols(r) <= w).unwrap_or_default(); // a narrow head keeps the folder's start
        let left = one_row(&self.label, w.saturating_sub(12 + cols(&right)));
        // A newer Ringo stays in the head until it is applied: the line that said so scrolls away, and the input's
        // hint gives way to others while a turn runs. Built, /restart applies it; else /update says how to get it.
        let news = match (&self.staged, &self.updated) {
            (Some(_), _) => "  ↑ update ready · /restart",
            (None, Some(_)) => "  ↑ update available · /update",
            (None, None) => "",
        };
        let news = if 10 + cols(&left) + cols(news) + 1 + cols(&right) <= w { news } else { "" };
        let pad = w.saturating_sub(10 + cols(&left) + cols(news) + cols(&right));
        f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" ▲▲ "), Span::styled("ringo", t.brand),
            Span::raw("  "), Span::raw(left), Span::styled(news, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(pad)), Span::raw(right)])).style(t.header), head);
        self.drawn_look = self.look();
        // the conversation, and the side panel beside it behind a bar
        let (view, split) = if live.panel {
            let [view, bar, side] = Layout::horizontal([Constraint::Min(1), Constraint::Length(1),
                                                        Constraint::Length(PANEL_W)]).areas(middle);
            f.render_widget(Paragraph::new(vec![Line::styled("│", t.border); bar.height as usize]), bar);
            let side = Rect { x: side.x + 1, width: side.width.saturating_sub(1), ..side };
            f.render_widget(Paragraph::new(self.panel(side.width as usize, side.height as usize)), side);
            (view, Some((bar.x - area.x) as usize))
        } else {
            (middle, None)
        };
        let ruled = |joint: &str| -> Line<'static> {
            let mut text = "─".repeat(w);
            if let Some(at) = split {
                text = format!("{}{joint}{}", "─".repeat(at), "─".repeat(w.saturating_sub(at + 1)));
            }
            Line::styled(text, t.border)
        };
        f.render_widget(Paragraph::new(ruled("┬")), rule);
        f.render_widget(Paragraph::new(ruled("┴")), base);
        self.conversation(f, view, live.panel);
        let mut rows_below = live.jobs.clone();
        rows_below.extend(live.queue.iter().cloned());
        f.render_widget(Paragraph::new(rows_below), below);
        f.render_widget(Paragraph::new(live.menu.clone()), menu);
        if let Some((title, lines)) = &live.ask {
            // The request, wrapped, from its first line, under a rule in the warning colour: a long one shows how
            // much more there is (all of it is in the conversation above), and the answers stay at the end.
            let room = asked.height.saturating_sub(1) as usize;
            let answers = lines.iter().rev().take_while(|l| l.width() > 0).count();
            let (what, tail) = lines.split_at(lines.len() - answers);
            let mut shown: Vec<Line> = what.to_vec();
            if shown.len() + tail.len() > room {
                shown.truncate(room.saturating_sub(tail.len() + 1));
                let more = what.len() - shown.len();
                shown.push(Line::styled(format!("… {more} more lines, all of them in the conversation above"), t.warn));
            }
            shown.extend(tail.iter().cloned());
            let frame = Block::new().borders(Borders::TOP).border_style(t.warn)
                .title(Span::styled(title.clone(), t.warn.add_modifier(Modifier::BOLD))).padding(Padding::horizontal(1));
            f.render_widget(Paragraph::new(shown).block(frame), asked);
        }
        if live.typing {
            let hint = match &self.toast {
                Some((text, at)) if at.elapsed() < TOAST => text.clone(),
                // the count grows as lines are written below: nothing arrives unseen
                _ if self.scroll > 0 => format!("scrolled up · {} rows below · End goes back", self.scroll),
                _ if self.talking.is_some() => "the agents are talking · a line of yours joins in · Esc stops them".into(),
                _ if self.s.busy().is_some() => "Esc stops the turn".into(),
                _ if self.staged.is_some() => "update ready · /restart".into(),
                _ if self.release.is_some() => "pipelines ready · /update approve".into(),
                _ => "⇧⇥ mode · ? keys".into(),
            };
            let marked = |rest: Span<'static>| Line::from([live.mark.clone(), vec![rest]].concat());
            let mut rows: Vec<Line> = if self.input.is_empty() {
                // what a line does now, by `talk`'s rules: starts a turn, goes into the agent's turn at work, or waits
                let main = self.main;
                let ask = if !main.spec().read || !self.must_wait(main) {
                    "Tell Ringo what you want to do"
                } else if self.s.busy_for(main).is_some_and(|id| self.s.steerable(id)) && !self.queue.iter().any(|(a, ..)| *a == main) {
                    "type more; it goes into this turn as it works"
                } else {
                    "type what comes next; it waits for this turn"
                };
                vec![marked(Span::styled(ask, t.dim))]
            } else {
                live.rows.iter().enumerate().skip(live.top).take(live.shown).map(|(i, r)| match i {
                    0 => marked(Span::raw(r.clone())),
                    _ => Line::from(vec![Span::raw(" ".repeat(live.mark_w)), Span::raw(r.clone())]),
                }).collect()
            };
            // the hint at the right of the first row where the text leaves room for it; an empty input without the
            // room shows the hint instead of its invitation
            if let Some(first) = rows.first_mut() {
                if first.width() + cols(&hint) + 3 <= w {
                    let pad = w - first.width() - cols(&hint) - 1;
                    first.spans.push(Span::raw(" ".repeat(pad)));
                    first.spans.push(Span::styled(hint, t.dim));
                } else if self.input.is_empty() {
                    *first = marked(Span::styled(hint, t.dim));
                }
            }
            f.render_widget(Paragraph::new(rows), input);
            if live.ask.is_none() || !self.input.is_empty() {
                f.set_cursor_position((input.x + live.mark_w as u16 + live.at.1 as u16, input.y + (live.at.0 - live.top) as u16));
            }
        }
    }

    /// Whom the line being typed goes to, as its prompt shows: the agent plain lines go to, or the one its @ names
    /// (all, for @all). A command, or an @ word that names no agent yet, shows no one.
    fn target(&self) -> Option<(&'static str, Style)> {
        let line: String = self.input.iter().collect();
        if line.trim_start().starts_with('/') {
            return None;
        }
        match line.split_whitespace().next().and_then(|w| w.strip_prefix('@')) {
            None => Some((self.main.name(), self.agent_style(self.main))),
            Some("all") => Some(("all", self.theme.focus)),
            Some(name) => Agent::from_name(name.trim_end_matches('!')).filter(|a| self.agents.contains(a))
                .map(|a| (a.name(), self.agent_style(a))),
        }
    }

    /// A waiting permission request as the screen shows it: its title, then every line of the action, wrapped, a
    /// blank row, and the answers (or how to reach them while a line is being typed).
    fn prompt_lines(&self, width: usize) -> Option<(String, Vec<Line<'static>>)> {
        let (id, p) = self.s.pending()?;
        let job = self.s.job(id).ok()?;
        let (who, _) = self.who(job);
        let more = self.s.waiting() - 1;
        let wants = match p.tool.as_str() {
            "Bash" => "run a command".to_string(),
            "WebFetch" => "open a web page".into(),
            "WebSearch" => "search the web".into(),
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "change a file".into(),
            tool if tool.starts_with("mcp__") => "use a connector".into(),
            tool => format!("use {tool}"),
        };
        let title = format!(" {who} #{id} wants to {wants}{} ", if more > 0 { format!(" · {more} more waiting") } else { String::new() });
        let mut lines: Vec<Line<'static>> = p.what.lines().flat_map(|l| wrap(&clean(l), width)).map(Line::raw).collect();
        lines.push(Line::default());
        if !self.input.is_empty() {
            lines.push(Line::styled("your line is still in the input: send it, or clear it with Esc, to answer", self.theme.dim));
        } else {
            for (i, (_, label)) in self.choices(p).into_iter().enumerate() {
                let (mark, style) = if i == self.choice { ("›", self.theme.focus) } else { (" ", self.theme.dim) };
                lines.push(Line::styled(format!("{mark} {}. {label}", i + 1), style));
            }
        }
        Some((title, lines))
    }

    /// The conversation as plain text, saved in the session's folder when the shell ends.
    fn save(&self) -> Option<PathBuf> {
        // a reply still being written goes in whole: what the screen keeps back for now (see `steady`) was received
        let whole: std::collections::HashMap<usize, Out> = self.streams.values()
            .map(|s| (s.at, Out::Reply(closed(clean(&s.raw))))).collect();
        let text: Vec<String> = self.out.iter().enumerate().flat_map(|(i, o)| whole.get(&i).unwrap_or(o).rows(100, &self.theme))
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string()).collect();
        let path = self.s.dir.join("conversation.txt");
        std::fs::write(&path, text.join("\n") + "\n").ok().map(|_| path)
    }
}

/// `ringo --resume`: the conversations kept for this folder, newest first, and the one the owner picks (Enter takes the
/// newest). None when there is none or the owner picks none.
fn pick(session: &Session) -> Option<PathBuf> {
    let root = session.dir.parent().and_then(Path::parent).unwrap_or(&session.dir);
    let kept: Vec<ringo_cli::Kept> = ringo_cli::kept(root, &session.repo).into_iter().take(10).collect();
    if kept.is_empty() {
        eprintln!("ringo: no conversation to go on with in {}; run ringo to start one", session.repo.display());
        return None;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    println!("Conversations in {}, newest first:", session.repo.display());
    for (n, k) in kept.iter().enumerate() {
        println!("  {}. {} ago · {}", n + 1, fmt_ago(now.saturating_sub(k.saved)), k.about);
    }
    print!("Go on with which (Enter for 1, q to cancel)? ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    match line.trim() {
        "" => Some(kept[0].path.clone()),
        n => n.parse::<usize>().ok().and_then(|n| kept.get(n.checked_sub(1)?)).map(|k| k.path.clone()),
    }
}

/// A time ago, roughly: 40 s, 12 min, 3 h, 2 d.
fn fmt_ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// What holds new jobs while the pipelines' speed check runs (see App::check_pipelines).
const CHECKING: &str = "the pipelines' speed check is running";

/// How the shell starts: new, from what the shell before a /restart handed on, or from a conversation the owner picks
/// (`ringo --resume`).
pub enum Start {
    New,
    Restarted(PathBuf),
    Pick,
}

/// The shell.
pub fn run(start: Start) -> ExitCode {
    let exe = std::env::current_exe().map(|e| std::fs::canonicalize(&e).unwrap_or(e)).unwrap_or_default();
    let agents = match ringo_cli::configured_agents() {
        Some(agents) if !agents.is_empty() => agents,
        _ => crate::setup::run(), // the first start: connect at least one agent
    };
    if agents.is_empty() {
        eprintln!("ringo: no agent is set up yet; run ringo again once one is installed");
        return ExitCode::from(2);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let session = match Session::new(&cwd) {
        Ok(mut s) => {
            // this binary (native pipelines unless a release says otherwise), as its own version keeps it
            s.pipeline = Some(vec![crate::updates::own_binary(&exe).into()]);
            s
        }
        Err(e) => {
            eprintln!("ringo: {e}");
            return ExitCode::from(2);
        }
    };
    let resume = match start {
        Start::New => None,
        Start::Restarted(path) => Some((path, false)),
        Start::Pick => match pick(&session) {
            Some(path) => Some((path, true)),
            None => {
                let _ = std::fs::remove_dir(&session.dir); // nothing in it yet
                return ExitCode::from(1);
            }
        },
    };
    let name = session.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let label = match &session.branch { Some(b) => format!("{name} ({b})"), None => name };
    let saved = crate::theme::settings_all(); // read once, for the theme, the mode and auto updates
    let mut app = App::new(session, agents, label, Theme::load_from(&saved), load_history());
    if let Some(mode) = saved.get("mode").and_then(|v| v.as_str().and_then(Mode::from_name)) {
        app.s.mode = mode;
    }
    // the model each agent runs on, where the owner chose one (/model)
    for (name, model) in saved.get("models").and_then(|v| v.as_object()).into_iter().flatten() {
        if let (Some(agent), Some(model)) = (Agent::from_name(name), model.as_str().filter(|m| ringo_cli::valid_model(m))) {
            app.s.models.insert(agent, model.to_string());
        }
    }
    match resume.as_ref().and_then(|(path, _)| app.s.go_on(path)) {
        Some(before) => {
            let text = before["conversation"].as_str().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
            text.lines().for_each(|line| app.out.push(Out::text(line)));
            let commit = &ringo_cli::COMMIT[..ringo_cli::COMMIT.len().min(7)];
            match resume.as_ref().is_some_and(|(_, picked)| *picked) {
                true => app.say("  the conversation goes on from where it was kept"),
                false => app.say(format!("  Ringo restarted ({commit}); the conversations go on")),
            }
        }
        None => {
            app.welcome();
            if let Some((path, _)) = &resume {
                app.say(format!("  {} could not be read, so this is a new conversation", path.display()));
            }
        }
    }
    let asked = app.plan_agents();
    app.read_limits(&asked, false); // for the side panel's gauges
    app.releasing = Some(std::thread::spawn(|| crate::updates::check_release(false))); // are the pipelines behind?
    let (notes, news) = std::sync::mpsc::channel();
    let (agents, state) = (app.agents.clone(), ringo_cli::state_root().join("engine-versions.json"));
    let auto = saved.get("auto_update").and_then(|v| v.as_bool()).unwrap_or(false);
    let check = notes.clone();
    std::thread::spawn(move || {
        let _ = check.send((false, crate::updates::check(&agents, &state)));
    });
    // Ringo's own update: when the shell opens, then hourly, one at a time
    let (updated, update_news) = std::sync::mpsc::channel();
    let update_ringo = |tx: std::sync::mpsc::Sender<Option<(String, PathBuf)>>, exe: PathBuf| {
        std::thread::spawn(move || {
            let _ = tx.send(crate::updates::update_ringo(&exe));
        });
    };
    update_ringo(updated.clone(), exe.clone());
    let (mut updating, mut updated_at) = (true, Instant::now());

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    let mut terminal = match open() {
        Ok(terminal) => terminal,
        Err(e) => {
            restore();
            eprintln!("ringo: cannot open the terminal: {e}");
            return ExitCode::from(2);
        }
    };
    #[cfg(unix)]
    watch_the_end();
    end_signals(true);
    let (mut dirty, mut tick) = (true, Instant::now());
    while !app.quit && !ENDED.load(Ordering::SeqCst) {
        if dirty {
            if terminal.draw(|f| app.draw(f)).is_err() {
                break;
            }
            dirty = false;
        }
        // Job events arrive on a channel, not as terminal input, so the poll timeout is how late one is shown: 25 ms
        // while jobs run (a pump is a few microseconds; redraws still happen only when something changed). Every event
        // already queued is taken before the next redraw.
        let wait = if app.s.running() > 0 || app.toast.is_some() || !app.plans.is_empty() || app.listing.is_some() { 25 } else { 250 };
        if event::poll(Duration::from_millis(wait)).unwrap_or(false) {
            loop {
                match event::read() {
                    Ok(Term::Key(k)) if k.kind == KeyEventKind::Press => {
                        app.key(k);
                        dirty = true;
                    }
                    Ok(Term::Paste(text)) => {
                        app.paste(&text);
                        dirty = true;
                    }
                    Ok(Term::Mouse(m)) => dirty |= app.mouse(m),
                    Ok(Term::Resize(..)) => dirty = true,
                    _ => {}
                }
                // ENDED: told to end from outside, the shell takes no more keys
                if app.quit || app.setup || app.update || ENDED.load(Ordering::SeqCst)
                    || !event::poll(Duration::ZERO).unwrap_or(false) {
                    break;
                }
            }
        }
        if app.setup {
            close(&mut terminal); // the agents' own sign-in runs in the plain terminal
            end_signals(false);
            app.agents = crate::setup::run();
            end_signals(true);
            match open() {
                Ok(t) => terminal = t,
                Err(_) => break,
            }
            app.setup = false;
            dirty = true;
            let names: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
            app.say(format!("  agents set up: {}", names.join(", ")));
        }
        dirty |= app.s.pump();
        // A line a turn ended before taking in goes next, ahead of the lines typed after it; one for a turn you
        // stopped goes back in the input, as the waiting lines do.
        for (id, agent, text) in std::mem::take(&mut app.s.returned).into_iter().rev() {
            if app.s.stopped(id) {
                let rest: String = app.input.iter().collect();
                app.input = if rest.is_empty() { text } else { format!("{text}\n{rest}") }.chars().collect();
                app.cursor = app.input.len();
            } else {
                app.queue.push_front((agent, text.clone(), text));
            }
        }
        dirty |= app.quota_done();
        // A job that just ended is seen before any waiting line starts: ultra's review holds the folder from there.
        app.feed();
        while let Some((ask, route)) = app.s.routed.pop_front() {
            app.start_routed(ask, &route);
            dirty = true;
        }
        dirty |= app.dispatch();
        if app.plans.iter().any(|(_, h)| h.is_finished()) {
            app.plans_done();
            dirty = true;
        }
        dirty |= app.release_done();
        dirty |= app.models_done();
        let asking = app.s.pending().map(|(_, p)| p.id.clone());
        if asking != app.asked.as_ref().map(|(id, _)| id.clone()) {
            app.asked = asking.map(|id| (id, Instant::now()));
            app.choice = 0;
            dirty = true;
        }
        while let Ok((applied, (lines, pending))) = news.try_recv() {
            lines.iter().for_each(|line| app.say(line));
            dirty = true;
            if applied {
                app.s.hold = None;
            } else if auto && app.s.running() == 0 && pending.iter().any(|u| u.unattended) {
                // Updates start only with no job running, and no job starts until they finish.
                let names: Vec<&str> = pending.iter().filter(|u| u.unattended).map(|u| u.agent.name()).collect();
                app.s.hold = Some(format!("updating {}", names.join(", ")));
                app.say(format!("  updating {}; jobs wait until it finishes (/update auto off: not next time)",
                                names.join(", ")));
                let (done, state) = (notes.clone(), ringo_cli::state_root().join("engine-versions.json"));
                std::thread::spawn(move || {
                    let _ = done.send((true, crate::updates::apply_unattended(pending, &state)));
                });
                continue;
            } else if auto && pending.iter().any(|u| u.unattended) {
                app.say("  jobs were already running, so the automatic update waits: /update when they finish");
            }
            app.pending = Some(pending);
        }
        if app.update {
            close(&mut terminal); // the updates run in the plain terminal, so their prompts and progress show
            println!("Updating agents; the shell comes back when they finish.");
            end_signals(false);
            let (lines, left) = crate::updates::apply_here(app.pending.take().unwrap_or_default());
            end_signals(true);
            match open() {
                Ok(t) => terminal = t,
                Err(_) => break,
            }
            lines.iter().for_each(|line| app.say(line));
            app.pending = Some(left);
            app.update = false;
            dirty = true;
            if let Some(line) = app.check_pipelines() {
                app.say(line);
            }
        }
        if let Ok(news) = update_news.try_recv() {
            updating = false;
            if let Some((line, bin)) = news.filter(|(l, _)| app.updated.as_ref() != Some(l)) {
                app.say(&line);
                app.updated = Some(line);
                app.staged = Some(bin).filter(|b| b.is_file());
                dirty = true;
            }
        }
        if !updating && updated_at.elapsed() >= Duration::from_secs(3600) {
            (updating, updated_at) = (true, Instant::now());
            update_ringo(updated.clone(), exe.clone());
        }
        app.feed();
        dirty |= app.settle();
        dirty |= app.fitted.len() != app.out.len();
        if app.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST) {
            app.toast = None;
            dirty = true;
        }
        if app.look() != app.drawn_look {
            dirty = true; // the cat's next frame, or its next state
        }
        if app.s.running() > 0 && tick.elapsed() >= Duration::from_millis(SPIN_MS as u64) {
            dirty = true; // the spinner and the running clocks
            tick = Instant::now();
        }
    }
    app.feed();
    close(&mut terminal);
    app.s.shutdown();
    // a shell with no turn of its own leaves the conversation it went on from as it was, unless it restarts
    let kept = (!app.s.jobs.is_empty() || app.restart).then(|| app.keep()).flatten();
    let saved = app.save();
    // an update goes in place now that this shell is done with its version
    let installed = app.staged.as_ref().map(|bin| crate::updates::install(&exe, bin));
    // Written, not printed: a shell whose terminal closed has nowhere to print, and a print that fails panics.
    use std::io::Write;
    if let Some(Err(e)) = &installed {
        let _ = writeln!(std::io::stderr(), "ringo: the update was not installed: {e}");
    }
    if app.restart {
        let next = match (&installed, &app.staged) { (Some(Err(_)), Some(bin)) => bin.clone(), _ => exe.clone() };
        return match kept {
            Some(kept) => restart(&next, &kept),
            None => {
                eprintln!("ringo: could not restart (this conversation could not be kept); run ringo --resume");
                ExitCode::from(1)
            }
        };
    }
    let resume = if kept.is_some() { "; ringo --resume goes on with it" } else { "" };
    let saved = saved.map_or(String::new(), |p| format!("; the conversation is saved in {}{resume}", p.display()));
    let _ = writeln!(stdout(), "ringo: {} tokens this session{saved}", fmt_tokens(app.s.tokens()));
    ExitCode::SUCCESS
}

/// The installed `ringo` takes this terminal over (the same process on Unix), handed the agents' conversations, what
/// each had yet to be told and this one's text, so an update applies without the conversation starting over.
fn restart(exe: &Path, kept: &Path) -> ExitCode {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--resume").arg(kept);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec(); // returns only if it failed
        eprintln!("ringo: could not restart ({}: {e}); run ringo again", exe.display());
        ExitCode::from(1)
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!("ringo: could not restart ({}: {e}); run ringo again", exe.display());
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_signal_to_end_reaches_the_loop_instead_of_killing_the_shell() {
        end_signals(true);
        // SAFETY: raises signals this process now handles; unhandled, either would end the test run here.
        unsafe { libc::raise(libc::SIGTERM) };
        assert!(ENDED.swap(false, Ordering::SeqCst), "kill: the loop sees it and ends as /quit does");
        unsafe { libc::raise(libc::SIGHUP) };
        assert!(ENDED.swap(false, Ordering::SeqCst), "and so for the terminal closing");
        end_signals(false);
    }

    #[test]
    fn a_reply_being_written_closes_its_open_marks() {
        let c = |t: &str| closed(t.to_string());
        assert_eq!(c("depends on the **load fact"), "depends on the **load fact**");
        assert_eq!(c("Python's built-in `dic "), "Python's built-in `dic` ");
        assert_eq!(c("a **b** and `c`"), "a **b** and `c`", "closed marks stay as they are");
        assert_eq!(c("`2 ** 3` is"), "`2 ** 3` is", "a code span's stars are not bold");
        assert_eq!(c("**a `2 ** 3` b"), "**a `2 ** 3` b**");
        assert_eq!(c("text **"), "text ", "nothing after the mark yet: left out (`****` would be a rule)");
        assert_eq!(c("Python's built-in `"), "Python's built-in ");
        assert_eq!(c("```py\nx = 2 ** 3 `"), "```py\nx = 2 ** 3 `", "inside a code block nothing is closed");
        assert_eq!(c("```py\nx = 1\n```\nnow **this"), "```py\nx = 1\n```\nnow **this**");
        assert_eq!(c("**bold**\n"), "**bold**\n");
    }

    #[test]
    #[allow(non_snake_case)] // the names the agents had as enum variants, so this test reads as before
    fn parses_routes_and_commands() {
        let named = |n: &str| Agent::from_name(n).unwrap();
        let (Codex, Claude, Grok) = (named("codex"), named("claude"), named("grok"));
        let all = [Codex, Claude, Grok];
        let parse = |line: &str, _: Agent| super::parse(line, &all);
        assert_eq!(parse("why is it slow?", Codex), Ok(Cmd::Talk("why is it slow?".into())));
        assert_eq!(parse("/ask why is it slow?", Codex), Ok(Cmd::Ask("why is it slow?".into())));
        assert!(parse("/ask", Codex).is_err());
        assert_eq!(parse("?", Codex), Ok(Cmd::Keys));
        assert_eq!((parse("/undo", Codex), parse("/copy", Codex), parse("/clear", Codex)),
                   (Ok(Cmd::Undo), Ok(Cmd::Copy), Ok(Cmd::Clear)));
        assert!(parse("   ", Codex).is_err());
        assert_eq!(parse("/update", Codex), Ok(Cmd::Update(None)));
        assert_eq!(parse("/update auto on", Codex), Ok(Cmd::Update(Some(true))));
        assert_eq!(parse("/update auto off", Codex), Ok(Cmd::Update(Some(false))));
        assert!(parse("/update auto", Codex).is_err());
        assert_eq!(parse("/update approve", Codex), Ok(Cmd::Approve));
        assert!(parse("/update approve now", Codex).is_err());
        assert_eq!(parse("@codex why is it slow?", Codex), Ok(Cmd::Agents(vec![(Codex, false)], "why is it slow?".into())));
        assert_eq!(parse("@claude! fix it", Codex), Ok(Cmd::Agents(vec![(Claude, true)], "fix it".into())));
        assert_eq!(parse("@codex @claude! do x", Grok), Ok(Cmd::Agents(vec![(Codex, false), (Claude, true)], "do x".into())));
        assert_eq!(parse("@all compare", Codex),
                   Ok(Cmd::Agents(vec![(Codex, true), (Claude, true), (Grok, true)], "compare".into())),
                   "every agent, each in its own copy");
        assert_eq!(parse("@all! compare", Codex), parse("@all compare", Codex));
        assert!(parse("@nobody x", Codex).unwrap_err().contains("agy"));
        // an agent alone: plain lines go to it; one not set up, one in its own copy, or two, say what to do
        assert_eq!(parse("@codex", Codex), Ok(Cmd::Switch(Codex)));
        assert_eq!(parse("@agy", Codex), Err("agy is not set up here; /setup adds it".into()));
        assert!(parse("@claude!", Codex).is_err() && parse("@codex @claude", Codex).is_err());
        assert_eq!(parse("/review #3", Codex), Ok(Cmd::ReviewJob(3)));
        assert_eq!(parse("/review main", Codex), Ok(Cmd::ReviewRepo("main".into(), "HEAD".into())));
        assert_eq!(parse("/apply 2", Codex), Ok(Cmd::Apply(2)));
        assert_eq!(parse("/design a cache for X", Codex), Ok(Cmd::Design("a cache for X".into())));
        assert!(parse("/frobnicate", Codex).is_err());
        assert_eq!(parse("/mode", Codex), Ok(Cmd::Mode(None)));
        assert_eq!(parse("/mode auto", Codex), Ok(Cmd::Mode(Some(Mode::Auto))));
        assert_eq!(parse("/mode read", Codex), Ok(Cmd::Mode(Some(Mode::Read))));
        assert!(parse("/mode yolo", Codex).is_err());
        assert_eq!(parse("/auto", Codex), Ok(Cmd::Auto));
        for manual in ["/usrman", "/instructions", "/manual", "/help", "/?"] {
            assert_eq!(parse(manual, Codex), Ok(Cmd::Help), "{manual}");
        }
        assert_eq!(parse("line one\nline two", Codex), Ok(Cmd::Talk("line one\nline two".into())));
    }

    #[test]
    fn a_paste_keeps_its_lines_and_loses_what_would_spread_it_out() {
        assert_eq!(clean("a\r\nb\rc\td\x1b[31me"), "a\nb\nc    d[31me");
    }

    #[test]
    fn the_input_wraps_and_the_cursor_follows() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(input_rows(&chars(""), 0, 10), (vec![String::new()], (0, 0)));
        assert_eq!(input_rows(&chars("abcdef"), 6, 4), (vec!["abcd".into(), "ef".into()], (1, 2)));
        assert_eq!(input_rows(&chars("abcd"), 4, 4), (vec!["abcd".into(), String::new()], (1, 0)));
        assert_eq!(input_rows(&chars("abcde"), 4, 4), (vec!["abcd".into(), "e".into()], (1, 0)));
        assert_eq!(input_rows(&chars("ab\ncd"), 3, 10), (vec!["ab".into(), "cd".into()], (1, 0)));
        assert_eq!(input_rows(&chars("ab\ncd"), 2, 10).1, (0, 2));
        // a wide character takes two columns and never splits across rows
        assert_eq!(input_rows(&chars("a漢字"), 3, 4), (vec!["a漢".into(), "字".into()], (1, 2)));
    }

    #[test]
    fn up_and_down_move_between_lines_before_history() {
        let input: Vec<char> = "abc\nd\nefgh".chars().collect();
        assert_eq!(vertical(&input, 2, true), None); // the first line: history
        assert_eq!(vertical(&input, 9, true), Some(5)); // column 2 of "efgh" to the end of "d"
        assert_eq!(vertical(&input, 5, true), Some(1));
        assert_eq!(vertical(&input, 2, false), Some(5));
        assert_eq!(vertical(&input, 5, false), Some(7));
        assert_eq!(vertical(&input, 9, false), None); // the last line: history
    }

    #[test]
    fn edits_from_the_home_folder_point_to_their_one_project() {
        let top = std::env::temp_dir().join(format!("ringo-home-{}", std::process::id()));
        for d in ["code/bot/.git", "code/bot/src", "code/other/.git", "notes"] {
            std::fs::create_dir_all(top.join(d)).unwrap();
        }
        let set = |files: &[&str]| files.iter().map(|f| f.to_string()).collect::<std::collections::BTreeSet<String>>();
        assert_eq!(one_project(&top, &set(&["code/bot/src/a.rs", "code/bot/README.md"])), Some(top.join("code/bot")));
        assert_eq!(one_project(&top, &set(&["code/bot/a.rs", "code/other/b.rs"])), None, "two projects");
        assert_eq!(one_project(&top, &set(&["code/bot/a.rs", "notes/c.md"])), None, "one file in no project");
        assert_eq!(one_project(&top, &set(&[])), None, "no edits");
        std::fs::create_dir_all(top.join(".git")).unwrap();
        assert_eq!(one_project(&top, &set(&["notes/c.md"])), None, "the home folder's own repository is not a project");
        let _ = std::fs::remove_dir_all(&top);
    }

    /// A shell on a session in a fresh folder outside git, for drawing its screen.
    fn app(agents: &[&str]) -> App {
        let dir = std::env::temp_dir().join(format!("ringo-tui-test-{}-{}", std::process::id(), agents.join("-")));
        let (cwd, state) = (dir.join("folder"), dir.join("state"));
        std::fs::create_dir_all(&cwd).unwrap();
        let s = Session::with_state_home(&cwd, &state, &dir).unwrap();
        let agents = agents.iter().map(|n| Agent::from_name(n).unwrap()).collect();
        App::new(s, agents, "folder".into(), Theme::named("mono", crate::theme::Depth::Mono).unwrap(), Vec::new())
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let b = terminal.backend().buffer();
        b.content.chunks(b.area.width as usize)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn the_screen_holds_the_head_the_conversation_and_the_input() {
        let mut a = app(&["codex", "claude"]);
        assert_eq!(a.main.name(), "claude", "claude takes code when it is set up");
        a.welcome();
        let rows = screen(&mut a, 80, 24);
        assert!(rows[0].starts_with(" ▲▲ ringo  folder") && rows[0].ends_with("ask mode · 0 tokens"), "{rows:#?}");
        assert!(rows[1].chars().all(|c| c == '─') && rows[22].chars().all(|c| c == '─'), "no side panel at 80: {rows:#?}");
        // an empty conversation: nothing at the top, the cat and where to start in the middle (no side panel at 80)
        assert!(a.out.is_empty() && rows[2].is_empty(), "the opening lines are in /usrman now: {rows:#?}");
        let middle = rows.iter().position(|r| r.trim() == "▲▲ ringo").expect("the name in the middle");
        assert!(rows[middle - 2].trim() == "( •.• )" && rows[middle + 2].trim() == "Tell Ringo what you want done, in your own words."
                && rows[middle + 3].trim() == "/usrman how it works · / commands · ? keys", "{rows:#?}");
        let at = rows[middle].find('▲').unwrap();
        assert!((at as i32 - (80 - 8) / 2).abs() <= 1, "centred: {rows:#?}");
        assert!(a.intro().contains("/undo takes back its last change"), "outside git too, in Ringo's own store");
        assert!(rows[23].starts_with(" claude ❯ Tell Ringo what you want to do") && rows[23].ends_with("⇧⇥ mode · ? keys"),
                "{rows:#?}");
        a.echo("hello");
        assert!(!screen(&mut a, 80, 24).iter().any(|r| r.contains("/usrman how it works")), "it goes once you speak");
        assert!(screen(&mut a, 80, 24)[2].starts_with(" you  hello"));
        let narrow = screen(&mut a, 30, 24);
        for row in &narrow {
            assert!(cols(row) <= 30, "{row:?}");
        }
        assert!(narrow[0].contains("ringo") && !narrow[0].contains("mode"), "a narrow head drops its right end: {narrow:#?}");
        assert_eq!(first_choice(&[Agent::from_name("grok").unwrap(), Agent::from_name("codex").unwrap()]).name(), "codex");
    }

    #[test]
    fn the_home_folder_says_to_start_in_the_project() {
        let said = |a: &App| a.out.iter().map(|o| format!("{o:?}")).collect::<String>();
        let mut a = app(&["claude"]);
        a.welcome();
        assert!(!a.s.at_home && !said(&a).contains("open in your home folder"));
        let dir = std::env::temp_dir().join(format!("ringo-tui-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Session::with_state_home(&dir, &dir.join("state"), &dir).unwrap();
        let mut h = App::new(s, vec![Agent::from_name("claude").unwrap()], "~".into(),
                             Theme::named("mono", crate::theme::Depth::Mono).unwrap(), Vec::new());
        h.welcome();
        assert!(h.s.at_home && said(&h).contains("open in your home folder") && said(&h).contains("cd into the project"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wide_screen_has_the_side_panel_with_the_cat() {
        let mut a = app(&["codex", "claude"]);
        a.welcome();
        let rows = screen(&mut a, 120, 30);
        let split = rows[1].chars().position(|c| c == '┬').expect("the rule meets the panel's bar");
        assert_eq!(split, 120 - PANEL_W as usize - 1);
        assert_eq!(rows[28].chars().position(|c| c == '┴'), Some(split));
        let side: Vec<String> = rows[2..28].iter().map(|r| r.chars().skip(split + 2).collect()).collect();
        assert_eq!(side[0..3], mascot("idle")[0].iter().map(|r| r.to_string()).collect::<Vec<_>>()[..], "{side:#?}");
        assert!(side.contains(&"AGENTS".to_string()), "{side:#?}");
        assert!(side.iter().any(|r| r.starts_with("○ claude") && r.contains("talking to you")), "{side:#?}");
        assert!(side.iter().any(|r| r.starts_with("○ codex") && r.ends_with("idle")), "{side:#?}");
        assert!(rows[2..28].iter().all(|r| r.chars().nth(split) == Some('│')), "the bar runs the panel's height");
        let long = a.panel(31, 30).into_iter().find(|l| l.spans.len() == 3 && l.spans[1].content.contains("claude")).unwrap();
        assert!(plain(&[long])[0].ends_with("talking to you"), "the state stays whole");
        // each plan's limits as last read, a gauge a window, the agent named once; the middle has no second cat
        let claude = Agent::from_name("claude").unwrap();
        a.limits = vec![(claude, ringo_cli::Plan { name: "max".into(), windows: vec![
            ringo_cli::Window { name: "5 hours".into(), percent: 50.0, resets: None },
            ringo_cli::Window { name: "weekly scoped".into(), percent: 100.0, resets: None }], notes: Vec::new() })];
        let side = plain(&a.panel(31, 40));
        let at = side.iter().position(|r| r == "LIMITS").expect("a limits section");
        assert_eq!(side[at + 1], format!("claude 5h {}{}  50%", "▰".repeat(6), "▱".repeat(6)));
        assert_eq!(side[at + 2], format!("       ws {} 100%", "▰".repeat(12)));
        let rows = screen(&mut a, 120, 30);
        assert!(rows.iter().filter(|r| r.contains("( •.• )")).count() == 1 && rows.iter().any(|r| r.contains("▲▲ ringo  ")
                || r.trim_start().starts_with("▲▲ ringo")), "{rows:#?}");
    }

    #[test]
    fn the_cat_has_every_state_and_fits_the_panel() {
        let rows = mascot("idle")[0].len();
        for state in ["idle", "working", "asking", "done", "failed"] {
            let frames = mascot(state);
            assert!(!frames.is_empty(), "{state}");
            for frame in frames {
                assert_eq!(frame.len(), rows, "{state}: every frame has the same rows");
                assert!(frame.iter().all(|r| cols(r) <= PANEL_W as usize - 4), "{state}: {frame:?}");
            }
        }
        assert_eq!(mascot("nonesuch"), mascot("idle"), "a state the art does not draw shows idle");
        assert!(mascot("working").len() > 1, "the working cat moves");
    }

    #[test]
    fn a_step_says_how_it_went() {
        let theme = Theme::named("mono", crate::theme::Depth::Mono).unwrap();
        let row = |text: &str, width: usize| plain(&[step_row(text, width, &theme)])[0].clone();
        assert_eq!(row("Read a.rs · 2 lines", 40), format!("  ✓ read a.rs{}2 lines", " ".repeat(36 - 9 - 7)));
        assert_eq!(cols(&row("Read a.rs · 2 lines", 200)), 68, "results line up at 64 columns on a wide pane");
        assert!(row("✗ $ make test · exit 2", 40).starts_with("  ✗ run make test") && row("✗ $ make test · exit 2", 40).ends_with("exit 2"));
        assert_eq!(row("◌ Edit a.rs", 40), "  ◌ edit a.rs");
        assert_eq!(row("· view_file a.rs", 40), "  · view_file a.rs", "only started: no claim about the outcome");
        assert_eq!(cols(&row(&format!("Grep {} · Found 3 files", "x".repeat(80)), 30)), 30, "a long step keeps to one row");
    }

    #[cfg(unix)]
    #[test]
    fn a_reply_grows_as_it_is_written_and_the_whole_one_replaces_it() {
        use std::os::unix::fs::PermissionsExt;
        let mut a = app(&["claude"]);
        let fake = std::env::temp_dir().join(format!("ringo-tui-fake-claude-{}", std::process::id()));
        std::fs::write(&fake, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        a.s.programs.insert(a.main, fake);
        a.talk(a.main, "hi".into(), "hi".into()).unwrap();
        let replies = |a: &App| a.out.iter().filter_map(|o| match o { Out::Reply(t) => Some(t.clone()), _ => None })
            .collect::<Vec<_>>();
        // time passing with no new piece
        let pause = |a: &mut App, by: Duration| {
            let s = a.streams.get_mut(&1).unwrap();
            (s.last, s.due) = (s.last - by, s.due.map(|at| at - by));
            a.settle()
        };
        a.s.jobs[0].fresh = vec![Said::Partial("Hel".into()), Said::Partial("lo".into())];
        a.feed();
        assert_eq!(replies(&a), [""], "the word being written waits for its end");
        a.s.jobs[0].fresh = vec![Said::Partial(", wor".into())];
        a.feed();
        assert!(screen(&mut a, 60, 12).iter().any(|r| r.trim() == "Hello,"), "and shows once it has ended");
        assert!(pause(&mut a, WORD_HOLD), "a pause shows what was kept back");
        assert!(screen(&mut a, 60, 12).iter().any(|r| r.trim() == "Hello, wor"));
        assert!(!a.settle(), "and then there is nothing left to show");
        a.s.jobs[0].fresh = vec![Said::Partial("l".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, worl"], "what has shown is not taken back when its word goes on");
        a.s.jobs[0].fresh = vec![Said::Partial("d\r".into()), Said::Partial("\nmore".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, world\n"], "a line end split across pieces stays one");
        a.s.jobs[0].fresh = vec![Said::Step("Read a.rs · 2 lines".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, world\nmore"], "a step means the pieces have stopped: nothing waits behind it");
        a.s.jobs[0].fresh = vec![Said::Reply("Hello, world.".into()), Said::Step("Read a.rs · 2 lines".into()),
                                 Said::Partial("Next".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, world.", ""], "the whole reply replaces its pieces; the next block opens anew");
        // kept back on screen, it was still received: the saved conversation has it, and a table as far as it got
        a.s.jobs[0].fresh = vec![Said::Partial(" up:\n\n| Name | Size |\n|---|---|\n| alpha | 1".into())];
        a.feed();
        assert_eq!(replies(&a)[1], "Next up:\n\n*a table is being written…*");
        let saved = std::fs::read_to_string(a.save().unwrap()).unwrap();
        assert!(saved.contains("Next up:") && saved.contains("alpha") && !saved.contains("is being written"), "{saved}");
        // a turn that ends mid-reply shows all it wrote
        a.s.kill(1).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while a.s.jobs[0].running() && Instant::now() < deadline {
            a.s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        a.feed();
        assert_eq!(replies(&a), ["Hello, world.", "Next up:\n\n| Name | Size |\n|---|---|\n| alpha | 1"]);
        a.s.shutdown();
    }

    /// A stand-in Claude Code for a talk: each run keeps the message it was sent (`.in.N`) and its arguments, waits a
    /// moment, and replies with its Nth word of `replies` (the last one from then on).
    #[cfg(unix)]
    fn talker(name: &str, replies: &[&str]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("ringo-tui-talker-{}-{name}", std::process::id()));
        let cases: Vec<String> = replies.iter().enumerate().map(|(i, r)| format!("{}) reply='{r}';;", i + 1)).collect();
        let script = r##"#!/bin/sh
n=$(($(cat "$0.n" 2>/dev/null || echo 0) + 1)); echo $n > "$0.n"
for a in "$@"; do printf '%s\n' "$a"; done > "$0.argv"
head -n 1 > "$0.in.$n"
sleep 0.15
echo '{"type":"system","subtype":"init","session_id":"s-NAME","apiKeySource":"none"}'
case $n in CASES *) reply='LAST';; esac
printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"$reply\"}],\"usage\":{\"input_tokens\":100,\"output_tokens\":5}}}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$reply\"}"
"##.replace("NAME", name).replace("CASES", &cases.join(" ")).replace("LAST", replies.last().unwrap());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(format!("{}.n", path.display()));
        path
    }

    /// What the stand-in's Nth run was sent.
    #[cfg(unix)]
    fn sent(talker: &Path, n: usize) -> String {
        let line = std::fs::read_to_string(format!("{}.in.{n}", talker.display())).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["message"]["content"].as_str().unwrap().to_string()
    }

    /// Runs the shell's loop until the agents' talk is over; `each` sees the app once a pass.
    #[cfg(unix)]
    fn until_talked(a: &mut App, mut each: impl FnMut(&mut App)) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while a.talking.is_some() {
            assert!(Instant::now() < deadline, "the talk did not end");
            a.s.pump();
            a.feed();
            each(a);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_agents_talk_a_topic_over_among_themselves() {
        let mut a = app(&["claude", "codex", "grok"]);
        let (claude, codex) = (Agent::from_name("claude").unwrap(), Agent::from_name("codex").unwrap());
        assert_eq!((parse("/discuss", &[claude]), parse("/discuss split the parser?", &[claude]), parse("/talk @codex @claude why", &[claude])),
                   (Ok(Cmd::Discuss(vec![], String::new())), Ok(Cmd::Discuss(vec![], "split the parser?".into())),
                    Ok(Cmd::Discuss(vec![codex, claude], "why".into()))));
        assert!(parse("/discuss @nobody x", &[claude]).unwrap_err().contains("no agent nobody"));
        // a specialist of the owner's takes part as any agent does; both stand-ins speak Claude Code's protocol
        let file = a.s.home.join("reviewer.md");
        std::fs::write(&file, "You review plans.\n").unwrap();
        let entry = serde_json::json!({"name": "talk-reviewer", "adapter": "claude", "program": "claude", "read": true, "instructions": file});
        let reviewer = Agent::register(ringo_cli::Spec::from_json(&entry, true).unwrap()).unwrap();
        // refused: nothing to talk over yet, one agent, an agent that only works in its own copy, /separate
        assert!(a.discuss(vec![], String::new()).unwrap_err().contains("say what they should talk over"));
        assert!(a.discuss(vec![claude], "x".into()).unwrap_err().contains("there is only claude"));
        assert!(a.discuss(vec![claude, Agent::from_name("grok").unwrap()], "x".into()).unwrap_err().contains("works only in its own copy"));
        assert!(a.discuss(vec![claude, reviewer], "x".into()).unwrap_err().contains("talk-reviewer is not set up here"));
        a.agents = vec![claude, reviewer];
        a.s.separate = true;
        assert!(a.discuss(vec![], "x".into()).unwrap_err().contains("/together first"));
        a.s.separate = false;

        // neither runs out of things to say: each speaks TALK_ROUNDS times, and that is all
        let (one, two) = (talker("one", &["Split the parser first."]), talker("two", &["The lexer first, I think."]));
        a.s.programs.insert(claude, one.clone());
        a.s.programs.insert(reviewer, two.clone());
        a.s.mode = Mode::Auto;
        a.send("/discuss should we split the parser?");
        assert_eq!(a.s.mode, Mode::Auto, "the session's mode is its own");
        let rows = screen(&mut a, 180, 30).join("\n");
        assert!(rows.contains("claude and talk-reviewer talk it over, one at a time, looking but not touching · at most 2 turns each"), "{rows}");
        assert!(rows.contains("the agents are talking · a line of yours joins in · Esc stops them"), "{rows}");
        assert!(a.discuss(vec![], "y".into()).unwrap_err().contains("talking already"));
        // while they talk: the conversation they share stays shared, and no one in the talk is taken away
        assert!(a.run(Cmd::Separate(true), "").unwrap_err().contains("the agents are talking") && !a.s.separate);
        assert!(a.run(Cmd::Agent(Some(OwnAgent::Remove("talk-reviewer".into()))), "").unwrap_err().contains("is in the talk under way"));
        until_talked(&mut a, |_| {});
        assert_eq!(a.s.jobs.len(), 2 * TALK_ROUNDS, "two agents, TALK_ROUNDS turns each, and not one more");
        assert!(a.s.jobs.iter().all(|j| j.talk && j.status == Status::Done));
        let kinds: Vec<Agent> = a.s.jobs.iter().filter_map(|j| match j.kind { Kind::Agent { agent, .. } => Some(agent), _ => None }).collect();
        assert_eq!(kinds, [claude, reviewer, claude, reviewer], "in turn, the one the owner talks to first");
        // the topic once; each hears what the other said, whole, and is asked to answer it
        assert!(sent(&one, 1).starts_with("I want you and talk-reviewer to talk this over; I am reading along: should we split the parser?\n\nYour reply is shown to them"));
        let (second, third) = (sent(&two, 1), sent(&one, 2));
        assert!(second.contains("claude: Split the parser first.") && second.contains("should we split the parser?")
                && second.contains("Go on with the talk: answer what claude said above."), "{second}");
        assert!(third.contains("talk-reviewer: The lexer first, I think.") && !third.contains("should we split the parser?")
                && third.contains("If you have nothing to add, reply with the one word AGREED."), "{third}");
        // a talk looks and does not touch, whatever the mode
        let argv = std::fs::read_to_string(format!("{}.argv", one.display())).unwrap();
        assert!(argv.contains("--permission-prompts\nnone") && argv.lines().any(|l| l == "Edit"), "{argv}");
        let rows = screen(&mut a, 180, 40);
        let text = rows.join("\n");
        assert!(text.contains("the talk is over: each has spoken twice · 4 turns") && text.contains("/discuss goes on with it"), "{text}");
        assert!(!text.contains("✓ done") && !text.contains("told claude"), "a talk's turns have no rows of their own: {text}");
        assert_eq!(rows.iter().filter(|r| r.contains("The lexer first, I think.")).count(), 2, "{text}");
        if std::env::var_os("RINGO_MEASURE_SHOW").is_some() {
            eprintln!("{}", screen(&mut a, 100, 30).join("\n"));
        }

        // a round that adds nothing ends it; alone, /discuss is about the last turn, which the others answer first
        a.main = reviewer;
        let (one, two) = (talker("one", &["AGREED."]), talker("two", &["never asked"]));
        a.s.programs.insert(claude, one.clone());
        a.s.programs.insert(reviewer, two.clone());
        a.send("/discuss");
        until_talked(&mut a, |_| {});
        assert_eq!(a.s.jobs.len(), 2 * TALK_ROUNDS + 1, "one agreed of two: over");
        assert!(sent(&one, 1).contains("I want you and talk-reviewer to talk the last turn above over") && sent(&one, 1).contains("do not try to reach them"));
        assert!(screen(&mut a, 180, 40).join("\n").contains("the talk is over: no one has more to add · 1 turn ·"));

        // a line of the owner's joins in: the next one hears it, and each may answer once more
        let (one, two) = (talker("one", &["One thing more."]), talker("two", &["And another."]));
        a.s.programs.insert(claude, one.clone());
        a.s.programs.insert(reviewer, two.clone());
        let before = a.s.jobs.len();
        a.send("/discuss @claude @talk-reviewer what about the tests?");
        let mut joined = false;
        until_talked(&mut a, |a| {
            if !joined && a.talking.as_ref().is_some_and(|t| t.jobs.len() == 2 * TALK_ROUNDS) {
                joined = true;
                a.send("keep the old tests");
                assert!(a.queue.is_empty(), "it joins the talk, and does not wait for a turn of its own");
            }
        });
        assert_eq!(a.s.jobs.len() - before, 2 * TALK_ROUNDS + 2, "the owner's line adds one round, and only one");
        let heard = sent(&one, 3);
        assert!(heard.contains("I add: keep the old tests") && heard.contains("answer what talk-reviewer said above, and what I add"), "{heard}");
        assert!(screen(&mut a, 180, 60).join("\n").contains("↳ claude hears this next, and the talk goes on"));

        // stopped: Esc ends the turn and the talk with it
        a.send("/discuss once more");
        let id = a.holding().unwrap();
        a.stop(id);
        until_talked(&mut a, |_| {});
        assert!(screen(&mut a, 180, 60).join("\n").contains("the talk is over: stopped · 1 turn"));
        assert!(agreed(" Agreed.\n") && agreed("AGREED") && !agreed("AGREED, but the lexer worries me") && !agreed(""));
        // /clear takes the conversation they talked in: the turn now running is the talk's last
        let before = a.s.jobs.len();
        a.send("/discuss and again");
        assert!(a.run(Cmd::Clear, "").unwrap().unwrap().ends_with("the agents' talk ends with the turn now running"));
        assert!(a.talking.is_none());
        let deadline = Instant::now() + Duration::from_secs(20);
        while a.s.running() > 0 && Instant::now() < deadline {
            a.s.pump();
            a.feed();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!((a.s.running(), a.s.jobs.len() - before), (0, 1), "no one answers a talk that was cleared away");
    }

    #[test]
    fn a_specialist_of_the_owners_own_is_an_agent_like_any_other() {
        let mut a = app(&["codex", "claude", "agy"]);
        let (codex, claude) = (Agent::from_name("codex").unwrap(), Agent::from_name("claude").unwrap());
        let add = |name: &str, file: Option<&str>, base: Option<&str>, skills: bool| {
            Cmd::Agent(Some(OwnAgent::Add { name: name.into(), file: file.map(String::from), base: base.map(String::from), skills }))
        };
        assert_eq!((parse("/agent", &[claude]), parse("/agent add @billing notes.md", &[claude]), parse("/agent add billing codex notes.md skills", &[claude]),
                    parse("/agent remove billing", &[claude])),
                   (Ok(Cmd::Agent(None)), Ok(add("billing", Some("notes.md"), None, false)), Ok(add("billing", Some("notes.md"), Some("codex"), true)),
                    Ok(Cmd::Agent(Some(OwnAgent::Remove("billing".into()))))));
        assert!(parse("/agent add", &[claude]).is_err() && parse("/agent add billing a.md b.md", &[claude]).is_err()
                && parse("/agent drop billing", &[claude]).is_err());
        // nothing of the owner's yet: how to make one, and the agents they made for Claude Code, found by name
        let made = a.s.home.join(".claude/agents");
        std::fs::create_dir_all(&made).unwrap();
        std::fs::write(made.join("shop-billing.md"), "---\nname: shop-billing\n---\nYou know the billing code.\n").unwrap();
        a.run(Cmd::Agent(None), "").unwrap();
        let rows = screen(&mut a, 100, 16).join("\n");
        assert!(rows.contains("none yet. A specialist is a file of instructions") && rows.contains("/agent add shop-billing"), "{rows}");
        // refused: a name that is taken or no name, a base that cannot carry instructions, skills off Claude, no file
        for (cmd, why) in [(add("claude", None, None, false), "one of Ringo's own agents"), (add("a b", None, None, false), "cannot be an agent's name"),
                           (add("billing", Some("shop-billing"), Some("agy"), false), "agy is not one"),
                           (add("billing", Some("shop-billing"), Some("codex"), true), "skills are Claude Code's"),
                           (add("billing", None, None, false), "no instructions file `billing`")] {
            assert!(a.run(cmd, "").unwrap_err().contains(why), "{why}");
        }
        // made: on claude unless another is named, from the Claude Code agent of that name
        let said = a.run(add("shop-billing", None, None, false), "").unwrap().unwrap();
        assert!(said.starts_with("  shop-billing is an agent here now: claude with ") && said.contains("(26 characters, about 6 tokens more"), "{said}");
        let own = Agent::from_name("shop-billing").unwrap();
        assert_eq!((a.agents.last(), own.spec().adapter, own.spec().read, own.spec().skills), (Some(&own), ringo_cli::Adapter::Claude, true, false));
        assert_eq!(parse("@shop-billing why is the invoice late?", &a.agents), Ok(Cmd::Agents(vec![(own, false)], "why is the invoice late?".into())));
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(a.s.state().join("agents.json")).unwrap()).unwrap();
        assert_eq!((kept["agents"][3].as_str(), kept["custom"][0]["name"].as_str()), (Some("shop-billing"), Some("shop-billing")), "kept for the next start");
        // it runs on its agent's plan: one read, one gauge
        a.s.programs.insert(claude, std::env::current_exe().unwrap());
        a.s.programs.insert(codex, std::env::current_exe().unwrap());
        a.s.programs.insert(own, std::env::current_exe().unwrap());
        assert_eq!(a.plan_agents(), [codex], "one program, one plan");
        a.run(Cmd::Agent(None), "").unwrap();
        let rows = screen(&mut a, 120, 16).join("\n");
        assert!(rows.contains("shop-billing  on claude · ") && rows.contains("26 characters") && !rows.contains("/agent add shop-billing"), "{rows}");
        // the same name again, and taken away: its file stays
        assert!(a.run(add("shop-billing", None, None, false), "").unwrap_err().contains("you already have an agent called shop-billing"));
        a.main = own;
        assert!(a.run(Cmd::Agent(Some(OwnAgent::Remove("shop-billing".into()))), "").unwrap().unwrap().contains("its instructions file is untouched"));
        assert!(!a.agents.contains(&own) && a.main == codex && made.join("shop-billing.md").is_file());
        assert!(a.run(Cmd::Agent(Some(OwnAgent::Remove("claude".into()))), "").unwrap_err().contains("not an agent of your own"));
    }

    #[test]
    fn each_agent_runs_on_the_model_its_owner_chose() {
        let mut a = app(&["claude", "agy"]);
        let claude = Agent::from_name("claude").unwrap();
        assert_eq!((parse("/model", &[claude]), parse("/model claude opus", &[claude]), parse("/model @claude default", &[claude])),
                   (Ok(Cmd::Model(None)), Ok(Cmd::Model(Some(("claude".into(), "opus".into())))),
                    Ok(Cmd::Model(Some(("claude".into(), "default".into()))))));
        assert!(parse("/model claude", &[claude]).is_err() && parse("/model claude a b", &[claude]).is_err());
        let set = |a: &mut App, agent: &str, model: &str| a.run(Cmd::Model(Some((agent.into(), model.into()))), "");
        assert!(set(&mut a, "claude", "opus").unwrap().unwrap().starts_with("  claude runs on opus from its next turn (saved)"));
        assert_eq!(a.s.models.get(&claude).map(String::as_str), Some("opus"));
        assert_eq!(crate::theme::settings_all()["models"]["claude"], "opus", "remembered for the next start");
        // an agent that takes none, one not set up, a name that is not one, and a model that is not one word
        assert!(set(&mut a, "agy", "x").unwrap_err().contains("chooses its own model"));
        assert!(set(&mut a, "codex", "x").unwrap_err().contains("not set up here"));
        assert!(set(&mut a, "nobody", "x").unwrap_err().contains("no agent nobody: claude, agy"));
        assert!(set(&mut a, "claude", "--bare").unwrap_err().contains("one word") && a.s.models[&claude] == "opus");
        // /model shows who runs on what, and the settings line names the choice
        assert!(a.run(Cmd::Model(None), "").unwrap().unwrap().contains("/ask, /review, /design and /research run on codex-chatgpt"));
        let rows = screen(&mut a, 90, 16);
        assert!(rows.iter().any(|r| r.trim() == "claude  opus  · your choice"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.trim() == "agy     its own  · it takes no model from Ringo"), "{rows:#?}");
        assert!(a.listing.is_none() && a.settings().contains("models claude opus (/model)"), "no codex here to ask for its list");
        // a turn that fails on a chosen model says where the choice came from
        let job = std::env::temp_dir().join(format!("ringo-tui-fake-model-{}", std::process::id()));
        std::fs::write(&job, "#!/bin/sh\nexit 3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&job, std::fs::Permissions::from_mode(0o755)).unwrap();
            a.s.programs.insert(claude, job);
            a.talk(claude, "hi".into(), "hi".into()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while a.s.running() > 0 && Instant::now() < deadline {
                a.s.pump();
                std::thread::sleep(Duration::from_millis(10));
            }
            a.feed();
            let rows = screen(&mut a, 110, 30);
            assert!(rows.iter().any(|r| r.contains("it ran on opus, your choice for claude: /model claude default goes back")), "{rows:#?}");
        }
        assert!(set(&mut a, "claude", "default").unwrap().unwrap().contains("claude runs on its own default again"));
        assert!(a.s.models.is_empty() && crate::theme::settings_all()["models"].get("claude").is_none());
        assert!(a.settings().contains("models each agent's own (/model)"));
        a.s.shutdown();
    }

    #[test]
    fn the_head_says_a_newer_ringo_is_there_until_it_is_applied() {
        let mut a = app(&["claude"]);
        a.pending = Some(Vec::new()); // the agents' own check has answered: none to update
        assert!(!screen(&mut a, 100, 10)[0].contains("update"), "nothing to say yet");
        // one this build cannot apply by itself: the head points to /update, which says how to get it
        a.updated = Some("  Ringo 1a2b3c4 is out (2 new changes). This build does not update itself".into());
        assert!(screen(&mut a, 100, 10)[0].contains("folder  ↑ update available · /update"));
        assert!(a.run(Cmd::Update(None), "/update").unwrap().unwrap().starts_with("  Ringo 1a2b3c4 is out (2 new changes)"));
        // built and waiting: /restart applies it, and the head says so while a turn's hint has the input's corner
        a.staged = Some(PathBuf::from("ringo"));
        let rows = screen(&mut a, 100, 10);
        assert!(rows[0].contains("folder  ↑ update ready · /restart") && rows[0].ends_with("ask mode · 0 tokens"), "{rows:#?}");
        assert!(!screen(&mut a, 40, 10)[0].contains("update"), "a narrow head keeps the folder and the mode");
    }

    #[test]
    fn a_reply_being_written_keeps_back_what_would_change() {
        let now = Duration::ZERO;
        let show = |text: &str| steady(text, now, 0).0;
        // the word being written waits for its end; a pause lets it through, and so does having shown already
        assert_eq!(show("The quick bro"), "The quick ");
        assert_eq!(steady("The quick bro", WORD_HOLD, 0), ("The quick bro".into(), 13, None));
        assert_eq!(steady("The quick bro", now, 11).0, "The quick bro", "what has shown stays");
        assert_eq!(show("\u{6f22}\u{5b57}"), "\u{6f22}\u{5b57}", "wide characters have no spaces to wait for");
        let long = format!("see {}", "x".repeat(HELD_WORD + 1));
        assert_eq!(show(&long), long, "a path or a URL shows as it grows");
        // a link shows once it is whole; a box, or a bracket in code, is no link
        assert_eq!((show("see [the "), show("see [the guide](GUIDE.md")), ("see ".into(), "see ".into()));
        assert_eq!(show("see [the guide](GUIDE.md) for "), "see [the guide](GUIDE.md) for ");
        assert_eq!((show("- [x] done "), show("use `a[i ")), ("- [x] done ".into(), "use `a[i` ".into()));
        let open = format!("the range [0, {} ", "9".repeat(HELD_LINK));
        assert_eq!(show(&open), open, "a bracket that closes nothing does not hold the line back for long");
        // a table shows once a line that is not one of its rows comes; until then one line counts its rows
        let table = "Sizes:\n\n| Name | Size |\n|---|---|\n| a | 1 |\n| b";
        assert_eq!(steady(table, now, 0), ("Sizes:\n\n*a table is being written · 1 row so far…*".into(), 8, Some(TABLE_HOLD)));
        assert_eq!(show("| Name"), "*a table is being written…*");
        assert_eq!(show(&format!("{table} | 2 |\n")), "Sizes:\n\n*a table is being written · 2 rows so far…*");
        assert_eq!(show(&format!("{table} | 2 |\n\nDone. ")), format!("{table} | 2 |\n\nDone. "));
        assert_eq!(steady(table, TABLE_HOLD, 0).0, table, "a long pause shows it as far as it is");
        assert!(steady(table, now, 9).0.starts_with("Sizes:\n\n| Name"), "and once it shows, it stays");
        assert_eq!(show("```\n| a | b"), "```\n| a | ", "in a code block a bar is only a bar");
    }

    /// An agent's reply with what replies have: emphasis, code, a link, a list, a table and a code block.
    const SAMPLE_REPLY: &str = "I found the cause in **`client.rs`**: the request is sent once and a timeout is never retried, \
so one slow response fails the whole sync. See [the retry notes](docs/RETRY.md) for the policy the team agreed on.\n\n\
What I changed:\n\n\
- `send` now retries up to 3 times, backing off 100 ms and then doubling each time\n\
- the new `retry_count` setting defaults to 3, and 0 turns retries off entirely\n\
- a test covers the case where the second attempt succeeds\n\n\
| File | Change | Lines |\n|---|---|---|\n\
| src/client.rs | wrap the send in a retry loop with backoff | +14 −2 |\n\
| src/config.rs | read `retry_count` from the settings file | +6 |\n\
| tests/client_test.rs | a response that fails once and then succeeds | +31 |\n\n\
The loop itself is short:\n\n\
```rust\nfor attempt in 0..=retries {\n    match self.http.send(req.clone()) {\n        Ok(resp) => return Ok(resp),\n\
        Err(e) if attempt == retries => return Err(e),\n        Err(_) => sleep(backoff(attempt)),\n    }\n}\n```\n\n\
All 42 tests pass. Nothing else in the client changed, and `/undo` takes this back if you would rather not keep it.\n";

    #[test]
    fn rows_already_written_do_not_change_as_a_reply_grows() {
        let theme = Theme::named("mono", crate::theme::Depth::Mono).unwrap();
        for width in [48, 78, 118] {
            let (mut text, mut floor, mut prev) = (String::new(), 0, Vec::<String>::new());
            for piece in pieces(SAMPLE_REPLY) {
                text.push_str(&piece);
                let (shown, kept, _) = steady(&text, Duration::ZERO, floor);
                floor = floor.max(kept);
                let cur = plain(&markdown(&shown, width, &theme));
                if let Some(last) = prev.len().checked_sub(1) {
                    assert_eq!(cur.get(..last), Some(&prev[..last]), "{width} columns: a row above the one being written changed");
                    assert!(prev[last].contains("a table is being written") || cur[last].starts_with(prev[last].as_str()),
                            "{width} columns: the row being written took text back: {:?} became {:?}", prev[last], cur[last]);
                }
                prev = cur;
            }
            assert_eq!(plain(&markdown(&steady(&text, WORD_HOLD.max(TABLE_HOLD), floor).0, width, &theme)),
                       plain(&markdown(SAMPLE_REPLY, width, &theme)), "and it ends as the whole reply reads");
        }
    }

    #[test]
    fn scrolled_up_the_rows_stay_put_whatever_is_written_meanwhile() {
        let mut a = app(&["claude"]);
        a.out.push(Out::Reply("first".into())); // an agent's reply, still being written
        for i in 0..30 {
            a.say(format!("line {i}"));
        }
        screen(&mut a, 60, 10);
        a.key(KeyEvent::from(KeyCode::PageUp));
        let before = screen(&mut a, 60, 10);
        assert_eq!(before[7], "line 25");
        // that reply grows by four rows above what is on screen, and a line arrives below it
        a.out[0] = Out::Reply("first\n\nsecond\n\nthird".into());
        a.fitted.clear();
        a.say("line 30");
        let after = screen(&mut a, 60, 10);
        assert_eq!(after[2..8], before[2..8], "what was being read has not moved");
        assert!(after[9].contains("scrolled up · 5 rows below"), "and the count says a line arrived: {after:#?}");
        // text selected there stays selected while the reply above grows again
        let (from, to) = (a.place(a.top + 4), a.place(a.top + 5));
        a.sel = Some(((from.0, from.1, 2), (to.0, to.1, 5)));
        assert_eq!(a.selected(), "ne 24\nline 2");
        a.out[0] = Out::Reply("first\n\nsecond\n\nthird\n\nfourth".into());
        a.fitted.clear();
        assert_eq!((screen(&mut a, 60, 10)[2..8].to_vec(), a.selected()), (before[2..8].to_vec(), "ne 24\nline 2".to_string()));
        a.sel = None;
        a.key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(screen(&mut a, 60, 10)[7], "line 21", "and PgUp still moves from there");
        a.key(KeyEvent::from(KeyCode::End));
        assert_eq!(screen(&mut a, 60, 10)[7], "line 30", "End, with nothing typed, goes back to the end");
        a.key(KeyEvent::from(KeyCode::Home));
        a.key(KeyEvent::from(KeyCode::PageUp)); // past the start is the start
        assert_eq!(screen(&mut a, 60, 10)[2].trim(), "first", "and Home to the start");
        a.input = "x".chars().collect();
        a.cursor = 1;
        a.key(KeyEvent::from(KeyCode::Home));
        assert_eq!((a.cursor, screen(&mut a, 60, 10)[2].trim().to_string()), (0, "first".to_string()), "typed, Home is the line's");
    }

    #[cfg(unix)]
    #[test]
    fn looks_are_counted_on_one_row_and_an_edit_shows_its_lines_in_colour() {
        use std::os::unix::fs::PermissionsExt;
        let mut a = app(&["claude"]);
        a.theme = Theme::named("siamese", crate::theme::Depth::TrueColor).unwrap();
        let fake = std::env::temp_dir().join(format!("ringo-tui-fake-looks-{}", std::process::id()));
        std::fs::write(&fake, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        a.s.programs.insert(a.main, fake);
        a.talk(a.main, "hi".into(), "hi".into()).unwrap();
        a.s.jobs[0].fresh = vec![Said::Looked("read", "Read a.rs".into()), Said::Looked("search", "Grep x".into()),
                                 Said::Looked("read", "Read b.rs".into()),
                                 Said::Step("Edit a.rs · +1 −1\n- let x = 1;\n+ let x = 2;".into()),
                                 Said::Looked("list", "LS src".into())];
        a.feed();
        let rows = screen(&mut a, 70, 16);
        let at = |text: &str| rows.iter().position(|r| r.trim_start().starts_with(text)).unwrap_or_else(|| panic!("{text}: {rows:#?}"));
        let (looked, edit, gone, came, again) = (at("✓ read 2 files, searched 1 time"), at("✓ edit a.rs"), at("- let x = 1;"),
                                                 at("+ let x = 2;"), at("✓ listed 1 folder"));
        assert!(looked < edit && edit + 1 == gone && gone + 1 == came && came < again, "{rows:#?}");
        assert!(rows[edit].ends_with("+1 −1"), "{rows:#?}");
        // removed lines in the failure colour, added ones in the success colour
        let colour = |row: usize, text: &str| {
            let fitted = a.out.iter().flat_map(|o| o.rows(70, &a.theme)).collect::<Vec<_>>();
            fitted.into_iter().find(|l| l.to_string().contains(text)).map(|l| l.spans[0].style).unwrap_or_default()
                .fg.unwrap_or_else(|| panic!("row {row} has no colour"))
        };
        assert_eq!((colour(gone, "let x = 1"), colour(came, "let x = 2")), (a.theme.fail.fg.unwrap(), a.theme.ok.fg.unwrap()));
        a.s.shutdown();
    }

    #[test]
    fn a_queued_compaction_checks_its_conversation_again() {
        let mut a = app(&["claude"]);
        assert!(a.run(Cmd::Compact, "/compact").unwrap_err().contains("no conversation"));
        assert!(a.turn(a.main, "/compact").unwrap_err().contains("no conversation"), "nor when it comes off the queue");
        let mut b = app(&["codex"]);
        assert!(b.run(Cmd::Compact, "/compact").unwrap_err().contains("compacts its conversation on its own"));
    }

    /// A fake Claude's answer to Ringo's plan read (`get_usage`): the 5-hour limit at the percent in `$0.pct`, else 20.
    #[cfg(unix)]
    const CLAUDE_PLAN: &str = "#!/bin/sh\ncase \"$*\" in *no-session-persistence*) read ask; pct=$(cat \"$0.pct\" 2>/dev/null || echo 20); \
        echo '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"ringo-usage\",\"response\":\
        {\"subscription_type\":\"max\",\"rate_limits\":{\"limits\":[{\"kind\":\"session\",\"percent\":'$pct',\
        \"resets_at\":\"2099-01-01T00:00:00Z\"}]}}}}'; exit 0;; esac\n";

    #[test]
    fn the_review_goes_to_a_plan_with_room() {
        use ringo_cli::Adapter::Codex;
        use ringo_cli::{Plan, Window};
        let plan = |percent: f64, resets: Option<u64>| Some(Ok(Plan { name: "plus".into(), windows: vec![
            Window { name: "week".into(), percent: 40.0, resets: Some(2_000_000) },
            Window { name: "5 hours".into(), percent, resets }], notes: vec![] }));
        let now = 1_000_000;
        let (room, out) = (plan(55.0, Some(now + 60)), plan(100.0, Some(now + 2 * 3600 + 600)));
        let pick = |backend: &str, codex: &Option<Result<Plan, String>>, claude: &Option<Result<Plan, String>>| {
            reviewer(backend, |a| if a == Codex { codex.clone() } else { claude.clone() }, now)
        };
        assert_eq!(pick("codex-chatgpt", &room, &None), Ok(("codex-chatgpt".into(), String::new())));
        assert_eq!(pick("codex-chatgpt", &out, &room),
                   Ok(("claude".into(), "Codex is out (5 hours 100% used, starts over in 2 h 10 min)".into())));
        assert_eq!(pick("claude", &room, &out).unwrap().0, "codex-chatgpt", "either way round");
        assert_eq!(pick("codex-chatgpt", &None, &room), Ok(("claude".into(), "Codex is not set up".into())));
        assert_eq!(pick("codex-chatgpt", &out, &None),
                   Err("Codex is out (5 hours 100% used, starts over in 2 h 10 min) and Claude is not set up".into()));
        assert!(pick("codex-chatgpt", &out, &out).unwrap_err().contains("and Claude is out (5 hours 100% used"));
        // a plan that could not be read counts as having room, and a used-up limit whose time has passed is over
        assert_eq!(pick("codex-chatgpt", &Some(Err("no answer".into())), &out).unwrap().0, "codex-chatgpt");
        assert_eq!(pick("codex-chatgpt", &plan(100.0, Some(now - 5)), &out).unwrap().0, "codex-chatgpt");
        // the owner's own backend command is used as it is: no plan is read
        assert_eq!(reviewer("my-review-cmd", |_| unreachable!(), now), Ok(("my-review-cmd".into(), String::new())));
    }

    #[cfg(unix)]
    #[test]
    fn ultra_reviews_outside_git_on_a_plan_with_room_and_says_at_once_when_none_has_any() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ringo-ultra-nogit-{}", std::process::id()));
        let (class, state) = (dir.join("CSC138"), dir.join("state"));
        std::fs::create_dir_all(&class).unwrap();
        std::fs::write(class.join("a.txt"), "one\n").unwrap();
        let script = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // a Claude whose turns wait while `claude.hold` is there, then add a line; a Codex whose 5 hours are used up;
        // a review that finds nothing
        let claude = script("claude", &format!("{CLAUDE_PLAN}while [ -f \"$0.hold\" ]; do sleep 0.05; done\n\
            head -n 1 > /dev/null\necho more >> a.txt\n\
            echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-n\",\"apiKeySource\":\"none\"}}'\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
        let codex = script("codex", "#!/bin/sh\nread a; read b; read c\necho '{\"id\":1,\"result\":{}}'\n\
            echo '{\"id\":2,\"result\":{\"rateLimits\":{\"planType\":\"plus\",\"primary\":{\"usedPercent\":100,\
            \"windowDurationMins\":300,\"resetsAt\":4102444800}}}}'\n");
        let review = script("ringo", "#!/bin/sh\necho \"$@\" >> \"$0.runs\"\n\
            while [ $# -gt 0 ]; do [ \"$1\" = --out ] && out=$2; shift; done\necho 'No defects found.' > \"$out\"\n");
        let s = Session::with_state(&class, &state).unwrap();
        assert!(!s.git && s.undoable());
        let (c, x) = (Agent::from_name("claude").unwrap(), Agent::from_name("codex").unwrap());
        let mut a = App::new(s, vec![c, x], "CSC138".into(), Theme::named("mono", crate::theme::Depth::Mono).unwrap(), Vec::new());
        a.s.programs.insert(c, claude.clone());
        a.s.programs.insert(x, codex);
        a.s.pipeline = Some(vec![review.clone().into()]);
        a.run(Cmd::Tier(Some(Tier::Ultra)), "/tier ultra").unwrap();
        let turn = |a: &mut App, task: &str| {
            std::fs::write(claude.with_extension("hold"), "").unwrap();
            assert_eq!(a.talk(c, task.into(), task.into()), Ok(None));
            let deadline = Instant::now() + Duration::from_secs(20);
            while a.reviewer.is_none() { // the plans are read while the turn runs
                assert!(Instant::now() < deadline, "the plan read did not answer");
                a.quota_done();
                std::thread::sleep(Duration::from_millis(20));
            }
            let (id, answer) = a.reviewer.clone().unwrap();
            std::fs::remove_file(claude.with_extension("hold")).unwrap();
            while a.ended.get(id - 1) != Some(&true) || a.s.running() > 0 || a.checking.is_some() {
                assert!(Instant::now() < deadline, "the turn did not finish: {} jobs", a.s.jobs.len());
                a.s.pump();
                a.quota_done();
                a.feed();
                a.dispatch();
                std::thread::sleep(Duration::from_millis(20));
            }
            answer
        };
        // Codex is out: the review goes to Claude, and reads the change from Ringo's own store
        let (backend, why) = turn(&mut a, "add a line").unwrap();
        assert!(backend == "claude" && why.starts_with("Codex is out (5 hours 100% used, starts over in "), "{why}");
        assert_eq!(a.s.jobs.len(), 2);
        let runs = std::fs::read_to_string(review.with_extension("runs")).unwrap();
        let arg = |name: &str| runs.split_whitespace().skip_while(|w| *w != name).nth(1).unwrap().to_string();
        assert!(arg("--backend") == "claude" && Path::new(&arg("--repo")).starts_with(state.canonicalize().unwrap()), "{runs}");
        let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
        assert!(said.contains("Ringo reviews #1's change as #2 on claude, since Codex is out") && said.contains("found no defects"), "{said}");
        assert!(!said.contains("needs a git project"), "{said}");
        // both are out: the owner hears it while the turn runs, and no review follows
        std::fs::write(claude.with_extension("pct"), "100").unwrap();
        let why = turn(&mut a, "add another").unwrap_err();
        assert!(why.starts_with("Codex is out") && why.contains(" and Claude is out (5 hours 100% used"), "{why}");
        assert_eq!(a.s.jobs.len(), 3, "no review job");
        let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
        assert!(said.contains("so #3's change will not be reviewed (Esc stops the turn)") && said.contains("no review of #3"), "{said}");
        assert_eq!(std::fs::read_to_string(review.with_extension("runs")).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_switch_takes_the_rest_of_its_line_and_agents_work_side_by_side() {
        use std::os::unix::fs::PermissionsExt;
        let mut a = app(&["claude", "agy"]);
        let (claude, agy) = (Agent::from_name("claude").unwrap(), Agent::from_name("agy").unwrap());
        let dir = std::env::temp_dir().join(format!("ringo-side-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("agy");
        std::fs::write(&fake, "#!/bin/sh\ncat > \"$0.stdin\"\n[ -e \"$0.hold\" ] && sleep 2\n\
            echo '{\"event\":\"init\",\"conversation_id\":\"c-s\",\"init\":{}}'\n\
            echo '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"ok\",\"usage\":{}}}'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        a.s.programs.insert(agy, fake.clone());
        let finish = |a: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while a.s.running() > 0 {
                assert!(Instant::now() < deadline, "the turn did not finish");
                a.s.pump();
                a.feed();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let send = |a: &mut App, line: &str| {
            a.input = line.chars().collect();
            a.submit();
        };
        // "/separate @agy …": the switch, then the rest as its own line (it was dropped)
        send(&mut a, "/separate @agy hello there");
        assert!(a.s.separate);
        finish(&mut a);
        assert_eq!(std::fs::read_to_string(fake.with_extension("stdin")).unwrap(),
                   "{\"event\":\"user\",\"message\":{\"content\":\"hello there\"}}\n");
        // together again: a turn of agy's holds agy's next line, and no other agent's
        send(&mut a, "/together");
        assert!(!a.s.separate);
        std::fs::write(fake.with_extension("hold"), "").unwrap();
        send(&mut a, "@agy slow one");
        assert!(a.s.busy_for(agy).is_some());
        assert!(a.must_wait(agy) && !a.must_wait(claude), "only agy waits");
        finish(&mut a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_plain_line_goes_to_the_agent_that_spoke_last_and_the_prompt_says_which() {
        use std::os::unix::fs::PermissionsExt;
        let mut a = app(&["claude", "agy"]);
        let (claude, agy) = (Agent::from_name("claude").unwrap(), Agent::from_name("agy").unwrap());
        let dir = std::env::temp_dir().join(format!("ringo-follow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("agy");
        std::fs::write(&fake, "#!/bin/sh\ncat > /dev/null\n\
            echo '{\"event\":\"init\",\"conversation_id\":\"c-f\",\"init\":{}}'\n\
            echo '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"do you have the key?\",\"usage\":{}}}'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        a.s.programs.insert(agy, fake);
        let finish = |a: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while a.s.running() > 0 {
                assert!(Instant::now() < deadline, "the turn did not finish");
                a.s.pump();
                a.feed();
                std::thread::sleep(Duration::from_millis(20));
            }
            a.feed();
        };
        let target = |a: &mut App, line: &str| {
            a.input = line.chars().collect();
            a.target().map(|(name, _)| name)
        };
        // the prompt names who the line goes to: the agent plain lines go to, the one an @ names, no one for a command
        assert_eq!(a.main, claude);
        assert_eq!(target(&mut a, "fix it"), Some("claude"));
        assert_eq!((target(&mut a, "@agy fix it"), target(&mut a, "@all x")), (Some("agy"), Some("all")));
        assert_eq!((target(&mut a, "@ag"), target(&mut a, "/help"), target(&mut a, "@codex x")), (None, None, None));
        a.input.clear();
        assert!(screen(&mut a, 80, 24)[23].starts_with(" claude ❯ "));
        // agy's words came last (a turn the owner started earlier, working beside claude's): the next line is for agy
        a.s.launch_agent(agy, false, "find the key").unwrap();
        finish(&mut a);
        assert_eq!(a.main, agy, "it spoke last");
        // a line being typed keeps the agent its prompt showed
        a.main = claude;
        a.input = "half typed".chars().collect();
        a.s.launch_agent(agy, false, "again").unwrap();
        finish(&mut a);
        assert_eq!(a.main, claude);
        // an agent named alone takes the plain lines
        a.input = "@agy".chars().collect();
        a.submit();
        assert_eq!(a.main, agy);
        let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
        assert!(said.contains("plain lines go to agy now"), "{said}");
        // the empty input says what a line does now, by talk's rules: claude's turn at work takes it, unless a line
        // already waits for claude
        let bin = dir.join("claude");
        std::fs::write(&bin, format!("{CLAUDE_PLAN}while [ -f \"$0.hold\" ]; do sleep 0.05; done\nhead -n 1 > /dev/null\n\
            echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-f\",\"apiKeySource\":\"none\"}}'\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("claude.hold"), "").unwrap();
        a.s.programs.insert(claude, bin);
        a.main = claude;
        let id = a.s.launch_agent(claude, false, "work").unwrap();
        let shown = |a: &mut App, text: &str| screen(a, 80, 24).iter().any(|r| r.contains(text));
        assert!(shown(&mut a, " claude ❯ type more; it goes into this turn as it works"));
        a.queue.push_back((claude, "next".into(), "next".into()));
        assert!(shown(&mut a, " claude ❯ type what comes next; it waits for this turn"));
        // stopped, the turn's waiting line comes back for claude, though agy is the one plain lines went to since
        a.main = agy;
        a.stop(id);
        assert_eq!((a.main, a.input.iter().collect::<String>()), (claude, "next".to_string()));
        // a plain line being typed for that agent stays, and the waiting line comes back above it (it was dropped)
        a.input = "half typed".chars().collect();
        a.cursor = 4;
        a.queue.push_back((claude, "later".into(), "later".into()));
        a.stop(id);
        assert_eq!((a.main, a.input.iter().collect::<String>(), a.cursor), (claude, "later\nhalf typed".to_string(), 10));
        // typed for another agent, by the prompt or by name, the line is left as it is: joined to it, claude's line
        // would go to that agent. The waiting line is shown, not sent and not lost from sight.
        for (main, typed) in [(agy, "half typed"), (claude, "@agy look")] {
            a.main = main;
            a.input = typed.chars().collect();
            a.queue.push_back((claude, format!("for claude, not {typed}"), format!("for claude, not {typed}")));
            a.stop(id);
            assert_eq!((a.main, a.input.iter().collect::<String>()), (main, typed.to_string()));
            let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
            assert!(said.contains(&format!("a line that waited for claude was not sent: for claude, not {typed}")), "{said}");
        }
        assert!(a.queue.is_empty());
        let _ = std::fs::remove_file(dir.join("claude.hold"));
        finish(&mut a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ultra_reviews_a_change_and_gives_one_turn_to_fix_it_and_no_more() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ringo-ultra-{}", std::process::id()));
        let (repo, state) = (dir.join("repo"), dir.join("state"));
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        let script = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // a Claude that appends a line to a.txt each turn, keeping each prompt; a review pipeline with one finding
        let claude = script("claude", &format!("{CLAUDE_PLAN}head -n 1 >> \"$0.prompts\"\necho more >> a.txt\n\
            echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-u\",\"apiKeySource\":\"none\"}}'\n\
            echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\"}}'\n"));
        let review = script("ringo", "#!/bin/sh\necho \"$@\" >> \"$0.runs\"\nwhile [ $# -gt 0 ]; do [ \"$1\" = --out ] && out=$2; shift; done\n\
            printf 'What this change does: adds a line.\\n\\n### 1. The new line is wrong (`a.txt:2`)\\n- Problem: it says more.\\n' > \"$out\"\n");
        let s = Session::with_state(&repo, &state).unwrap();
        let mut a = App::new(s, vec![Agent::from_name("claude").unwrap()], "repo".into(),
                             Theme::named("mono", crate::theme::Depth::Mono).unwrap(), Vec::new());
        a.s.programs.insert(a.main, claude.clone());
        a.s.pipeline = Some(vec![review.clone().into()]);
        assert!(a.run(Cmd::Tier(Some(Tier::Ultra)), "/tier ultra").unwrap().unwrap().contains("one turn to fix"));
        assert_eq!(a.talk(a.main, "add a line".into(), "add a line".into()), Ok(None));
        let deadline = Instant::now() + Duration::from_secs(20);
        while a.s.jobs.len() < 3 || a.s.running() > 0 || !a.queue.is_empty() {
            assert!(Instant::now() < deadline, "the chain did not finish: {} jobs", a.s.jobs.len());
            a.s.pump();
            a.feed();
            if a.checking.is_some() {
                assert!(a.must_wait(a.main), "a line sent during the review waits for it");
            }
            a.dispatch();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300)); // anything more would have started by now
        a.s.pump();
        a.feed();
        a.dispatch();
        // the turn, its review, the one revision: nothing after it, though the revision changed the file too
        assert_eq!(a.s.jobs.len(), 3, "{:?}", a.s.jobs.iter().map(|j| j.title.clone()).collect::<Vec<_>>());
        assert!(matches!(a.s.jobs[1].kind, Kind::Ringo { .. }) && !a.s.jobs[2].changed.is_empty());
        let prompts = std::fs::read_to_string(claude.with_extension("prompts")).unwrap();
        assert!(prompts.lines().nth(1).unwrap().contains("The new line is wrong"), "the revision gets the review: {prompts}");
        assert_eq!(std::fs::read_to_string(review.with_extension("runs")).unwrap().lines().count(), 1, "one review");
        assert!(a.checking.is_none() && a.revision.is_none());
        let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
        assert!(said.contains("Ringo reviews #1") && said.contains("one turn to fix what review #2 found"), "{said}");
        // a turn keeps the tier it started in: ultra chosen while a standard turn runs brings no review of it
        a.run(Cmd::Tier(Some(Tier::Standard)), "/tier standard").unwrap();
        a.talk(a.main, "once more".into(), "once more".into()).unwrap();
        a.s.tier = Tier::Ultra;
        while a.s.running() > 0 {
            a.s.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        a.feed();
        a.dispatch();
        assert_eq!((a.s.jobs.len(), a.checking), (4, None), "no review of a turn started in standard");
        assert!(!said.contains("Ringo's review #2 of your change"), "the revision is not shown as a line typed: {said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiers_and_usage_parse_and_show() {
        let all = [Agent::from_name("claude").unwrap()];
        assert_eq!(parse("/tier", &all), Ok(Cmd::Tier(None)));
        assert_eq!(parse("/tier deep", &all), Ok(Cmd::Tier(Some(Tier::Deep))));
        assert!(parse("/tier max", &all).is_err());
        assert_eq!(parse("/usage", &all), Ok(Cmd::Usage));
        let mut a = app(&["grok"]); // no agent with a plan to ask: /usage answers at once
        a.s.tier = Tier::Deep;
        let rows = screen(&mut a, 80, 10);
        assert!(rows[0].ends_with("ask mode · deep · 0 tokens"), "the head names a tier other than standard: {rows:#?}");
        a.s.tier = Tier::Ultra;
        let narrow = screen(&mut a, 32, 10);
        assert!(narrow[0].ends_with("ultra"), "a costly tier is the last thing the head drops: {narrow:#?}");
        // Esc stops ultra's review as it would a turn, and gives the waiting lines back
        a.checking = Some((7, 6, a.main));
        a.queue.push_back((a.main, "next".into(), "next".into()));
        assert_eq!(a.holding(), Some(7));
        a.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(a.input.iter().collect::<String>(), "next");
        a.checking = None;
        a.input.clear();
        a.cursor = 0;
        assert_eq!(a.run(Cmd::Usage, "/usage"), Ok(None));
        let said: String = a.out.iter().map(|o| format!("{o:?}")).collect();
        assert!(said.contains("this session 0") && said.contains("last 24 hours"), "{said}");
        // a plan is its agent's name and plan, then a gauge a limit: 34% of 24 cells is 8, in the calm colour
        let t = Theme::named("siamese", crate::theme::Depth::TrueColor).unwrap();
        let plan = ringo_cli::Plan { name: "plus".into(), windows: vec![ringo_cli::Window {
            name: "week".into(), percent: 34.0, resets: None }], notes: vec!["2 full resets left".into()] };
        let rows: Vec<Line> = plan_rows("codex", t.codex, &plan, &t).iter().flat_map(|o| o.rows(80, &t)).collect();
        let text: Vec<String> = rows.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert_eq!(text[0], "  codex · plus plan · 2 full resets left");
        assert_eq!(text[1].trim_end(), format!("    week          {}{}  34%", "▰".repeat(8), "▱".repeat(16)));
        assert_eq!(rows[1].spans[1].style, t.ok);
        let full = Out::Meter { label: "5 hours".into(), pad: 7, percent: 96.0, value: "96%".into(), note: String::new(), fill: None };
        assert_eq!(full.rows(80, &t)[0].spans[1].style, t.fail, "nearly used up: the failure colour");
        let some = Out::Meter { label: "x".into(), pad: 1, percent: 0.4, value: "0%".into(), note: String::new(), fill: None };
        assert_eq!(some.rows(80, &t)[0].spans[1].content, "▰", "a little used is not nothing");
        for w in [20, 40, 120] {
            assert!(full.rows(w, &t).iter().all(|l| l.width() <= w), "a gauge fits its width ({w})");
        }
        assert_eq!((fmt_until(90), fmt_until(3 * 3600 + 120), fmt_until(2 * 86_400 + 7200)), ("2 min".into(), "3 h 2 min".into(), "2 d 2 h".into()));
    }

    #[cfg(unix)]
    #[test]
    fn each_agent_takes_its_own_turn_at_once_together_or_separate() {
        use std::os::unix::fs::PermissionsExt;
        let mut a = app(&["claude", "codex"]);
        let (claude, codex) = (Agent::from_name("claude").unwrap(), Agent::from_name("codex").unwrap());
        assert_eq!((parse("/separate", &[claude]), parse("/together", &[claude])), (Ok(Cmd::Separate(true)), Ok(Cmd::Separate(false))));
        // what follows a command that takes nothing is refused, never dropped (the input splits off a switch's rest)
        for line in ["/separate @agy go", "/undo now", "/clear all of it", "/compact focus on x"] {
            assert!(parse(line, &[claude]).unwrap_err().contains("takes nothing after it"), "{line}");
        }
        let fake = std::env::temp_dir().join(format!("ringo-tui-fake-both-{}", std::process::id()));
        std::fs::write(&fake, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        a.s.programs.insert(claude, fake.clone());
        a.s.programs.insert(codex, fake);
        assert!(a.talk(claude, "fix it".into(), "fix it".into()).is_ok());
        assert_eq!(a.talk(codex, "docs".into(), "@codex docs".into()), Ok(None));
        assert_eq!(a.s.running(), 2, "together too, codex starts beside claude");
        assert!(a.run(Cmd::Separate(true), "/separate").unwrap().unwrap().contains("separate:"));
        // a line for claude goes into claude's running turn; codex's has not started one it can take a line in yet
        assert_eq!(a.talk(claude, "more".into(), "more".into()), Ok(Some("  ↳ #1 takes this as it works".into())));
        assert_eq!(a.s.jobs[0].steered, ["more"]);
        assert_eq!(a.talk(codex, "and".into(), "@codex and".into()), Ok(None));
        assert!(!a.dispatch(), "both are busy");
        assert!(screen(&mut a, 80, 12)[0].ends_with("ask mode · separate · 0 tokens"), "the head says so");
        // Esc stops the latest turn (codex's); only codex's waiting line comes back to the input
        a.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(a.input.iter().collect::<String>(), "@codex and");
        assert!(a.queue.is_empty());
        assert!(a.run(Cmd::Separate(true), "/separate").unwrap().unwrap().contains("already"));
        a.s.shutdown();
    }

    #[test]
    fn a_line_waits_while_it_cannot_start() {
        let mut a = app(&["claude"]);
        a.s.max_jobs = 0; // every job slot taken
        assert_eq!(a.talk(a.main, "fix it".into(), "fix it".into()), Ok(None));
        assert_eq!(a.queue.len(), 1, "it waits instead of being refused");
        assert!(a.waits(&Ok(Cmd::Talk("and this".into()))), "and the next line shows when it starts, not now");
        let rows = screen(&mut a, 60, 12);
        assert!(rows.iter().any(|r| r.contains("↳ next for claude: fix it")), "{rows:#?}");
        let wide = screen(&mut a, 120, 30);
        assert!(wide.iter().any(|r| r.contains("↳ claude: fix it")), "the side panel names it too: {wide:#?}");
        // /auto turns auto on and never off: typed with auto on (remembered from last time) it says so and stays
        a.s.mode = Mode::Auto;
        assert!(a.run(Cmd::Auto, "/auto").unwrap().unwrap().contains("already on"));
        assert_eq!(a.s.mode, Mode::Auto);
    }

    #[test]
    fn the_conversation_scrolls_and_its_text_selects() {
        let mut a = app(&["claude"]);
        for i in 0..30 {
            a.say(format!("line {i}"));
        }
        // 10 rows: the head and its rule, six of conversation (rows 2 to 7), a rule, the input
        let rows = screen(&mut a, 30, 10);
        assert_eq!(rows[7], "line 29", "the end shows, above the rule and the input: {rows:#?}");
        a.key(KeyEvent::from(KeyCode::PageUp));
        let up = screen(&mut a, 30, 10);
        assert_eq!(up[7], "line 25", "PgUp goes back a screen less two rows: {up:#?}");
        assert!(up[9].contains("scrolled up"), "{up:#?}");
        a.say("line 30");
        assert_eq!(screen(&mut a, 30, 10)[7], "line 25", "new lines below keep what shows still");
        a.key(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(screen(&mut a, 30, 10)[7], "line 30", "typing goes back to the end");
        // a drag from "ne 29" through "line 3" (screen rows 6 and 7) selects just that text
        let (from, to) = (a.place(a.top + 4), a.place(a.top + 5));
        a.sel = Some(((from.0, from.1, 2), (to.0, to.1, 5)));
        assert_eq!(a.selected(), "ne 29\nline 3");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(30, 10)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let cell = |x: u16, y: u16| terminal.backend().buffer()[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(cell(2, 6) && !cell(1, 6) && cell(5, 7) && !cell(6, 7), "the selection shows reversed");
    }

    #[test]
    fn a_wide_character_takes_its_two_columns() {
        let mut a = app(&["claude"]);
        a.say("\u{2705} ok");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(8, 5)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let row: Vec<&str> = terminal.backend().buffer().content[16..21].iter().map(|c| c.symbol()).collect(); // row 2
        assert_eq!(row, ["\u{2705}", " ", " ", "o", "k"]);
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn a_long_line_wraps_at_words_under_its_own_text() {
        let dim = Style::new().add_modifier(ratatui::style::Modifier::DIM);
        let rows = fit(&Line::from(vec![Span::raw("  • "), Span::styled("alpha beta gamma", dim)]), 12, 4);
        assert_eq!(plain(&rows), ["  • alpha", "    beta", "    gamma"]);
        assert!(rows[1].spans.iter().any(|s| s.content == "beta" && s.style == dim), "styles survive the wrap");
        assert_eq!(plain(&fit(&Line::raw("abcdefghij"), 4, 0)), ["abcd", "efgh", "ij"], "a word longer than a row is cut");
        assert_eq!(plain(&fit(&Line::raw("\u{26A0}\u{FE0F} x"), 10, 0)), ["\u{26A0} x"], "no emoji presentation selector");
    }

    #[test]
    fn the_manual_covers_every_command_and_fits_the_terminal() {
        for (command, _) in COMMANDS {
            let head = command.split(' ').next().unwrap();
            assert!(MANUAL.contains(&format!("`{head}")), "the manual does not explain {head}");
        }
        let theme = Theme::named("mono", crate::theme::Depth::Mono).unwrap();
        for width in [50, 80, 120] {
            let lines = markdown(MANUAL, width, &theme);
            assert!(lines.len() > 50);
            for line in plain(&lines) {
                assert!(cols(&line) <= width, "{width} columns: {line:?}");
            }
        }
    }

    #[test]
    fn a_reply_reads_as_markdown() {
        let theme = Theme::named("mono", crate::theme::Depth::Mono).unwrap();
        let md = |text: &str, width: usize| plain(&markdown(text, width, &theme));
        let text = "# Title\n\nSome **bold** and `code`.\n\n- one item that wraps around\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        assert_eq!(md(text, 20), ["Title", "", "Some bold and code.", "", "• one item that", "  wraps around", "• two", "",
                                  "a  b", "────", "1  2"]);
        // a table too wide for the terminal: a block per row, each cell under its column's name
        let wide = "| name | status |\n|---|---|\n| LangGraph | yes it is there |\n";
        assert_eq!(md(wide, 20), ["• LangGraph", "  status: yes it is", "  there"]);
        assert_eq!(md(wide, 40), ["name       status", "──────────────────────────", "LangGraph  yes it is there"]);
        assert_eq!(md(wide, 24), ["name       status", &"─".repeat(24), "LangGraph  yes it is", "           there"],
                   "when the first column takes half the row or less, the last one wraps under itself");
        assert_eq!(md("```\nlet x = 1;\n```", 20), ["  let x = 1;"]);
        assert_eq!(md("```\nlet x = 1 + 2;\n```", 10), ["  let x =", "  1 + 2;"], "wrapped code stays in its block");
        assert_eq!(md("> aaa bbb ccc", 8), ["│ aaa", "│ bbb", "│ ccc"], "a quote's wrapped rows keep its bar");
        assert_eq!(md("1. a\n2. b\n   - c", 20), ["1. a", "2. b", "   • c"]);
        assert_eq!(md("see [the docs](https://x.dev) and [a.rs](src/a.rs)", 60), ["see the docs (https://x.dev) and a.rs"]);
    }

    #[test]
    fn the_line_editing_keys_find_words_and_lines() {
        let text: Vec<char> = "fix  the bug\nin parse.rs".chars().collect();
        assert_eq!(word_start(&text, 12), 9, "back over the word the cursor ends");
        assert_eq!(word_start(&text, 5), 0, "spaces before the cursor go with the word before them");
        assert_eq!(word_start(&text, 0), 0);
        assert_eq!(word_end(&text, 3), 8, "on over spaces, then the word");
        assert_eq!(word_end(&text, 20), text.len());
        assert_eq!(line_bounds(&text, 4), (0, 12));
        assert_eq!(line_bounds(&text, 15), (13, text.len()));
        assert_eq!(line_bounds(&text, 12), (0, 12), "at the line's end, before its newline");
    }

    #[test]
    fn a_step_stays_on_one_row_and_keeps_what_came_of_it() {
        assert_eq!(one_row("Read a.rs · 2 lines", 40), "Read a.rs · 2 lines");
        assert_eq!(one_row("Grep langgraph|deepagents|langchain in exovra-platform · 2 lines", 30),
                   "Grep langgraph|deep… · 2 lines", "exactly 30 columns");
        assert_eq!(one_row("$ cargo test --workspace --all-features", 20), "$ cargo test --work…");
        let long_result = format!("$ make · {}", "x".repeat(30));
        assert_eq!(cols(&one_row(&long_result, 20)), 20, "a result too long to keep whole is cut with the rest");
    }

    #[test]
    fn the_menu_offers_commands_and_agents() {
        let agents = [("claude", true), ("grok", false)];
        let to = |before: &str| menu(before, &agents).into_iter().map(|(to, ..)| to).collect::<Vec<_>>();
        assert_eq!(to("/re"), ["/review ", "/review ", "/research ", "/restart"]);
        assert_eq!(to("/he"), ["/help"], "a command that takes nothing more completes without a space");
        assert_eq!(to("@"), ["@claude ", "@claude! ", "@grok! ", "@all "], "grok works only in its own copy");
        assert_eq!(to("fix it @cl"), ["@claude ", "@claude! "]);
        assert!(to("a /re").is_empty(), "a command only as the line's first word");
        assert!(to("ping @someone").is_empty() && to("plain words").is_empty());
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_tokens(870), "870");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_age(Duration::from_secs(75)), "1:15");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!([base64(b"hi"), base64(b"hey"), base64(b"heya")], ["aGk=", "aGV5", "aGV5YQ=="]);
        assert_eq!(slice_cols("a漢字b", 1, 3), "漢", "a wide character counts where it starts");
    }

    /// Pieces of 4 to 24 characters, as an agent's text deltas come (the same pieces every run).
    fn pieces(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let (mut out, mut i, mut seed) = (Vec::new(), 0, 12345u32);
        while i < chars.len() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = 4 + (seed >> 16) as usize % 21;
            out.push(chars[i..(i + n).min(chars.len())].iter().collect());
            i += n;
        }
        out
    }

    /// Not a check: prints what the path a reply is written through costs and how still its rows stay.
    /// cargo test --release measure_ -- --ignored --nocapture --test-threads 1
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn measure_a_reply_being_written() {
        use std::os::unix::fs::PermissionsExt;
        const SHELL_DOC: &str = include_str!("../../docs/SHELL.md");
        let fake = std::env::temp_dir().join(format!("ringo-tui-fake-measure-{}", std::process::id()));
        std::fs::write(&fake, "#!/bin/sh\nsleep 120\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (claude, codex) = (Agent::from_name("claude").unwrap(), Agent::from_name("codex").unwrap());
        let start = |fake: &Path| {
            let mut a = app(&["claude", "codex"]);
            a.s.programs.insert(claude, fake.to_path_buf());
            a.s.programs.insert(codex, fake.to_path_buf());
            a.talk(claude, "go".into(), "go".into()).unwrap();
            a
        };
        let stats = |name: &str, mut us: Vec<u128>, extra: String| {
            let n = us.len().max(1);
            let late = us[n - n / 10 - 1..].iter().sum::<u128>() / (n / 10 + 1) as u128;
            let total: u128 = us.iter().sum();
            us.sort();
            println!("{name:<44} frames {n:>5} · mean {:>6} µs · p95 {:>6} µs · max {:>6} µs · last tenth {late:>6} µs · \
                      total {:>5} ms{extra}", total / n as u128, us[n * 95 / 100], us[n - 1], total / 1000);
        };
        let short: String = MANUAL.chars().take(3000).collect();
        for (name, text, w, h) in [("a 3,000-character reply at 80x24", short.as_str(), 80, 24),
                                   ("a 3,000-character reply at 120x40", short.as_str(), 120, 40),
                                   ("the 14.8k guide as a reply at 120x40", MANUAL, 120, 40),
                                   ("the 56k shell page as a reply at 120x40", SHELL_DOC, 120, 40)] {
            let mut a = start(&fake);
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            let (mut us, mut prev, mut above, mut back): (Vec<u128>, Vec<String>, usize, usize) = (Vec::new(), Vec::new(), 0, 0);
            for piece in pieces(text) {
                a.s.jobs[0].fresh = vec![Said::Partial(piece)];
                let at = Instant::now();
                a.feed();
                terminal.draw(|f| a.draw(f)).unwrap();
                us.push(at.elapsed().as_micros());
                let cur = plain(&a.fitted[a.streams[&1].at]);
                if let Some(last) = prev.len().checked_sub(1) {
                    let show = std::env::var_os("RINGO_MEASURE_SHOW").is_some();
                    if let Some(i) = (0..last).find(|&i| cur.get(i) != Some(&prev[i])) {
                        above += 1; // a row above the one being written changed
                        if show {
                            println!("  above, {} rows up: {:?}\n      became {:?}", last - i, prev[i], cur.get(i));
                        }
                    } else if !prev[last].contains("a table is being written")
                        && !cur.get(last).is_some_and(|row| row.starts_with(prev[last].as_str())) {
                        back += 1; // the row being written took text back
                        if show {
                            println!("  back: {:?}\n      became {:?}", prev[last], cur.get(last));
                        }
                    }
                }
                prev = cur;
            }
            stats(name, us, format!(" · rows above rewritten in {above} frames, the last row took text back in {back}"));
            a.s.shutdown();
        }
        // two agents side by side: claude's reply opened first and still grows, with codex's steps and reply under it
        let mut a = start(&fake);
        a.talk(codex, "docs".into(), "@codex docs".into()).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        a.s.jobs[0].fresh = vec![Said::Partial("Starting. ".into())];
        a.feed();
        a.s.jobs[1].fresh = (0..300).map(|i| Said::Step(format!("Edit src/file{i}.rs · 2 lines\n- old\n+ new"))).collect();
        a.s.jobs[1].fresh.push(Said::Partial(MANUAL.chars().take(6000).collect()));
        a.feed();
        let mut us = Vec::new();
        for piece in pieces(&short) {
            a.s.jobs[0].fresh = vec![Said::Partial(piece)];
            let at = Instant::now();
            a.feed();
            terminal.draw(|f| a.draw(f)).unwrap();
            us.push(at.elapsed().as_micros());
        }
        stats("the first of two agents writing, 120x40", us, String::new());
        a.s.shutdown();
        // a long conversation: what a turn's end costs (the text kept for --resume) and a frame with nothing new
        let mut a = start(&fake);
        for i in 0..300 {
            a.echo(&format!("request {i}"));
            a.out.extend((0..10).map(|k| Out::Step(format!("Read src/file{k}.rs · 40 lines"))));
            a.reply(&short);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let at = Instant::now();
        a.save();
        println!("{:<44} {} µs for {} entries", "saving a 300-turn conversation", at.elapsed().as_micros(), a.out.len());
        let us = (0..200).map(|_| {
            let at = Instant::now();
            terminal.draw(|f| a.draw(f)).unwrap();
            at.elapsed().as_micros()
        }).collect();
        stats("a frame of it with nothing new", us, String::new());
        a.s.shutdown();
        let _ = std::fs::remove_file(&fake);
    }
}

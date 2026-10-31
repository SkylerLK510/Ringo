//! The `ringo` shell. The conversation, and what every agent does, prints into the terminal's own scrollback, as
//! Claude Code's and Codex's do, so the mouse wheel scrolls it and a drag selects its text alone. Below it, a live area
//! holds the running jobs, any permission request, the input box and a status line. That area redraws only when
//! something changed (an event, a key), and for the spinner (every SPIN_MS) while jobs run.

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event as Term, KeyCode, KeyEvent,
                                KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::layout::{Constraint, Layout};
use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget, Wrap};
use ratatui::{DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport};
use ringo_cli::{Agent, Decision, Kind, Mode, Session, Status};
use std::collections::VecDeque;
use std::io::stdout;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const HELP: &str = "\
Ringo's manual (/usrman, /instructions or /help shows it again).
Private state: {state}

A line without / or @ is a question: one tool-free call, with your last few
questions and answers as context. It costs a few thousand tokens and cannot
see your files (pipelines backend: {backend}). When that call reads the line
as a task (files to change, read or review, or live sources), Ringo starts it:
claude takes code turns (codex when claude is not set up), /research takes
sources.

Mode (now: {mode}), one switch for every agent: Shift-Tab cycles it,
/mode ask|auto|read sets it, /auto toggles auto.
  ask   agents read, and edit their own worktree, freely; anything else (a
        command in a write turn, a connector such as Jira, the network, a push
        or a PR) waits for you: y allow, a allow for this session, n decline.
        Read turns (@claude) never run commands: they work in your own tree
  auto  agents act on their own; only what their safety review flags, or an
        always-ask rule names (sudo, disk tools, force pushes, publishing,
        agent settings), waits for you. Codex works in its sandbox with the
        network and its own reviewer; it cannot stop to ask
  read  agents look but do not touch: no edits, no commands, no connectors

Ringo's pipelines run the models as tool-free workers with executable checks
and one review:
  /review #N           job N's change (its task as the change description)
  /review BASE [HEAD]  BASE...HEAD of this repository
  /design TEXT         the one-call design pipeline on TEXT
  /research TOPIC      one live web search call, every quoted source fetched
                       and checked in code, one write call that cites only
                       what was verified (about 70k tokens, mostly search
                       results)
  /work SPEC.json      the checked work loop on a task spec
  /backend SPEC        backend for those pipelines (now: {backend})

Agents (full agent turns: about 17,000 tokens before they start, far more when
they search your files; each job's line counts them), for questions about
your code and for changes:
  @codex TASK          ask an agent; it can read the repository but not
                       change it (the conversation continues)
  @codex! TASK         let it edit, in a private worktree from HEAD; nothing
                       lands until you /apply
  @codex @claude TASK  the same task to several agents at once; @all means
                       every agent set up (read turns skip the ones with no
                       read-only mode, such as grok)
  @agy TASK            Google's Antigravity CLI, set up with /setup; agents
                       you add yourself take @name! turns
  {#N} in a TASK       hands job N's reply (and change) to the agent, shown
                       before it is sent
  /setup               add another agent and sign in to it
  /update              update the agents the opening check found out of date,
                       in this terminal (no tokens); /update auto on|off: do
                       it by itself when the shell opens
  /route on|off        suggest a command for a line that reads like a
                       research, design, review or edit task or a question
                       about this folder's files, for you to send (Enter),
                       change, or ask as a plain question (Esc); no model call
                       (now: {route})
  /theme [NAME]        colours: siamese, siamese-light, pink, midnight or mono
                       (now: {theme})

Jobs: /show N (its log)  /diff N  /apply N  /drop N (remove its
worktree)  /kill N  /quit

Keys: Enter sends; Ctrl-J, Alt-Enter or \\ then Enter starts a new line (a paste
keeps its lines and is not sent). Up / Down move between lines, then through
earlier messages. Shift-Tab changes the mode, Esc clears, Ctrl-C twice quits.
The conversation is in the terminal's scrollback: scroll it and select from it
as in any other program.";

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Agents(Vec<(Agent, bool)>, String),
    Ask(String),
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
    Route(bool),
    /// `/update` (None) or `/update auto on|off`.
    Update(Option<bool>),
    /// `/mode` (None: show it) or `/mode ask|auto|read`.
    Mode(Option<Mode>),
    /// `/auto`: auto mode on, or back to ask mode.
    Auto,
    Quit,
}

fn job_number(word: Option<&str>) -> Result<usize, String> {
    word.map(|w| w.trim_start_matches('#')).and_then(|w| w.parse().ok()).ok_or("give a job number, e.g. 3".into())
}

/// One input line as a command. A line without `/` or `@` is a question (one tool-free call); @all means every
/// set-up agent that can read.
pub fn parse(line: &str, all: &[Agent]) -> Result<Cmd, String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('/') {
        let mut words = rest.split_whitespace();
        let name = words.next().unwrap_or("");
        let tail = rest[name.len()..].trim();
        return match name {
            "usrman" | "instructions" | "manual" | "help" | "?" => Ok(Cmd::Help),
            "quit" | "exit" | "q" => Ok(Cmd::Quit),
            "setup" => Ok(Cmd::Setup),
            "auto" => Ok(Cmd::Auto),
            "mode" => match words.next() {
                None => Ok(Cmd::Mode(None)),
                Some(m) => Mode::from_name(m).map(|m| Cmd::Mode(Some(m))).ok_or("/mode ask, /mode auto or /mode read".into()),
            },
            "theme" => Ok(Cmd::Theme(words.next().map(str::to_string))),
            "route" => match words.next() {
                Some("on") => Ok(Cmd::Route(true)),
                Some("off") => Ok(Cmd::Route(false)),
                _ => Err("/route on or /route off".into()),
            },
            "update" => match (words.next(), words.next()) {
                (None, _) => Ok(Cmd::Update(None)),
                (Some("auto"), Some("on")) => Ok(Cmd::Update(Some(true))),
                (Some("auto"), Some("off")) => Ok(Cmd::Update(Some(false))),
                _ => Err("/update, or /update auto on|off".into()),
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
            _ => Err(format!("unknown or incomplete command /{name}; /usrman lists them")),
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
            "all" => agents.extend(all.iter().filter(|a| write || a.spec().read).map(|a| (*a, write))),
            _ => agents.push((Agent::from_name(name).ok_or_else(|| {
                let known: Vec<&str> = Agent::all().into_iter().map(|a| a.name()).collect();
                format!("no agent {name}: {} or all (/setup adds one)", known.join(", "))
            })?, write)),
        }
        rest = rest.trim_start()[word.len()..].trim_start();
    }
    if agents.is_empty() {
        return if rest.is_empty() { Err("type a question, or /usrman".into()) } else { Ok(Cmd::Ask(rest.to_string())) };
    }
    if rest.is_empty() {
        return Err("say what the agent should do".into());
    }
    Ok(Cmd::Agents(agents, rest.to_string()))
}

pub fn fmt_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

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

/// The most rows one print into the scrollback takes (a longer single line is cut there; all of it is on disk).
const CHUNK_ROWS: usize = 1000;

/// Prints conversation lines into the scrollback above the live area, word-wrapped to the terminal's width.
fn flush<B: Backend>(terminal: &mut Terminal<B>, lines: Vec<Line<'static>>) -> Result<(), B::Error> {
    terminal.autoresize()?;
    let width = terminal.size()?.width.max(1);
    let (mut chunk, mut rows) = (Vec::new(), 0);
    for line in lines {
        let n = Paragraph::new(line.clone()).wrap(Wrap { trim: false }).line_count(width).clamp(1, CHUNK_ROWS);
        if rows > 0 && rows + n > CHUNK_ROWS {
            insert(terminal, std::mem::take(&mut chunk), rows)?;
            rows = 0;
        }
        chunk.push(line);
        rows += n;
    }
    if rows > 0 {
        insert(terminal, chunk, rows)?;
    }
    Ok(())
}

fn insert<B: Backend>(terminal: &mut Terminal<B>, lines: Vec<Line<'static>>, rows: usize) -> Result<(), B::Error> {
    let text = Paragraph::new(lines).wrap(Wrap { trim: false });
    terminal.insert_before(rows as u16, |buf| text.render(buf.area, buf))
}

/// An inline live area of `height` rows at the cursor: no alternate screen, so the terminal keeps its scrollback.
fn inline(height: u16) -> std::io::Result<DefaultTerminal> {
    Terminal::with_options(CrosstermBackend::new(stdout()), TerminalOptions { viewport: Viewport::Inline(height) })
}

fn open(height: u16) -> std::io::Result<DefaultTerminal> {
    enable_raw_mode()?;
    execute!(stdout(), EnableBracketedPaste)?; // a paste arrives as one event, not as keys (its newlines as Enter)
    inline(height)
}

/// The live area goes and the terminal is as it was (for an agent's sign-in, an update, or the end); the
/// conversation stays in the scrollback.
fn close(terminal: &mut DefaultTerminal) {
    let top = terminal.get_frame().area().y;
    let _ = terminal.clear();
    let _ = terminal.set_cursor_position((0, top));
    let _ = terminal.show_cursor();
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
}

/// A live area of a new height, where the old one began (growing pushes the conversation up into the scrollback).
fn reshape(terminal: &mut DefaultTerminal, height: u16) -> std::io::Result<()> {
    let top = terminal.get_frame().area().y;
    terminal.clear()?;
    terminal.set_cursor_position((0, top))?;
    *terminal = inline(height)?;
    Ok(())
}

/// How many printed lines the shell keeps, to print the end of the conversation again after the window narrows.
const RECENT: usize = 300;

/// Prints the conversation lines that wait for the scrollback.
fn print(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    let lines = std::mem::take(&mut app.out);
    app.recent.extend(lines.iter().cloned());
    let extra = app.recent.len().saturating_sub(RECENT);
    app.recent.drain(..extra);
    flush(terminal, lines)
}

/// Prints what waits for the scrollback, fits the live area to what it holds, and draws it.
fn paint(terminal: &mut DefaultTerminal, app: &mut App, shape: &mut u16) -> std::io::Result<()> {
    let size = terminal.size()?;
    if size.width < app.width {
        // A narrower window: ratatui clears the whole screen (rows the terminal re-wrapped would garble it), and the
        // visible end of the conversation goes with it, so that end is printed again at the new width.
        terminal.autoresize()?;
        let room = size.height.saturating_sub(*shape) as usize;
        let (mut again, mut rows) = (Vec::new(), 0);
        for line in app.recent.iter().rev() {
            rows += Paragraph::new(line.clone()).wrap(Wrap { trim: false }).line_count(size.width).max(1);
            if rows > room {
                break;
            }
            again.push(line.clone());
        }
        again.reverse();
        flush(terminal, again)?;
    }
    app.width = size.width;
    if !app.out.is_empty() {
        print(terminal, app)?;
    }
    let live = app.live(size.width, size.height);
    let want = live.height().clamp(1, size.height.max(1));
    if want != *shape {
        reshape(terminal, want)?;
        *shape = want;
    }
    terminal.draw(|f| app.draw(f, &live))?;
    Ok(())
}

struct App {
    s: Session,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist: usize,
    /// Conversation lines not yet printed into the scrollback, the last RECENT printed, and the width they were
    /// printed at.
    out: Vec<Line<'static>>,
    recent: VecDeque<Line<'static>>,
    width: u16,
    /// Per job (at its id - 1): whether the line it ended with is printed.
    ended: Vec<bool>,
    /// When the last key arrived: an Enter right behind another key is part of a paste (see BURST).
    last_key: Instant,
    /// The job whose line was printed last (0: none, or the shell's own), so a run of one job's lines is named once.
    last: usize,
    quit_armed: bool,
    quit: bool,
    setup: bool,
    label: String,
    agents: Vec<Agent>,
    theme: Theme,
    started: Instant,
    /// Route suggestions on (/route on), and the plain line a suggestion is standing in for.
    route: bool,
    suggested: Option<String>,
    /// The permission request on screen and when it appeared (answer keys wait ANSWER_GRACE after that).
    asked: Option<(String, Instant)>,
    /// Agent updates found when the shell opened (None until that check reports), and `/update` asked for.
    pending: Option<Vec<crate::updates::Update>>,
    update: bool,
}

/// What the live area holds in one frame: the running jobs' rows, the permission request, and the input's rows.
struct Live {
    jobs: Vec<Line<'static>>,
    ask: Option<(String, Vec<Line<'static>>)>,
    ask_h: u16,
    rows: Vec<String>,
    at: (usize, usize),
    /// The first input row shown, and how many are.
    top: usize,
    shown: usize,
}

impl Live {
    fn height(&self) -> u16 {
        self.jobs.len() as u16 + self.ask_h + self.shown as u16 + 2 + 1 // the input's border, the status line
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

impl App {
    fn say(&mut self, text: impl AsRef<str>) {
        for line in clean(text.as_ref()).lines() {
            self.out.push(Line::raw(line.to_string()));
        }
        self.last = 0;
    }

    /// The owner's own line in the conversation, after a blank line.
    fn echo(&mut self, line: &str) {
        self.out.push(Line::default());
        for (i, l) in clean(line).lines().enumerate() {
            self.out.push(Line::from(vec![Span::styled(if i == 0 { "› " } else { "  " }, self.theme.focus),
                                          Span::styled(l.to_string(), self.theme.selected)]));
        }
        self.last = 0;
    }

    fn help_text(&self) -> String {
        HELP.replace("{backend}", &self.s.backend).replace("{theme}", self.theme.name).replace("{mode}", self.s.mode.name())
            .replace("{route}", if self.route { "on" } else { "off" }).replace("{state}", &self.s.dir.display().to_string())
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
            Kind::Agent { agent, write } => (format!("{}{}", agent.name(), if *write { "!" } else { "" }), self.agent_style(*agent)),
            Kind::Ringo { command } => (format!("ringo {command}"), self.theme.ringo),
        }
    }

    /// Queues every job's new log lines for the conversation, each run of one job's lines under its name, and one
    /// line when a job ends.
    fn feed(&mut self) {
        for i in 0..self.s.jobs.len() {
            if self.ended.len() == i {
                self.ended.push(false);
            }
            let fresh = std::mem::take(&mut self.s.jobs[i].fresh);
            let job = &self.s.jobs[i];
            let mut lines = Vec::new();
            if !fresh.is_empty() {
                if self.last != job.id {
                    let (who, style) = self.who(job);
                    lines.push(Line::from(vec![Span::styled(format!("#{} ", job.id), self.theme.dim), Span::styled(who, style)]));
                }
                for line in &fresh {
                    lines.extend(clean(line).lines().map(|l| Line::raw(format!("  {l}"))));
                }
            }
            let ends = !job.running() && !self.ended[i];
            if ends {
                lines.extend(self.job_end(job));
            }
            let id = job.id;
            self.ended[i] |= ends;
            if !fresh.is_empty() {
                self.last = id;
            }
            if ends {
                self.last = 0;
            }
            self.out.extend(lines);
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
        let (who, who_style) = self.who(job);
        let calls = match job.calls { 0 => String::new(), 1 => " · 1 call".into(), n => format!(" · {n} calls") };
        let mut tail = format!(" {what} · {} · {} tokens{calls}", fmt_age(job.age()), fmt_tokens(job.tokens.total()));
        if job.worktree.is_some() && job.status == Status::Done {
            tail += &format!(" · change ready: /diff {0}, /apply {0}", job.id);
        }
        let mut lines = vec![Line::from(vec![Span::styled(format!("{mark} "), style), Span::styled(format!("#{} ", job.id), t.dim),
                                             Span::styled(who, who_style), Span::styled(tail, t.dim)])];
        let overlaps: Vec<String> = self.s.overlaps(job.id).into_iter()
            .map(|(other, files)| format!("#{other} {}", files.join(", "))).collect();
        if !overlaps.is_empty() {
            lines.push(Line::styled(format!("  ⚠ same files as {}", overlaps.join("; ")), t.warn));
        }
        lines
    }

    /// Who takes a suggested agent turn: the set-up agents that can take it now (a read turn needs a read-only mode
    /// and a free conversation), with claude, then codex, preferred (see route::owner).
    fn owner(&self, route: &crate::route::Route) -> Option<Agent> {
        let read = *route == crate::route::Route::Read;
        let able: Vec<&str> = self.agents.iter().filter(|a| !read || (a.spec().read && self.s.busy(**a).is_none()))
            .map(|a| a.name()).collect();
        crate::route::owner(&able).and_then(Agent::from_name)
    }

    /// Sets the mode for the turns started from now on (a running turn keeps its own), and saves it.
    fn set_mode(&mut self, mode: Mode) -> String {
        self.s.mode = mode;
        let saved = crate::theme::set_setting("mode", serde_json::json!(mode.name())).err()
            .map_or(String::new(), |e| format!(" (not saved: {e})"));
        let what = match mode {
            Mode::Ask => "agents ask you before commands, connectors, the network, pushes and PRs",
            Mode::Auto => "agents act on their own; flagged actions and always-ask rules still wait for you",
            Mode::Read => "agents look but do not touch",
        };
        format!("  mode: {}: {what}; turns already running keep theirs{saved}", mode.name())
    }

    /// Starts the task an ask call routed (`ROUTE: <route>`): code turns go to their owner, sources to /research;
    /// a review needs its range, so it is put in the input line to complete.
    fn start_routed(&mut self, ask: usize, route: &str) {
        let line = self.s.job(ask).map(|j| j.title_full.clone()).unwrap_or_default();
        let (kind, write) = match route {
            "edit" => (crate::route::Route::Edit, true),
            "read" => (crate::route::Route::Read, false),
            "research" => {
                match self.s.research(&line) {
                    Ok(id) => self.say(format!("  → #{ask} is a task for live sources: started /research as #{id}")),
                    Err(e) => self.say(format!("  → #{ask} needs live sources, but /research did not start: {e}")),
                }
                return;
            }
            "review" => {
                self.say(format!("  → #{ask} asks for a review: say what to review (Enter sends it)"));
                if self.input.is_empty() {
                    self.input = "/review HEAD~1".chars().collect();
                    self.cursor = self.input.len();
                }
                return;
            }
            other => {
                self.say(format!("  → #{ask} came back as route `{other}`, which Ringo does not know; nothing started"));
                return;
            }
        };
        let Some(agent) = self.owner(&kind) else {
            self.say(format!("  → #{ask} is a task, but no agent can take it now (/setup adds one)"));
            return;
        };
        match self.s.launch_agent(agent, write, &line) {
            Ok(id) => {
                let how = if write { "edits in a worktree" } else { "reads this folder" };
                self.say(format!("  → #{ask} is a task: @{}{} {how} as #{id} (mode: {})", agent.name(),
                                 if write { "!" } else { "" }, self.s.mode.name()));
            }
            Err(e) => self.say(format!("  → #{ask} is a task for @{}: {e}", agent.name())),
        }
    }

    fn spinner(&self) -> &'static str {
        SPIN[(self.started.elapsed().as_millis() / SPIN_MS) as usize % SPIN.len()]
    }

    fn submit(&mut self) {
        let line: String = self.input.iter().collect();
        self.input.clear();
        self.cursor = 0;
        if line.trim().is_empty() {
            return;
        }
        let was_suggested = self.suggested.take().is_some();
        let plain = !line.trim_start().starts_with(['/', '@']);
        if self.route && plain && !was_suggested {
            let route = crate::route::classify(&line, self.s.git);
            let agent = self.owner(&route).map_or("", |a| a.name());
            let needs = matches!(route, crate::route::Route::Edit | crate::route::Route::Read);
            let suggested = crate::route::suggestion(&route, line.trim(), agent).filter(|_| !needs || !agent.is_empty());
            if let Some((cmd, cost)) = suggested {
                self.say(format!("  → suggested: {cmd}\n    ({cost}). Enter sends it; change it if you like; Esc asks it \
                                  as a plain question instead (about 1-2k tokens)."));
                self.input = cmd.chars().collect();
                self.cursor = self.input.len();
                self.suggested = Some(line);
                return;
            }
        }
        self.history.push(line.clone());
        self.hist = self.history.len();
        self.echo(&line);
        match parse(&line, &self.agents.clone()).and_then(|cmd| self.run(cmd)) {
            Ok(Some(text)) => self.say(text),
            Ok(None) => {}
            Err(e) => self.say(format!("  {e}")),
        }
    }

    /// Runs a command; Some(text) goes to the conversation. A started job's own lines follow there as they come.
    fn run(&mut self, cmd: Cmd) -> Result<Option<String>, String> {
        Ok(match cmd {
            Cmd::Help => Some(self.help_text()),
            Cmd::Mode(None) => Some(format!("  mode: {} (Shift-Tab cycles ask, auto, read)", self.s.mode.name())),
            Cmd::Mode(Some(mode)) => Some(self.set_mode(mode)),
            Cmd::Auto => {
                let mode = if self.s.mode == Mode::Auto { Mode::Ask } else { Mode::Auto };
                Some(self.set_mode(mode))
            }
            Cmd::Route(on) => {
                self.route = on;
                let saved = crate::theme::set_setting("route", serde_json::json!(on)).err()
                    .map_or(" (saved for next time)".to_string(), |e| format!(" (not saved: {e})"));
                Some(format!("  route suggestions {}{saved}", if on { "on" } else { "off" }))
            }
            Cmd::Theme(None) => Some(format!("  theme: {} (also: {})", self.theme.name,
                crate::theme::NAMES.iter().filter(|n| **n != self.theme.name).copied().collect::<Vec<_>>().join(", "))),
            Cmd::Theme(Some(name)) => {
                let depth = crate::theme::depth(|k| std::env::var(k).ok());
                match Theme::named(&name, depth) {
                    Some(theme) => {
                        self.theme = theme;
                        match crate::theme::save(theme.name) {
                            Ok(()) => Some(format!("  theme: {} (saved for next time)", theme.name)),
                            Err(e) => Some(format!("  theme: {} (not saved: {e})", theme.name)),
                        }
                    }
                    None => Some(format!("  no theme {name}: {}", crate::theme::NAMES.join(", "))),
                }
            }
            Cmd::Update(None) => match &self.pending {
                _ if self.s.hold.is_some() => {
                    self.s.hold.as_ref().map(|why| format!("  {why}; it reports here when done"))
                }
                None => Some("  the update check is still running; try again in a moment".into()),
                Some(p) if p.is_empty() => Some("  nothing to update (checked when the shell opened)".into()),
                Some(_) if self.s.running() > 0 => {
                    Some("  wait for the running jobs, or /kill them: an update replaces the agent's program".into())
                }
                Some(_) => {
                    self.update = true;
                    None
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
                Some(format!("#{id} {who} · {}: its last {} log lines (the whole stream is in {})\n{}", job.title_full,
                             log.len(), job.dir.display(), log.join("\n")))
            }
            Cmd::Kill(id) => {
                self.s.kill(id)?;
                Some(format!("  stopping #{id}"))
            }
            Cmd::Diff(id) => {
                let patch = self.s.diff(id)?;
                Some(if patch.is_empty() { format!("  #{id} changed nothing") } else { patch })
            }
            Cmd::Apply(id) => Some(format!("  applied #{id} to your tree (not committed):\n{}", self.s.apply(id)?)),
            Cmd::Drop(id) => {
                self.s.drop_worktree(id)?;
                Some(format!("  removed #{id}'s worktree; its logs stay in {}", self.s.job(id)?.dir.display()))
            }
            Cmd::ReviewJob(of) => {
                self.s.review_job(of)?;
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
            Cmd::Agents(agents, prompt) => {
                let expanded = self.s.expand(&prompt)?;
                let mut notes = Vec::new();
                if expanded.len() != prompt.len() {
                    notes.push(format!("  handoff adds {} characters to the task:", expanded.len() - prompt.len()));
                    notes.push(expanded.clone());
                }
                for (agent, write) in agents {
                    if let Err(e) = self.s.launch_agent(agent, write, &expanded) {
                        notes.push(format!("  {}: {e}", agent.name()));
                    }
                }
                if notes.is_empty() { None } else { Some(notes.join("\n")) }
            }
        })
    }

    fn insert(&mut self, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        self.input.splice(self.cursor..self.cursor, chars);
        self.cursor += n;
    }

    /// A paste goes into the input as it is, its lines kept; nothing is sent until Enter.
    fn paste(&mut self, text: &str) {
        self.insert(&clean(text));
    }

    fn recall(&mut self) {
        self.input = self.history.get(self.hist).map(|h| h.chars().collect()).unwrap_or_default();
        self.cursor = self.input.len();
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let burst = self.last_key.elapsed() < BURST;
        self.last_key = Instant::now();
        if !(ctrl && matches!(k.code, KeyCode::Char('c' | 'd'))) {
            self.quit_armed = false;
        }
        // An agent waiting for an answer has the keys once the input is empty: y, a or n (Esc declines); Ctrl-C still
        // quits, anything else is ignored. A line being typed keeps its keys until it is sent or cleared, and keys in
        // the first moment after a request appears are ignored, so a word being typed never answers by accident.
        if let Some((id, _)) = self.s.pending().filter(|_| self.input.is_empty()) {
            if self.asked.as_ref().is_some_and(|(_, at)| at.elapsed() < ANSWER_GRACE) {
                return;
            }
            let decision = match k.code {
                KeyCode::Char('y' | 'Y') if !ctrl => Some(Decision::Once),
                KeyCode::Char('a' | 'A') if !ctrl => Some(Decision::Session),
                KeyCode::Char('n' | 'N') if !ctrl => Some(Decision::Deny),
                KeyCode::Esc => Some(Decision::Deny),
                KeyCode::Char('c' | 'd') if ctrl => None,
                _ => return,
            };
            if let Some(decision) = decision {
                let said = self.s.decide(id, decision).unwrap_or_else(|e| e);
                self.say(format!("  #{id}: {said}"));
                return;
            }
        }
        match k.code {
            KeyCode::Char('c' | 'd') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else if self.s.running() > 0 && !self.quit_armed {
                    self.quit_armed = true;
                    let n = self.s.running();
                    self.say(format!("  {n} job{} running; Ctrl-C again stops {} and quits",
                                     if n == 1 { " is" } else { "s are" }, if n == 1 { "it" } else { "them" }));
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.len(),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Char(c) if !ctrl => self.insert(c.encode_utf8(&mut [0; 4])),
            KeyCode::Enter if alt => self.insert("\n"),
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
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
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
            KeyCode::BackTab => {
                let text = self.set_mode(self.s.mode.next());
                self.say(text);
            }
            KeyCode::Esc if self.suggested.is_some() => {
                // a suggestion declined: ask the original line as a plain question instead
                let line = self.suggested.take().unwrap_or_default();
                self.input = line.chars().collect();
                self.suggested = Some(String::new()); // so this submit is not routed again
                self.submit();
            }
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            _ => {}
        }
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
            let calls = if job.calls > 0 { format!(" {}c", job.calls) } else { String::new() };
            let stats = format!("  {} {}{calls}  ", fmt_age(job.age()), fmt_tokens(job.tokens.total()));
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

    /// What the live area holds for a terminal of `width` x `height`.
    fn live(&self, width: u16, height: u16) -> Live {
        let w = width as usize;
        let jobs = self.job_rows(w, (height / 3).max(2) as usize);
        let ask = self.prompt_lines(w.saturating_sub(2));
        let ask_h = ask.as_ref().map_or(0, |(_, lines)| (lines.len() as u16 + 2).clamp(3, (height / 2).max(3)));
        let (rows, at) = input_rows(&self.input, self.cursor, w.saturating_sub(2));
        let shown = rows.len().min((height / 4).clamp(1, 10) as usize);
        let top = (at.0 + 1).saturating_sub(shown);
        Live { jobs, ask, ask_h, rows, at, top, shown }
    }

    fn draw(&self, f: &mut Frame, live: &Live) {
        let t = self.theme;
        let [jobs, asked, input, foot] = Layout::vertical([Constraint::Length(live.jobs.len() as u16),
            Constraint::Length(live.ask_h), Constraint::Length(live.shown as u16 + 2), Constraint::Length(1)]).areas(f.area());
        let block = |title: String| Block::bordered().border_type(BorderType::Rounded).border_style(t.border)
            .title(Span::styled(title, t.title));
        f.render_widget(Paragraph::new(live.jobs.clone()), jobs);
        if let Some((title, lines)) = &live.ask {
            // The request, wrapped, above the input, from its first line: a long one shows how much more there is (all
            // of it is in the conversation above), and the keys stay on the last line.
            let room = asked.height.saturating_sub(2) as usize;
            let mut lines = lines.clone();
            let keys = lines.pop().unwrap_or_default();
            let mut shown = lines.clone();
            if shown.len() + 1 > room {
                shown.truncate(room.saturating_sub(2));
                let more = lines.len() - shown.len();
                shown.push(Line::styled(format!("… {more} more lines, all of them in the conversation above"), t.warn));
            }
            shown.push(keys);
            f.render_widget(Paragraph::new(shown).block(block(title.clone()).border_style(t.warn)), asked);
        }
        let hint = " @claude or @codex for an agent · Ctrl-J new line · /usrman ".to_string();
        let rows: Vec<Line> = live.rows.iter().skip(live.top).take(live.shown).map(|r| Line::raw(r.clone())).collect();
        f.render_widget(Paragraph::new(rows).block(block(hint)), input);
        f.set_cursor_position((input.x + 1 + live.at.1 as u16, input.y + 1 + (live.at.0 - live.top) as u16));
        let status = format!(" {} · mode: {} · {} running · {} tokens", self.label, self.s.mode.name(), self.s.running(),
                             fmt_tokens(self.s.tokens()));
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled(" ringo ", t.brand), Span::styled(status, t.header)]))
            .style(t.header), foot);
    }

    /// A waiting permission request as the live area shows it: its title, then every line of the action, wrapped,
    /// and the keys.
    fn prompt_lines(&self, width: usize) -> Option<(String, Vec<Line<'static>>)> {
        let (id, p) = self.s.pending()?;
        let job = self.s.job(id).ok()?;
        let (who, _) = self.who(job);
        let more = self.s.waiting() - 1;
        let title = format!(" {who} #{id} asks before it acts{} ", if more > 0 { format!(" · {more} more waiting") } else { String::new() });
        let mut lines: Vec<Line<'static>> = p.what.lines().flat_map(|l| wrap(&clean(l), width)).map(Line::raw).collect();
        let keys = if !self.input.is_empty() {
            "send or clear (Esc) your line to answer: y allow · a allow for this session · n decline".to_string()
        } else if self.s.always_asks(p) {
            "y allow this once (an always-ask rule: it asks every time) · n decline".to_string()
        } else {
            format!("y allow · a allow `{}` for this session · n decline", ringo_cli::parse::short(&p.rule, 60))
        };
        lines.push(Line::styled(keys, self.theme.dim));
        Some((title, lines))
    }
}

pub fn run() -> ExitCode {
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
            if let Ok(me) = std::env::current_exe() {
                s.pipeline = Some(vec![me.into()]); // this binary: native pipelines unless a release says otherwise
            }
            s
        }
        Err(e) => {
            eprintln!("ringo: {e}");
            return ExitCode::from(2);
        }
    };
    let branch = std::process::Command::new("git").arg("-C").arg(&session.repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"]).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let name = session.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let label = match branch { Some(b) => format!("{name} ({b})"), None => name };
    let mut app = App { s: session, input: Vec::new(), cursor: 0, history: Vec::new(), hist: 0, out: Vec::new(),
                        recent: VecDeque::new(), width: 0, ended: Vec::new(), last_key: Instant::now(), last: 0, quit_armed: false, quit: false, setup: false, label, agents,
                        theme: Theme::load(), started: Instant::now(),
                        route: crate::theme::setting("route").and_then(|v| v.as_bool()).unwrap_or(false),
                        suggested: None, asked: None, pending: None, update: false };
    if let Some(mode) = crate::theme::setting("mode").and_then(|v| v.as_str().and_then(Mode::from_name)) {
        app.s.mode = mode;
    }
    let found: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
    let greeting = format!(" {} · agents: {} (/setup adds another) · mode: {}", app.label, found.join(", "), app.s.mode.name());
    app.out.push(Line::from(vec![Span::styled(" ringo ", app.theme.brand), Span::styled(greeting, app.theme.dim)]));
    app.say("Ask anything, or @claude / @codex for an agent turn. /usrman shows the manual.");
    let (notes, news) = std::sync::mpsc::channel();
    let (agents, state) = (app.agents.clone(), ringo_cli::state_root().join("engine-versions.json"));
    let auto = crate::theme::setting("auto_update").and_then(|v| v.as_bool()).unwrap_or(false);
    let check = notes.clone();
    std::thread::spawn(move || {
        let _ = check.send((false, crate::updates::check(&agents, &state)));
    });

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        hook(info);
    }));
    let mut shape = 4; // the live area's height; it follows what the area holds
    let mut terminal = match open(shape) {
        Ok(terminal) => terminal,
        Err(e) => {
            let _ = disable_raw_mode();
            eprintln!("ringo: cannot open the terminal: {e}");
            return ExitCode::from(2);
        }
    };
    let (mut dirty, mut tick) = (true, Instant::now());
    while !app.quit {
        if dirty {
            if paint(&mut terminal, &mut app, &mut shape).is_err() {
                break;
            }
            dirty = false;
        }
        // Job events arrive on a channel, not as terminal input, so the poll timeout is how late one is shown: 25 ms
        // while jobs run (a pump is a few microseconds; redraws still happen only when something changed). Every event
        // already queued is taken before the next redraw.
        let wait = if app.s.running() > 0 { 25 } else { 250 };
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
                    Ok(Term::Resize(..)) => dirty = true,
                    _ => {}
                }
                if app.quit || app.setup || app.update || !event::poll(Duration::ZERO).unwrap_or(false) {
                    break;
                }
            }
        }
        if app.setup {
            let _ = print(&mut terminal, &mut app);
            close(&mut terminal); // the agents' own sign-in runs in the plain terminal
            app.agents = crate::setup::run();
            match open(shape) {
                Ok(t) => terminal = t,
                Err(_) => break,
            }
            app.setup = false;
            dirty = true;
            let names: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
            app.say(format!("  agents set up: {}", names.join(", ")));
        }
        dirty |= app.s.pump();
        while let Some((ask, route)) = app.s.routed.pop_front() {
            app.start_routed(ask, &route);
            dirty = true;
        }
        let asking = app.s.pending().map(|(_, p)| p.id.clone());
        if asking != app.asked.as_ref().map(|(id, _)| id.clone()) {
            app.asked = asking.map(|id| (id, Instant::now()));
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
            let _ = print(&mut terminal, &mut app);
            close(&mut terminal); // the updates run in the plain terminal, so their prompts and progress show
            println!("Updating agents; the shell comes back when they finish.");
            let (lines, left) = crate::updates::apply_here(app.pending.take().unwrap_or_default());
            match open(shape) {
                Ok(t) => terminal = t,
                Err(_) => break,
            }
            lines.iter().for_each(|line| app.say(line));
            app.pending = Some(left);
            app.update = false;
            dirty = true;
        }
        app.feed();
        dirty |= !app.out.is_empty();
        if app.s.running() > 0 && tick.elapsed() >= Duration::from_millis(SPIN_MS as u64) {
            dirty = true; // the spinner and the running clocks
            tick = Instant::now();
        }
    }
    app.feed();
    let _ = print(&mut terminal, &mut app);
    close(&mut terminal);
    app.s.shutdown();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(non_snake_case)] // the names the agents had as enum variants, so this test reads as before
    fn parses_routes_and_commands() {
        let named = |n: &str| Agent::from_name(n).unwrap();
        let (Codex, Claude, Grok) = (named("codex"), named("claude"), named("grok"));
        let all = [Codex, Claude, Grok];
        let parse = |line: &str, _: Agent| super::parse(line, &all);
        assert_eq!(parse("why is it slow?", Codex), Ok(Cmd::Ask("why is it slow?".into())));
        assert!(parse("   ", Codex).is_err());
        assert_eq!(parse("/update", Codex), Ok(Cmd::Update(None)));
        assert_eq!(parse("/update auto on", Codex), Ok(Cmd::Update(Some(true))));
        assert_eq!(parse("/update auto off", Codex), Ok(Cmd::Update(Some(false))));
        assert!(parse("/update auto", Codex).is_err());
        assert_eq!(parse("@codex why is it slow?", Codex), Ok(Cmd::Agents(vec![(Codex, false)], "why is it slow?".into())));
        assert_eq!(parse("@claude! fix it", Codex), Ok(Cmd::Agents(vec![(Claude, true)], "fix it".into())));
        assert_eq!(parse("@codex @claude! do x", Grok), Ok(Cmd::Agents(vec![(Codex, false), (Claude, true)], "do x".into())));
        assert_eq!(parse("@all compare", Codex), Ok(Cmd::Agents(vec![(Codex, false), (Claude, false)], "compare".into())));
        assert!(parse("@nobody x", Codex).unwrap_err().contains("agy"));
        let with_agy = [Codex, named("agy"), Grok];
        assert_eq!(super::parse("@all! x", &with_agy),
                   Ok(Cmd::Agents(vec![(Codex, true), (named("agy"), true), (Grok, true)], "x".into())));
        assert_eq!(super::parse("@all x", &with_agy), Ok(Cmd::Agents(vec![(Codex, false), (named("agy"), false)], "x".into())));
        assert!(parse("@codex", Codex).is_err());
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
        assert_eq!(parse("line one\nline two", Codex), Ok(Cmd::Ask("line one\nline two".into())));
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
    fn the_conversation_goes_into_the_scrollback_word_wrapped() {
        use ratatui::backend::TestBackend;
        let rows = |b: &ratatui::buffer::Buffer| -> Vec<String> {
            b.content.chunks(b.area.width as usize)
                .map(|r| r.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string()).collect()
        };
        let mut terminal = Terminal::with_options(TestBackend::new(10, 4), TerminalOptions { viewport: Viewport::Inline(2) })
            .unwrap();
        flush(&mut terminal, vec![Line::raw("one"), Line::raw("two words wrap"), Line::raw("three")]).unwrap();
        let mut all = rows(terminal.backend().scrollback());
        all.extend(rows(terminal.backend().buffer()));
        let all: Vec<String> = all.into_iter().filter(|r| !r.is_empty()).collect();
        assert_eq!(all, ["one", "two words", "wrap", "three"]);
        // more than one print's worth of rows still arrives whole, in order
        let many: Vec<Line> = (0..CHUNK_ROWS + 5).map(|i| Line::raw(i.to_string())).collect();
        flush(&mut terminal, many).unwrap();
        let mut all = rows(terminal.backend().scrollback());
        all.extend(rows(terminal.backend().buffer()));
        let tail: Vec<String> = all.into_iter().filter(|r| !r.is_empty()).rev().take(3).collect();
        assert_eq!(tail, [(CHUNK_ROWS + 4).to_string(), (CHUNK_ROWS + 3).to_string(), (CHUNK_ROWS + 2).to_string()]);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_tokens(870), "870");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_age(Duration::from_secs(75)), "1:15");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
    }
}

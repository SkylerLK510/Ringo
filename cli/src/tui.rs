//! The `ringo` shell. The conversation, and what every agent does, prints into the terminal's own scrollback, as
//! Claude Code's and Codex's do, so the mouse wheel scrolls it and a drag selects its text alone. Below it, a live area
//! holds the running jobs, any permission request, the input box and a status line. That area redraws only when
//! something changed (an event, a key), and for the spinner (every SPIN_MS) while jobs run.

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event as Term, KeyCode, KeyEvent,
                                KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
                                PushKeyboardEnhancementFlags};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use ratatui::layout::{Constraint, Layout};
use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget, Wrap};
use ratatui::{DefaultTerminal, Frame, Terminal, TerminalOptions, Viewport};
use ringo_cli::{Agent, Decision, Kind, Mode, Said, Session, Status};
use std::collections::VecDeque;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
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

Keys: Enter sends. Shift-Enter (in terminals that report it: kitty, Ghostty,
WezTerm, iTerm2 with CSI u on), Ctrl-J, Alt-Enter or \\ then Enter starts a new
line; a paste keeps its lines and is not sent. Typing / or @ opens a menu
under the input: Up / Down choose, Tab or Enter completes. Otherwise Up / Down
move between lines, then through earlier messages (kept between sessions).
Shift-Tab changes the mode, Esc clears, Ctrl-C twice quits. The conversation
is in the terminal's scrollback: scroll it and select from it as in any other
program.";

/// What the menu offers as a line starts with `/`: each command as it reads, and what it does.
const COMMANDS: &[(&str, &str)] = &[
    ("/usrman", "the manual"),
    ("/mode ask|auto|read", "what agents may do without asking"),
    ("/auto", "auto mode on, or back to ask"),
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
    ("/update", "update the agents found out of date"),
    ("/route on|off", "suggest a command for a line that reads like a task"),
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
    for &(name, read) in agents.iter().chain(&[("all", true)]) {
        let (asks, edits) = if name == "all" {
            ("every set-up agent reads and answers".to_string(), "every set-up agent edits, each in its own worktree".to_string())
        } else {
            (format!("{name} reads and answers; it cannot edit"), format!("{name} edits in its own worktree; /apply lands it"))
        };
        for (tag, what) in [(format!("@{name}"), asks), (format!("@{name}!"), edits)] {
            if tag.starts_with(word) && (read || tag.ends_with('!')) {
                entries.push((format!("{tag} "), tag, what));
            }
        }
    }
    entries
}

/// Whether the terminal reports Shift-Enter apart from Enter (the kitty keyboard protocol is on).
static SHIFT_ENTER: AtomicBool = AtomicBool::new(false);

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
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(history_file()) {
        let _ = writeln!(file, "{}", serde_json::json!(line));
    }
}

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
/// when it fits (else one block per row: its first cell, then each other cell under its column's name).
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
                    self.emit(vec![Span::styled(format!("  {line}"), self.t.ringo)]);
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
        let depth: usize = self.lists.iter().map(|(_, w)| *w).sum();
        let mark_w = self.lists.last().map_or(0, |(_, w)| *w);
        let marker = self.marker.take().unwrap_or_else(|| " ".repeat(mark_w));
        let quote = "│ ".repeat(self.quote);
        let hang = cols(&quote) + depth;
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
        if total + depth <= self.width {
            let line = |row: &[Vec<Span<'static>>], head: bool| -> Vec<Span<'static>> {
                let mut spans = Vec::new();
                for (c, w) in widths.iter().enumerate() {
                    let cell = row.get(c).cloned().unwrap_or_default();
                    let pad = w - width(&cell) + if c + 1 < n { 2 } else { 0 };
                    spans.extend(cell.into_iter().map(|s| if head { s.patch_style(bold) } else { s }));
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans
            };
            let (head, rule) = (line(&table.head, true), vec![Span::styled("─".repeat(total), self.t.dim)]);
            self.emit(head);
            self.emit(rule);
            for row in &table.rows {
                let spans = line(row, false);
                self.emit(spans);
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
    terminal.insert_before(rows as u16, |buf| {
        text.render(buf.area, buf);
        uncover(buf);
    })
}

/// ratatui prints every cell of an inserted line, the cell a wide character covers too (a space). The terminal has
/// already moved two columns for that character, so the space would push the rest of the row one column right; an
/// empty cell prints nothing.
fn uncover(buf: &mut ratatui::buffer::Buffer) {
    use unicode_width::UnicodeWidthStr;
    let width = buf.area.width as usize;
    for i in 0..buf.content.len() {
        let covers = buf.content[i].symbol().width().saturating_sub(1);
        for k in 1..=covers.min(width - 1 - i % width) {
            buf.content[i + k].set_symbol("");
        }
    }
}

/// An inline live area of `height` rows at the cursor: no alternate screen, so the terminal keeps its scrollback.
fn inline(height: u16) -> std::io::Result<DefaultTerminal> {
    Terminal::with_options(CrosstermBackend::new(stdout()), TerminalOptions { viewport: Viewport::Inline(height) })
}

fn open(height: u16) -> std::io::Result<DefaultTerminal> {
    enable_raw_mode()?;
    // A paste arrives as one event, not as keys (its newlines as Enter). Not fatal: the legacy Windows console has no
    // bracketed paste, and there a paste's keys are told apart by their timing (see BURST).
    let _ = execute!(stdout(), EnableBracketedPaste);
    // Shift-Enter reaches the shell only where the terminal speaks the kitty keyboard protocol; elsewhere it is Enter.
    let keys = supports_keyboard_enhancement().unwrap_or(false)
        && execute!(stdout(), PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)).is_ok();
    SHIFT_ENTER.store(keys, Ordering::Relaxed);
    inline(height)
}

/// The keyboard protocol and bracketed paste off, and raw mode left.
fn restore() {
    if SHIFT_ENTER.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
}

/// The live area goes and the terminal is as it was (for an agent's sign-in, an update, or the end); the
/// conversation stays in the scrollback.
fn close(terminal: &mut DefaultTerminal) {
    let top = terminal.get_frame().area().y;
    let _ = terminal.clear();
    let _ = terminal.set_cursor_position((0, top));
    let _ = terminal.show_cursor();
    restore();
}

/// A live area of a new height, where the old one began (growing pushes the conversation up into the scrollback).
fn reshape(terminal: &mut DefaultTerminal, height: u16) -> std::io::Result<()> {
    let top = terminal.get_frame().area().y;
    terminal.clear()?;
    terminal.set_cursor_position((0, top))?;
    *terminal = inline(height)?;
    Ok(())
}

/// A conversation entry. It is fitted to the terminal's width when it prints (and again if the window narrows).
#[derive(Clone, Debug)]
enum Out {
    /// A line; a row past the width continues this many columns in.
    Line(Line<'static>, usize),
    /// An agent's reply, as markdown, two columns in.
    Reply(String),
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

/// How many printed entries the shell keeps, to print the end of the conversation again after the window narrows.
const RECENT: usize = 300;

/// Prints the conversation lines that wait for the scrollback.
fn print(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    terminal.autoresize()?;
    let width = terminal.size()?.width as usize;
    let out = std::mem::take(&mut app.out);
    let lines: Vec<Line<'static>> = out.iter().flat_map(|o| o.rows(width, &app.theme)).collect();
    app.recent.extend(out);
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
        let mut again: Vec<Line<'static>> = Vec::new();
        for entry in app.recent.iter().rev() {
            // the end of an entry taller than what is left (a long reply) still comes back
            let rows = entry.rows(size.width as usize, &app.theme);
            again.extend(rows.into_iter().rev().take(room.saturating_sub(again.len())));
            if again.len() >= room {
                break;
            }
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
    out: Vec<Out>,
    recent: VecDeque<Out>,
    width: u16,
    /// The menu's chosen row (see `menu`).
    pick: usize,
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
    menu: Vec<Line<'static>>,
}

impl Live {
    fn height(&self) -> u16 {
        // the input's border, the status line
        self.jobs.len() as u16 + self.ask_h + self.shown as u16 + 2 + self.menu.len() as u16 + 1
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
            self.out.push(Out::text(line));
        }
        self.last = 0;
    }

    /// A blank line, unless the conversation just had one.
    fn gap(&mut self) {
        if self.out.last().or(self.recent.back()).is_some_and(|o| !o.blank()) {
            self.out.push(Out::Line(Line::default(), 0));
        }
    }

    /// The owner's own line in the conversation, after a blank line.
    fn echo(&mut self, line: &str) {
        self.gap();
        for (i, l) in clean(line).lines().enumerate() {
            self.out.push(Out::Line(Line::from(vec![Span::styled(if i == 0 { "› " } else { "  " }, self.theme.focus),
                                                    Span::styled(l.to_string(), self.theme.selected)]), 2));
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
            let (id, ends) = (self.s.jobs[i].id, !self.s.jobs[i].running() && !self.ended[i]);
            let (who, style) = self.who(&self.s.jobs[i]);
            if !fresh.is_empty() && self.last != id {
                self.gap();
                self.out.push(Out::Line(Line::from(vec![Span::styled(format!("#{id} "), self.theme.dim),
                                                        Span::styled(who, style)]), 0));
            }
            for said in fresh {
                match said {
                    Said::Reply(text) => {
                        self.gap();
                        self.out.push(Out::Reply(clean(&text)));
                        self.gap();
                    }
                    Said::Step(text) => {
                        let text = clean(&text);
                        self.out.push(Out::Line(Line::from(vec![Span::styled("  • ", style),
                                                                Span::styled(text, self.theme.dim)]), 4));
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
                let lines = self.job_end(&self.s.jobs[i]);
                self.out.extend(lines.into_iter().map(|l| Out::Line(l, 2)));
                self.ended[i] = true;
                self.last = 0;
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
        if self.history.last() != Some(&line) {
            save_history(&line);
            self.history.push(line.clone());
        }
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
                let wt = job.worktree.as_ref().map_or(String::new(), |w| format!("; its worktree: {}", w.display()));
                Some(format!("#{id} {who} · {}: its last {} log lines (the whole stream is in {}{wt})\n{}", job.title_full,
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

    fn menu_entries(&self) -> Vec<(String, String, String)> {
        let before: String = self.input[..self.cursor].iter().collect();
        let agents: Vec<(&str, bool)> = self.agents.iter().map(|a| (a.name(), a.spec().read)).collect();
        menu(&before, &agents)
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
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
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
            KeyCode::BackTab | KeyCode::Tab if k.code == KeyCode::BackTab || shift => {
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
            let calls = match job.calls { 0 => String::new(), 1 => " · 1 call".into(), n => format!(" · {n} calls") };
            let tokens = match job.tokens.total() { 0 => String::new(), n => format!(" · {} tokens", fmt_tokens(n)) };
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

    /// What the live area holds for a terminal of `width` x `height`.
    fn live(&self, width: u16, height: u16) -> Live {
        let w = width as usize;
        let jobs = self.job_rows(w, (height / 3).max(2) as usize);
        let ask = self.prompt_lines(w.saturating_sub(2));
        let ask_h = ask.as_ref().map_or(0, |(_, lines)| (lines.len() as u16 + 2).clamp(3, (height / 2).max(3)));
        let (rows, at) = input_rows(&self.input, self.cursor, w.saturating_sub(2));
        let shown = rows.len().min((height / 4).clamp(1, 10) as usize);
        let top = (at.0 + 1).saturating_sub(shown);
        let entries = self.menu_entries();
        let room = (height / 4).clamp(1, 6) as usize;
        let pick = self.pick.min(entries.len().saturating_sub(1));
        let first = (pick + 1).saturating_sub(room);
        let label = entries.iter().map(|(_, l, _)| cols(l)).max().unwrap_or(0);
        let t = &self.theme;
        let menu = entries.iter().enumerate().skip(first).take(room)
            .map(|(i, (_, l, what))| {
                let (mark, style) = if i == pick { ("› ", t.focus) } else { ("  ", t.dim) };
                Line::from(vec![Span::styled(format!(" {mark}{l}{} ", " ".repeat(label - cols(l))), style),
                                Span::styled(what.clone(), t.dim)])
            })
            .collect();
        Live { jobs, ask, ask_h, rows, at, top, shown, menu }
    }

    fn draw(&self, f: &mut Frame, live: &Live) {
        let t = self.theme;
        let [jobs, asked, input, menu, foot] = Layout::vertical([Constraint::Length(live.jobs.len() as u16),
            Constraint::Length(live.ask_h), Constraint::Length(live.shown as u16 + 2),
            Constraint::Length(live.menu.len() as u16), Constraint::Length(1)]).areas(f.area());
        f.render_widget(Paragraph::new(live.menu.clone()), menu);
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
        let newline = if SHIFT_ENTER.load(Ordering::Relaxed) { "Shift-Enter" } else { "Ctrl-J" };
        let hint = format!(" / commands · @ agents · {newline} new line ");
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
    let mut app = App { s: session, input: Vec::new(), cursor: 0, history: load_history(), hist: 0, out: Vec::new(),
                        recent: VecDeque::new(), width: 0, pick: 0, ended: Vec::new(), last_key: Instant::now(), last: 0, quit_armed: false, quit: false, setup: false, label, agents,
                        theme: Theme::load(), started: Instant::now(),
                        route: crate::theme::setting("route").and_then(|v| v.as_bool()).unwrap_or(false),
                        suggested: None, asked: None, pending: None, update: false };
    app.hist = app.history.len();
    if let Some(mode) = crate::theme::setting("mode").and_then(|v| v.as_str().and_then(Mode::from_name)) {
        app.s.mode = mode;
    }
    let found: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
    let greeting = format!(" {} · agents: {} (/setup adds another) · mode: {}", app.label, found.join(", "), app.s.mode.name());
    app.out.push(Out::Line(Line::from(vec![Span::styled(" ringo ", app.theme.brand), Span::styled(greeting, app.theme.dim)]), 1));
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
        restore();
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
        assert_eq!(md("```\nlet x = 1;\n```", 20), ["  let x = 1;"]);
        assert_eq!(md("> aaa bbb ccc", 8), ["│ aaa", "│ bbb", "│ ccc"], "a quote's wrapped rows keep its bar");
        assert_eq!(md("1. a\n2. b\n   - c", 20), ["1. a", "2. b", "   • c"]);
        assert_eq!(md("see [the docs](https://x.dev) and [a.rs](src/a.rs)", 60), ["see the docs (https://x.dev) and a.rs"]);
    }

    #[test]
    fn the_menu_offers_commands_and_agents() {
        let agents = [("claude", true), ("grok", false)];
        let to = |before: &str| menu(before, &agents).into_iter().map(|(to, ..)| to).collect::<Vec<_>>();
        assert_eq!(to("/re"), ["/review ", "/review ", "/research "]);
        assert_eq!(to("/usr"), ["/usrman"], "a command that takes nothing more completes without a space");
        assert_eq!(to("@"), ["@claude ", "@claude! ", "@grok! ", "@all ", "@all! "], "grok has no read-only mode");
        assert_eq!(to("fix it @cl"), ["@claude ", "@claude! "]);
        assert!(to("a /re").is_empty(), "a command only as the line's first word");
        assert!(to("ping @someone").is_empty() && to("plain words").is_empty());
    }

    #[test]
    fn a_wide_character_does_not_shift_its_row() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 1));
        buf.set_string(0, 0, "\u{2705} ok", Style::default());
        uncover(&mut buf);
        let symbols: Vec<&str> = buf.content.iter().map(|c| c.symbol()).collect();
        assert_eq!(symbols, ["\u{2705}", "", " ", "o", "k", " "]);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_tokens(870), "870");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_age(Duration::from_secs(75)), "1:15");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
    }
}

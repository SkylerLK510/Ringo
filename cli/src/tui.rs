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
    ("/ask TEXT", "a quick answer from one cheap call, without your files"),
    ("/mode ask|auto|read", "what agents may do without asking"),
    ("/tier quick|standard|deep|ultra", "how hard agents think; ultra also reviews each change and fixes what it finds"),
    ("/usage", "what you have used, and how much of your plans is left"),
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
    /// A plain line: for the agent the owner is talking to.
    Talk(String),
    Agents(Vec<(Agent, bool)>, String),
    /// `/ask TEXT`: one tool-free call.
    Ask(String),
    Undo,
    Copy,
    Clear,
    Compact,
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
    /// `/mode` (None: show it) or `/mode ask|auto|read`.
    Mode(Option<Mode>),
    /// `/auto`: auto mode on, or back to ask mode.
    Auto,
    /// `/tier` (None: show it) or `/tier quick|standard|deep|ultra`.
    Tier(Option<Tier>),
    Usage,
    Quit,
}

fn job_number(word: Option<&str>) -> Result<usize, String> {
    word.map(|w| w.trim_start_matches('#')).and_then(|w| w.parse().ok()).ok_or("give a job number, e.g. 3".into())
}

/// One input line as a command. A line without `/` or `@` is for the agent the owner is talking to; @all means every
/// set-up agent.
pub fn parse(line: &str, all: &[Agent]) -> Result<Cmd, String> {
    let line = line.trim();
    if line == "?" {
        return Ok(Cmd::Keys);
    }
    if let Some(rest) = line.strip_prefix('/') {
        let mut words = rest.split_whitespace();
        let name = words.next().unwrap_or("");
        let tail = rest[name.len()..].trim();
        return match name {
            "usrman" | "instructions" | "manual" | "help" | "?" => Ok(Cmd::Help),
            "quit" | "exit" | "q" => Ok(Cmd::Quit),
            "setup" => Ok(Cmd::Setup),
            "undo" => Ok(Cmd::Undo),
            "copy" => Ok(Cmd::Copy),
            "clear" | "new" => Ok(Cmd::Clear),
            "compact" => Ok(Cmd::Compact),
            "ask" if !tail.is_empty() => Ok(Cmd::Ask(tail.into())),
            "auto" => Ok(Cmd::Auto),
            "usage" | "cost" => Ok(Cmd::Usage),
            "tier" | "effort" => match words.next() {
                None => Ok(Cmd::Tier(None)),
                Some(t) => Tier::from_name(t).map(|t| Cmd::Tier(Some(t)))
                    .ok_or("/tier quick, /tier standard, /tier deep or /tier ultra".into()),
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
        return Err("say what the agent should do".into());
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
fn plan_line(agent: &str, plan: &ringo_cli::Plan) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let windows: Vec<String> = plan.windows.iter().map(|w| {
        let reset = w.resets.map(|r| format!(", starts over in {}", fmt_until(r.saturating_sub(now)))).unwrap_or_default();
        format!("{} {:.0}% used{reset}", w.name, w.percent)
    }).collect();
    let name = if plan.name.is_empty() { String::new() } else { format!(" ({} plan)", plan.name) };
    let notes = plan.notes.iter().map(|n| format!(" · {n}")).collect::<String>();
    match windows.is_empty() {
        true => format!("  {agent}{name}: no limits reported{notes}"),
        false => format!("  {agent}{name}: {}{notes}", windows.join(" · ")),
    }
}

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
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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
    /// A step an agent took, on one row under its job.
    Step(String),
    /// The box the shell opens with, a line per row (the first is its title).
    Welcome(Vec<String>),
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
            Out::Step(text) => vec![step_row(text, width, theme)],
            Out::Welcome(lines) => {
                // the title in the brand colour, the rest under it, dim
                let mut rows = Vec::new();
                for (i, text) in lines.iter().enumerate() {
                    let style = if i == 0 { theme.brand.add_modifier(Modifier::BOLD) } else { theme.dim };
                    rows.extend(fit(&Line::from(vec![Span::raw(" "), Span::styled(text.clone(), style)]), width, 1));
                }
                rows
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
    let cut = |t: &str, room: usize| {
        let (mut out, mut used) = (String::new(), 0);
        for c in t.chars() {
            used += c.width().unwrap_or(0);
            if used + 1 > room {
                break;
            }
            out.push(c);
        }
        format!("{}…", out.trim_end())
    };
    match text.rsplit_once(" · ") {
        Some((what, came)) if cols(came) + 3 <= width / 2 => format!("{} · {came}", cut(what, width - cols(came) - 3)),
        _ => cut(text, width),
    }
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

/// A path under the home folder as `~/…`.
fn home_short(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    match path.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// A spot in the conversation: (row, column).
type Spot = (usize, usize);

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
    rows: usize,
    /// Rows scrolled up from the end (0: following it), where the conversation showed in the last frame and its
    /// first row there, and the text selected with the mouse (from, to) and whether a drag is under way.
    scroll: usize,
    view: Rect,
    top: usize,
    sel: Option<(Spot, Spot)>,
    dragging: bool,
    /// A short note at the input's right end (copied, could not copy) and when it appeared.
    toast: Option<(String, Instant)>,
    /// How the last job ended (true: done) and when, for the cat; and the cat's look last drawn.
    ended_at: Option<(bool, Instant)>,
    drawn_look: (&'static str, usize),
    /// Each job's reply being written: its place in `out`, and its pieces as they came.
    streams: std::collections::HashMap<usize, (usize, String)>,
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
    /// `/usage`'s plan checks still under way, one thread an agent.
    plans: Vec<std::thread::JoinHandle<(Agent, Result<ringo_cli::Plan, String>)>>,
    /// Ultra: Ringo's review under way of a turn here (review job, turn, its agent), during which lines wait; the
    /// revision it asked for (its task, while queued) and that revision's job, which no review follows.
    checking: Option<(usize, usize, Agent)>,
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
              fitted_w: 0, rows: 0, scroll: 0, view: Rect::default(), top: 0, sel: None, dragging: false, toast: None,
              ended_at: None, drawn_look: ("", 0), streams: Default::default(),
              main: first_choice(&agents), queue: VecDeque::new(), pick: 0, choice: 0, ended: Vec::new(),
              last_key: Instant::now(), last: 0, quit_armed: false, quit: false, setup: false, label, agents, theme,
              started: Instant::now(), asked: None, pending: None, update: false, plans: Vec::new(), checking: None,
              revision: None, revision_id: None, quota: None, reviewer: None }
    }

    /// The box and the three lines the shell opens with.
    fn welcome(&mut self) {
        let found: Vec<&str> = self.agents.iter().map(|a| a.name()).collect();
        self.out.push(Out::Welcome(vec![
            "Welcome to Ringo".into(),
            format!("{} · agents {} (/setup adds another) · {} mode (Shift-Tab changes it)", home_short(&self.s.repo),
                    found.join(", "), self.s.mode.name()),
        ]));
        self.gap();
        let undo = match &self.s.no_undo {
            None => "; /undo takes back its last change.".to_string(),
            Some(why) => format!(". /undo is off here: {why}."),
        };
        self.say(format!("  Type what you want done, in your own words, and press Enter. {} works in this folder and \
                          asks before it runs a command{undo}", self.main.name()));
        self.say("  @codex or @claude picks who you talk to · / lists the commands · ? the keys · /help the guide");
        // Started in the home folder (or above it), a turn has no /undo and no project's instructions, and spends
        // its first steps finding the project it was asked about: one turn read 3.9M tokens that way.
        if self.s.at_home {
            self.say("  Ringo is open in your home folder (or a folder above it). For work on a project, quit (Ctrl-C \
                      twice), cd into the project and \
                      start ringo there: /undo works, the project's own instructions load, and the agent does not \
                      have to search for it.");
        }
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
        for (i, l) in clean(line).lines().enumerate() {
            let label = if i == 0 { " you  " } else { "      " };
            self.out.push(Out::Line(Line::from(vec![Span::styled(label, self.theme.focus.add_modifier(Modifier::BOLD)),
                                                    Span::styled(l.to_string(), self.theme.selected)]), 6));
        }
        self.last = 0;
    }

    fn note(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    /// The settings in force, printed under the manual.
    fn settings(&self) -> String {
        format!("  now: mode {} · tier {} · talking to {} · questions and pipelines on {} · theme {}\n  private state: {}",
                self.s.mode.name(), self.s.tier.name(), self.main.name(), self.s.backend, self.theme.name, self.s.dir.display())
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
                self.out.push(Out::Line(Line::from(vec![Span::styled(format!(" {who}"), style.add_modifier(Modifier::BOLD)),
                                                        Span::styled(format!("  #{id}"), self.theme.dim)]), 0));
            }
            for said in fresh {
                match said {
                    // A piece of the reply grows the job's open reply, or opens one; the whole reply replaces it.
                    // ceiling: the growing reply is fitted again (its markdown parsed) for each frame it grows in.
                    // The raw pieces are kept and cleaned whole, so a line end split across two pieces stays one.
                    Said::Partial(text) => match self.streams.get_mut(&id) {
                        Some((at, raw)) if *at < self.out.len() => {
                            raw.push_str(&text);
                            self.out[*at] = Out::Reply(clean(raw));
                            self.fitted.truncate(*at);
                        }
                        _ => {
                            self.gap();
                            self.out.push(Out::Reply(clean(&text)));
                            self.streams.insert(id, (self.out.len() - 1, text));
                        }
                    },
                    Said::Reply(text) => match self.streams.remove(&id) {
                        Some((at, _)) if at < self.out.len() => {
                            self.out[at] = Out::Reply(clean(&text));
                            self.fitted.truncate(at);
                            self.gap();
                        }
                        _ => self.reply(&text),
                    },
                    Said::Step(text) => {
                        self.out.push(Out::Step(clean(&text)));
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
                self.streams.remove(&id); // a reply cut off stays as far as it got
                self.ended_at = match self.s.jobs[i].status {
                    Status::Done => Some((true, Instant::now())),
                    Status::Failed(_) => Some((false, Instant::now())),
                    _ => None,
                };
                let lines = self.job_end(&self.s.jobs[i]);
                self.out.extend(lines.into_iter().map(|l| Out::Line(l, 2)));
                self.ended[i] = true;
                self.last = 0;
                ended.push(id);
            }
        }
        for id in ended {
            self.after_end(id);
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

    /// A line for `agent` in this folder, continuing their conversation: now, or next while a turn runs here (as it
    /// was typed, `line`, it shows when it starts). An agent that works only in its own copy takes it there.
    fn talk(&mut self, agent: Agent, task: String, line: String) -> Result<Option<String>, String> {
        if !agent.spec().read {
            let expanded = self.s.expand(&task)?;
            let id = self.s.launch_agent(agent, true, &expanded)?;
            return Ok(Some(format!("  #{id}: {} works only in its own copy; /apply {id} keeps its change", agent.name())));
        }
        if self.must_wait() {
            self.queue.push_back((agent, task, line));
            return Ok(None);
        }
        self.turn(agent, &task)
    }

    /// Whether a turn in this folder has to wait: one runs here, lines wait before it, every job slot is taken, or an
    /// agent update holds the session.
    fn must_wait(&self) -> bool {
        self.s.busy().is_some() || !self.queue.is_empty() || self.s.running() >= self.s.max_jobs || self.s.hold.is_some()
            || self.checking.is_some()
    }

    /// Starts the oldest waiting line once the folder is free; true when it did.
    fn dispatch(&mut self) -> bool {
        let free = self.s.busy().is_none() && self.s.running() < self.s.max_jobs && self.s.hold.is_none()
            && self.checking.is_none();
        let Some((agent, task, line)) = free.then(|| self.queue.pop_front()).flatten() else { return false };
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

    /// `/usage`: this session's tokens, the last day's and week's from Ringo's record, and each plan's limits, asked
    /// of the agents in the background (about a second; no model call).
    fn usage(&mut self) -> Option<String> {
        let mut by: std::collections::BTreeMap<String, u64> = Default::default();
        for job in &self.s.jobs {
            let who = match job.kind { Kind::Agent { agent, .. } => agent.name().to_string(), Kind::Ringo { .. } => "ringo".into() };
            *by.entry(who).or_default() += job.tokens_so_far();
        }
        let parts = |by: &std::collections::BTreeMap<String, u64>| by.iter().filter(|(_, n)| **n > 0)
            .map(|(who, n)| format!("{who} {}", fmt_tokens(*n))).collect::<Vec<_>>().join(", ");
        let total = |by: &std::collections::BTreeMap<String, u64>| fmt_tokens(by.values().sum());
        self.say(format!("  this session: {} tokens{}", total(&by),
                         if by.values().any(|n| *n > 0) { format!(" ({})", parts(&by)) } else { String::new() }));
        let (day, first) = self.s.used_since(86_400);
        let (week, _) = self.s.used_since(7 * 86_400);
        let sums = |m: std::collections::BTreeMap<String, ringo_cli::parse::Tokens>| m.into_iter().map(|(k, t)| (k, t.total())).collect();
        let (day, week): (std::collections::BTreeMap<String, u64>, std::collections::BTreeMap<String, u64>) = (sums(day), sums(week));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let kept = match first {
            Some(at) if now.saturating_sub(at) < 7 * 86_400 => format!(" (Ringo has kept this record for {})", fmt_until(now - at)),
            Some(_) => String::new(),
            None => " (Ringo keeps this record from now on)".into(),
        };
        self.say(format!("  last 24 hours: {} tokens · last 7 days: {}{kept}", total(&day), total(&week)));
        let asked: Vec<Agent> = self.agents.iter().copied()
            .filter(|a| matches!(a.spec().adapter, ringo_cli::Adapter::Claude | ringo_cli::Adapter::Codex) && self.s.installed(*a))
            .collect();
        if asked.is_empty() {
            return None;
        }
        for agent in &asked {
            let (agent, program) = (*agent, self.s.program(*agent));
            self.plans.push(std::thread::spawn(move || (agent, ringo_cli::plan(agent, &program))));
        }
        Some(format!("  asking {} for your plan's limits…", asked.iter().map(|a| a.name()).collect::<Vec<_>>().join(" and ")))
    }

    /// The plan checks that have answered, as lines.
    fn plans_done(&mut self) -> bool {
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.plans).into_iter().partition(|h| h.is_finished());
        self.plans = running;
        for handle in &done.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<_>>() {
            let (agent, result) = handle;
            let line = match result {
                Ok(plan) => plan_line(agent.name(), plan),
                // Claude's own answer is experimental: a turn's last rate-limit event, when there is one, stands in
                Err(e) => match self.s.claude_limits.as_ref().filter(|_| agent.spec().adapter == ringo_cli::Adapter::Claude) {
                    Some(info) => format!("{} (from this session's last claude turn; its own answer failed: {e})",
                                          plan_line(agent.name(), &ringo_cli::claude_plan_from_event(info))),
                    None => format!("  {}: {e}", agent.name()),
                },
            };
            self.say(line);
        }
        self.plans.is_empty()
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
    /// the input, to send or change.
    fn stop(&mut self, id: usize) {
        let _ = self.s.kill(id);
        let queued: Vec<String> = self.queue.drain(..).map(|(_, _, line)| line).collect();
        if !queued.is_empty() && self.input.is_empty() {
            self.input = queued.join("\n").chars().collect();
            self.cursor = self.input.len();
        }
        match self.checking {
            Some((review, turn, _)) if review == id => self.say(format!("  stopping review #{id}; #{turn}'s change stays as it is")),
            _ => self.say(format!("  stopping #{id}; what it changed so far stays (/undo takes it back)")),
        }
    }

    /// Whether a line would wait in the queue: a line for an agent in this folder while a turn runs here.
    fn waits(&self, cmd: &Result<Cmd, String>) -> bool {
        let here = match cmd {
            Ok(Cmd::Talk(_) | Cmd::Compact) => self.main.spec().read,
            Ok(Cmd::Agents(agents, _)) => matches!(agents[..], [(a, false)] if a.spec().read),
            _ => false,
        };
        here && self.must_wait()
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
        let cmd = parse(&line, &self.agents.clone());
        if !self.waits(&cmd) {
            self.echo(&line); // a queued line shows when it starts
        }
        match cmd.and_then(|cmd| self.run(cmd, &line)) {
            Ok(Some(text)) => self.say(text),
            Ok(None) => {}
            Err(e) => self.say(format!("  {e}")),
        }
    }

    /// Runs a command (`line`: as typed); Some(text) goes to the conversation. A started job's own lines follow
    /// there as they come.
    fn run(&mut self, cmd: Cmd, line: &str) -> Result<Option<String>, String> {
        Ok(match cmd {
            Cmd::Talk(text) => return self.talk(self.main, text, line.to_string()),
            Cmd::Help => {
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
                Some("  a new conversation: each agent's next turn starts fresh".into())
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
            Cmd::Tier(None) => Some(format!("  tier: {} (also: {})", self.s.tier.name(),
                Tier::ALL.iter().filter(|t| **t != self.s.tier).map(|t| t.name()).collect::<Vec<_>>().join(", "))),
            Cmd::Tier(Some(tier)) => Some(self.set_tier(tier)),
            Cmd::Usage => self.usage(),
            Cmd::Mode(None) => Some(format!("  mode: {} (Shift-Tab cycles ask, auto, read)", self.s.mode.name())),
            Cmd::Mode(Some(mode)) => Some(self.set_mode(mode)),
            Cmd::Auto => {
                let mode = if self.s.mode == Mode::Auto { Mode::Ask } else { Mode::Auto };
                Some(self.set_mode(mode))
            }
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
                let wt = job.worktree.as_ref().map_or(String::new(), |w| format!("; its copy: {}", w.display()));
                Some(format!("#{id} {who} · {}: its last {} log lines (the whole stream is in {}{wt})\n{}", job.title_full,
                             log.len(), job.dir.display(), log.join("\n")))
            }
            Cmd::Kill(id) => {
                if self.s.busy() == Some(id) {
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
            KeyCode::PageUp => self.scroll += page,
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

    /// The wheel scrolls the conversation; a drag over it selects its text and copies it when the button comes up.
    fn mouse(&mut self, m: MouseEvent) -> bool {
        let v = self.view;
        let row = m.row.clamp(v.y, (v.y + v.height).saturating_sub(1));
        let spot = (self.top + (row - v.y) as usize, m.column.saturating_sub(v.x) as usize);
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll += 3,
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

    /// The selection, first spot first.
    fn selection(&self) -> Option<(Spot, Spot)> {
        let (a, b) = self.sel?;
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
            self.streams.retain(|_, (at, _)| at.checked_sub(extra).map(|n| *at = n).is_some());
        }
        let same = width == self.fitted_w;
        if !same {
            self.fitted.clear();
            self.fitted_w = width;
        }
        while self.fitted.len() < self.out.len() {
            let rows = self.out[self.fitted.len()].rows(width, &self.theme);
            self.fitted.push(rows);
        }
        let rows = self.fitted.iter().map(Vec::len).sum();
        if same && self.scroll > 0 {
            self.scroll += rows - self.rows.min(rows); // rows added below keep what is on screen still
        }
        self.rows = rows;
        rows
    }

    /// The conversation's rows that fit `area`, scrolled as set, with the selection shown.
    fn conversation(&mut self, f: &mut Frame, area: Rect) {
        let total = self.fit_to(area.width as usize);
        let h = area.height as usize;
        self.scroll = self.scroll.min(total.saturating_sub(h));
        let top = total.saturating_sub(h + self.scroll);
        let shown: Vec<Line<'static>> = self.fitted.iter().flatten().skip(top).take(h).cloned().collect();
        f.render_widget(Paragraph::new(shown), area);
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
            queue = self.queue.iter().take(3).map(|(_, _, line)| {
                let text = one_row(&clean(line).replace('\n', " "), w.saturating_sub(12));
                Line::from(vec![Span::styled("   ↳ next: ", t.dim), Span::styled(text, t.selected)])
            }).collect();
            if self.queue.len() > 3 {
                queue.push(Line::styled(format!("   … {} more queued", self.queue.len() - 3), t.dim));
            }
        }
        let ask = self.prompt_lines(w.saturating_sub(2));
        let ask_h = ask.as_ref().map_or(0, |(_, lines)| (lines.len() as u16 + 1).clamp(2, (height / 2).max(2)));
        let typing = ask.is_none() || !self.input.is_empty();
        let (rows, at) = input_rows(&self.input, self.cursor, w.saturating_sub(3)); // " ❯ "
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
        Live { panel, jobs, queue, ask, ask_h, typing, rows, at, top, shown, menu }
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
            for (_, _, line) in &self.queue {
                out.push(Line::styled(fit(&format!("↳ {}", clean(line).replace('\n', " "))), t.selected));
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
        // a tier other than standard is the last to go: it changes what each turn costs
        let right = [format!("{mode} mode · {tier}{tokens} tokens "), format!("{mode} · {tier}{tokens} "),
                     format!("{} ", tier.trim_end_matches(['·', ' '])), String::new()].into_iter()
            .filter(|r| r.is_empty() || !r.trim().is_empty())
            .find(|r| 10 + 8 + cols(r) <= w).unwrap_or_default(); // a narrow head keeps the folder's start
        let left = one_row(&self.label, w.saturating_sub(12 + cols(&right)));
        let pad = w.saturating_sub(10 + cols(&left) + cols(&right));
        f.render_widget(Paragraph::new(Line::from(vec![Span::raw(" ▲▲ "), Span::styled("ringo", t.brand),
            Span::raw("  "), Span::raw(left), Span::raw(" ".repeat(pad)), Span::raw(right)])).style(t.header), head);
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
        self.conversation(f, view);
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
                _ if self.scroll > 0 => "scrolled up · PgDn goes back".into(),
                _ if self.s.busy().is_some() => "Esc stops the turn".into(),
                _ => "⇧⇥ mode · ? keys".into(),
            };
            let mut rows: Vec<Line> = if self.input.is_empty() {
                let ask = match self.s.busy() {
                    Some(_) => "type what comes next; it waits for this turn".to_string(),
                    None => "Tell Ringo what you want to do".to_string(),
                };
                vec![Line::from(vec![Span::styled(" ❯ ", t.focus), Span::styled(ask, t.dim)])]
            } else {
                live.rows.iter().enumerate().skip(live.top).take(live.shown).map(|(i, r)| {
                    Line::from(vec![Span::styled(if i == 0 { " ❯ " } else { "   " }, t.focus), Span::raw(r.clone())])
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
                    *first = Line::from(vec![Span::styled(" ❯ ", t.focus), Span::styled(hint, t.dim)]);
                }
            }
            f.render_widget(Paragraph::new(rows), input);
            if live.ask.is_none() || !self.input.is_empty() {
                f.set_cursor_position((input.x + 3 + live.at.1 as u16, input.y + (live.at.0 - live.top) as u16));
            }
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
        let text: Vec<String> = self.out.iter().flat_map(|o| o.rows(100, &self.theme))
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string()).collect();
        let path = self.s.dir.join("conversation.txt");
        std::fs::write(&path, text.join("\n") + "\n").ok().map(|_| path)
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
    let name = session.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let label = match &session.branch { Some(b) => format!("{name} ({b})"), None => name };
    let saved = crate::theme::settings_all(); // read once, for the theme, the mode and auto updates
    let mut app = App::new(session, agents, label, Theme::load_from(&saved), load_history());
    if let Some(mode) = saved.get("mode").and_then(|v| v.as_str().and_then(Mode::from_name)) {
        app.s.mode = mode;
    }
    app.welcome();
    let (notes, news) = std::sync::mpsc::channel();
    let (agents, state) = (app.agents.clone(), ringo_cli::state_root().join("engine-versions.json"));
    let auto = saved.get("auto_update").and_then(|v| v.as_bool()).unwrap_or(false);
    let check = notes.clone();
    std::thread::spawn(move || {
        let _ = check.send((false, crate::updates::check(&agents, &state)));
    });

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
    let (mut dirty, mut tick) = (true, Instant::now());
    while !app.quit {
        if dirty {
            if terminal.draw(|f| app.draw(f)).is_err() {
                break;
            }
            dirty = false;
        }
        // Job events arrive on a channel, not as terminal input, so the poll timeout is how late one is shown: 25 ms
        // while jobs run (a pump is a few microseconds; redraws still happen only when something changed). Every event
        // already queued is taken before the next redraw.
        let wait = if app.s.running() > 0 || app.toast.is_some() || !app.plans.is_empty() { 25 } else { 250 };
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
                if app.quit || app.setup || app.update || !event::poll(Duration::ZERO).unwrap_or(false) {
                    break;
                }
            }
        }
        if app.setup {
            close(&mut terminal); // the agents' own sign-in runs in the plain terminal
            app.agents = crate::setup::run();
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
        dirty |= app.quota_done();
        // A job that just ended is seen before any waiting line starts: ultra's review holds the folder from there.
        app.feed();
        while let Some((ask, route)) = app.s.routed.pop_front() {
            app.start_routed(ask, &route);
            dirty = true;
        }
        dirty |= app.dispatch();
        if !app.plans.is_empty() && app.plans.iter().any(|h| h.is_finished()) {
            app.plans_done();
            dirty = true;
        }
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
            let (lines, left) = crate::updates::apply_here(app.pending.take().unwrap_or_default());
            match open() {
                Ok(t) => terminal = t,
                Err(_) => break,
            }
            lines.iter().for_each(|line| app.say(line));
            app.pending = Some(left);
            app.update = false;
            dirty = true;
        }
        app.feed();
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
    let saved = app.save().map_or(String::new(), |p| format!("; the conversation is saved in {}", p.display()));
    println!("ringo: {} tokens this session{saved}", fmt_tokens(app.s.tokens()));
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
        assert_eq!(parse("@codex why is it slow?", Codex), Ok(Cmd::Agents(vec![(Codex, false)], "why is it slow?".into())));
        assert_eq!(parse("@claude! fix it", Codex), Ok(Cmd::Agents(vec![(Claude, true)], "fix it".into())));
        assert_eq!(parse("@codex @claude! do x", Grok), Ok(Cmd::Agents(vec![(Codex, false), (Claude, true)], "do x".into())));
        assert_eq!(parse("@all compare", Codex),
                   Ok(Cmd::Agents(vec![(Codex, true), (Claude, true), (Grok, true)], "compare".into())),
                   "every agent, each in its own copy");
        assert_eq!(parse("@all! compare", Codex), parse("@all compare", Codex));
        assert!(parse("@nobody x", Codex).unwrap_err().contains("agy"));
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
        assert_eq!(rows[2], " Welcome to Ringo");
        let words = rows.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(words.contains("/undo takes back its last change"), "outside git too, in Ringo's own store: {rows:#?}");
        assert!(rows[23].starts_with(" ❯ Tell Ringo what you want to do") && rows[23].ends_with("⇧⇥ mode · ? keys"),
                "{rows:#?}");
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
        a.s.jobs[0].fresh = vec![Said::Partial("Hel".into()), Said::Partial("lo".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello"], "the pieces grow one reply");
        screen(&mut a, 60, 12);
        a.s.jobs[0].fresh = vec![Said::Partial(", wor".into())];
        a.feed();
        assert!(screen(&mut a, 60, 12).iter().any(|r| r.trim() == "Hello, wor"), "and it shows as it grows");
        a.s.jobs[0].fresh = vec![Said::Partial("ld\r".into()), Said::Partial("\nmore".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, world\nmore"], "a line end split across pieces stays one");
        a.s.jobs[0].fresh = vec![Said::Reply("Hello, world.".into()), Said::Step("Read a.rs · 2 lines".into()),
                                 Said::Partial("Next".into())];
        a.feed();
        assert_eq!(replies(&a), ["Hello, world.", "Next"], "the whole reply replaces its pieces; the next block opens anew");
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
                assert!(a.must_wait(), "a line sent during the review waits for it");
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
        assert!(said.contains("this session: 0 tokens") && said.contains("last 24 hours"), "{said}");
        assert!(plan_line("codex", &ringo_cli::Plan { name: "plus".into(), windows: vec![ringo_cli::Window {
            name: "week".into(), percent: 34.0, resets: None }], notes: vec!["2 full resets left".into()] })
            .ends_with("codex (plus plan): week 34% used · 2 full resets left"));
        assert_eq!((fmt_until(90), fmt_until(3 * 3600 + 120), fmt_until(2 * 86_400 + 7200)), ("2 min".into(), "3 h 2 min".into(), "2 d 2 h".into()));
    }

    #[test]
    fn a_line_waits_while_it_cannot_start() {
        let mut a = app(&["claude"]);
        a.s.max_jobs = 0; // every job slot taken
        assert_eq!(a.talk(a.main, "fix it".into(), "fix it".into()), Ok(None));
        assert_eq!(a.queue.len(), 1, "it waits instead of being refused");
        assert!(a.waits(&Ok(Cmd::Talk("and this".into()))), "and the next line shows when it starts, not now");
        let rows = screen(&mut a, 60, 12);
        assert!(rows.iter().any(|r| r.contains("↳ next: fix it")), "{rows:#?}");
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
        a.sel = Some(((a.top + 4, 2), (a.top + 5, 5)));
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
        assert_eq!(to("/re"), ["/review ", "/review ", "/research "]);
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
}

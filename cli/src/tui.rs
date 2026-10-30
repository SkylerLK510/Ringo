//! The `ringo` shell: a live panel of jobs and their subagents, the selected job's log, and one input line.
//! It redraws only when something changed (an event, a key) or once a second while jobs run.

use ratatui::crossterm::event::{self, Event as Term, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use crate::theme::Theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Frame;
use ringo_cli::{Agent, Kind, Session, Status};
use std::collections::VecDeque;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const HELP: &str = "\
Cheap by default. A line without / or @ is a question: one tool-free call, with your last few questions and
answers as context. It costs a few thousand tokens, and it cannot see your files (pipelines backend: {backend}).
Ringo's pipelines run the models as tool-free workers with executable checks and one review:
  /review #N           Ringo reviews job N's change (task text as the change description)
  /review BASE [HEAD]  Ringo reviews BASE...HEAD of this repository
  /design TEXT         Ringo's one-call design pipeline on TEXT
  /research TOPIC      Ringo's research: one live web search call, every quoted source fetched and checked in code,
                       one write call that cites only what was verified (about 70k tokens, mostly search results)
  /work SPEC.json      Ringo's checked work loop on a task spec
  /backend SPEC        backend for those pipelines (now: {backend})
Agents (full agent turns: about 17,000 tokens before they start, far more when they search your files; the panel
counts every one), for questions about your code and for changes:
  @codex TASK          ask an agent; it can read the repository but not change it (conversation continues)
  @codex! TASK         let it edit, in a private worktree from HEAD; nothing lands until you /apply
  @codex @claude TASK  the same task to several agents at once; @all means codex and claude
  {#N} in a TASK       hands job N's reply (and change) to the agent, shown before it is sent
  /setup               add another agent and sign in to it
  /route on|off        suggest a command for a line that reads like a design, review or edit task, for you to
                       send (Enter), change, or ask as a plain question (Esc); no model call (now: {route})
  /theme [NAME]        colours: siamese, siamese-light, pink, midnight or mono (now: {theme})
Jobs: /show N  /diff N  /apply N  /drop N (remove its worktree)  /kill N  /quit
Keys: Tab / Shift-Tab select a job, PgUp / PgDn scroll, Up / Down history, Esc clear, Ctrl-C twice quits.";

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
            "help" | "?" => Ok(Cmd::Help),
            "quit" | "exit" | "q" => Ok(Cmd::Quit),
            "setup" => Ok(Cmd::Setup),
            "theme" => Ok(Cmd::Theme(words.next().map(str::to_string))),
            "route" => match words.next() {
                Some("on") => Ok(Cmd::Route(true)),
                Some("off") => Ok(Cmd::Route(false)),
                _ => Err("/route on or /route off".into()),
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
            "all" => agents.extend(all.iter().filter(|a| write || **a != Agent::Grok).map(|a| (*a, write))),
            _ => agents.push((Agent::from_name(name).ok_or(format!("no agent {name}: codex, claude, grok or all"))?, write)),
        }
        rest = rest.trim_start()[word.len()..].trim_start();
    }
    if agents.is_empty() {
        return if rest.is_empty() { Err("type a question, or /help".into()) } else { Ok(Cmd::Ask(rest.to_string())) };
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

struct App {
    s: Session,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist: usize,
    selected: usize, // 0 is the console, otherwise a job id
    scroll: usize,   // lines up from the bottom of the log
    log_height: usize,
    console: VecDeque<String>,
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
}

/// The running mark: a braille spinner, one frame per SPIN_MS while anything runs (the only redraws then).
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPIN_MS: u128 = 120;

impl App {
    fn say(&mut self, text: impl AsRef<str>) {
        for line in text.as_ref().lines() {
            if self.console.len() == 2000 {
                self.console.pop_front();
            }
            self.console.push_back(line.to_string());
        }
    }

    fn help_text(&self) -> String {
        HELP.replace("{backend}", &self.s.backend).replace("{theme}", self.theme.name)
            .replace("{route}", if self.route { "on" } else { "off" })
    }

    fn help(&mut self) {
        let text = self.help_text();
        self.say(text);
    }

    fn agent_style(&self, agent: Agent) -> Style {
        match agent {
            Agent::Codex => self.theme.codex,
            Agent::Claude => self.theme.claude,
            Agent::Grok => self.theme.grok,
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
            let agent = self.agents.iter().find(|a| **a != Agent::Grok).copied().unwrap_or(self.agents[0]);
            let route = crate::route::classify(&line, self.s.git);
            if let Some((cmd, cost)) = crate::route::suggestion(&route, line.trim(), agent.name()) {
                self.say(format!("  → suggested: {cmd}\n    ({cost}). Enter sends it; change it if you like; Esc asks it \
                                  as a plain question instead (about 1-2k tokens)."));
                self.input = cmd.chars().collect();
                self.cursor = self.input.len();
                self.suggested = Some(line);
                self.selected = 0;
                return;
            }
        }
        self.history.push(line.clone());
        self.hist = self.history.len();
        self.say(format!("› {line}"));
        match parse(&line, &self.agents.clone()).and_then(|cmd| self.run(cmd)) {
            Ok(Some(text)) => {
                self.say(text);
                self.selected = 0;
            }
            Ok(None) => {}
            Err(e) => {
                self.say(format!("  {e}"));
                self.selected = 0;
            }
        }
        self.scroll = 0;
    }

    /// Runs a command; Some(text) goes to the console.
    fn run(&mut self, cmd: Cmd) -> Result<Option<String>, String> {
        let started = |id: usize, app: &mut App| {
            app.selected = id;
            None
        };
        Ok(match cmd {
            Cmd::Help => Some(self.help_text()),
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
                self.s.job(id)?;
                started(id, self)
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
                let id = self.s.review_job(of)?;
                started(id, self)
            }
            Cmd::ReviewRepo(base, head) => {
                let id = self.s.review_repo(&base, &head)?;
                started(id, self)
            }
            Cmd::Design(text) => {
                let brief = self.s.expand(&text)?;
                let id = self.s.design(&brief)?;
                started(id, self)
            }
            Cmd::Ask(question) => {
                let question = self.s.expand(&question)?;
                let id = self.s.ask(&question)?;
                started(id, self)
            }
            Cmd::Research(topic) => {
                let topic = self.s.expand(&topic)?;
                let id = self.s.research(&topic)?;
                started(id, self)
            }
            Cmd::Work(spec) => {
                let id = self.s.work(Path::new(&spec))?;
                started(id, self)
            }
            Cmd::Agents(agents, prompt) => {
                let expanded = self.s.expand(&prompt)?;
                let mut notes = Vec::new();
                if expanded.len() != prompt.len() {
                    notes.push(format!("  handoff adds {} characters to the task:", expanded.len() - prompt.len()));
                    notes.push(expanded.clone());
                }
                for (agent, write) in agents {
                    match self.s.launch_agent(agent, write, &expanded) {
                        Ok(id) => self.selected = id,
                        Err(e) => notes.push(format!("  {}: {e}", agent.name())),
                    }
                }
                if notes.is_empty() { None } else { Some(notes.join("\n")) }
            }
        })
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if !(ctrl && matches!(k.code, KeyCode::Char('c' | 'd'))) {
            self.quit_armed = false;
        }
        match k.code {
            KeyCode::Char('c' | 'd') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else if self.s.running() > 0 && !self.quit_armed {
                    self.quit_armed = true;
                    self.selected = 0;
                    self.say(format!("  {} jobs are running; Ctrl-C again stops them and quits", self.s.running()));
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
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
            }
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
            KeyCode::Up if self.hist > 0 => {
                self.hist -= 1;
                self.input = self.history[self.hist].chars().collect();
                self.cursor = self.input.len();
            }
            KeyCode::Down if self.hist < self.history.len() => {
                self.hist += 1;
                self.input = self.history.get(self.hist).map(|h| h.chars().collect()).unwrap_or_default();
                self.cursor = self.input.len();
            }
            KeyCode::Tab => {
                self.selected = (self.selected + 1) % (self.s.jobs.len() + 1);
                self.scroll = 0;
            }
            KeyCode::BackTab => {
                self.selected = (self.selected + self.s.jobs.len()) % (self.s.jobs.len() + 1);
                self.scroll = 0;
            }
            KeyCode::PageUp => self.scroll += self.log_height.max(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.log_height.max(1)),
            KeyCode::Esc if self.suggested.is_some() => {
                // a suggestion declined: ask the original line as a plain question instead
                let line = self.suggested.take().unwrap_or_default();
                self.input = line.chars().collect();
                self.suggested = Some(String::new()); // so this submit is not routed again
                self.submit();
            }
            KeyCode::Esc => {
                if self.input.is_empty() {
                    self.selected = 0;
                }
                self.input.clear();
                self.cursor = 0;
            }
            _ => {}
        }
    }

    /// What the panel shows for one job: its mark, who runs it, what it is doing, any same-files warning, and its
    /// time, tokens and Ringo model calls.
    fn job_parts(&self, job: &ringo_cli::Job) -> JobParts {
        let t = &self.theme;
        let (mark, mark_style) = match &job.status {
            Status::Running => (self.spinner(), t.running),
            Status::Done => ("✓", t.ok),
            Status::Failed(_) => ("✗", t.fail),
            Status::Cancelled => ("■", t.cancelled),
        };
        let (who, who_style) = match &job.kind {
            Kind::Agent { agent, write } => (format!("{}{}", agent.name(), if *write { "!" } else { "" }), self.agent_style(*agent)),
            Kind::Ringo { command } => (format!("ringo {command}"), t.ringo),
        };
        let mut detail = job.activity.clone();
        if job.worktree.is_some() && job.status == Status::Done {
            detail = format!("{} {detail}", if job.applied { "[applied]" } else { "[change ready]" });
        }
        let overlaps: Vec<String> = self.s.overlaps(job.id).into_iter()
            .map(|(other, files)| format!("#{other} {}", files.join(", "))).collect();
        let warning = if overlaps.is_empty() { String::new() } else { format!("⚠ same files as {}", overlaps.join("; ")) };
        let calls = if job.calls > 0 { format!(" {}c", job.calls) } else { String::new() };
        let stats = format!("{} {}{calls}", fmt_age(job.age()), fmt_tokens(job.tokens.total()));
        JobParts { mark, mark_style, who, who_style, detail, warning, stats }
    }

    fn sub_mark(&self, ended: Option<bool>) -> (&'static str, Style) {
        match ended {
            None => (self.spinner(), self.theme.running),
            Some(true) => ("✓", self.theme.ok),
            Some(false) => ("✗", self.theme.fail),
        }
    }

    /// The stacked panel: one row per job across the full width, for terminals too narrow for the side column.
    fn panel_lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let mut lines = Vec::new();
        for job in &self.s.jobs {
            let JobParts { mark, mark_style, who, who_style, mut detail, warning, stats } = self.job_parts(job);
            if !warning.is_empty() {
                detail = format!(" · {detail}");
            }
            let id = format!("#{:<3}", job.id);
            let who = format!("{who:<13}");
            let (age_tokens, calls) = stats.split_once(' ').map_or((stats.as_str(), ""), |(a, rest)| (a, rest));
            let stats = format!(" {:>6} {:>10}  ", age_tokens, calls);
            let room = width.saturating_sub(id.chars().count() + who.chars().count() + stats.chars().count() + 2);
            let warn_room = warning.chars().count().min(room);
            let row = if self.selected == job.id { t.selected } else { Style::new() };
            lines.push(Line::from(vec![
                Span::styled(id, t.dim.patch(row)),
                Span::styled(who, who_style.patch(row)),
                Span::styled(mark.to_string(), mark_style.patch(row)),
                Span::styled(stats, t.dim.patch(row)),
                Span::styled(ringo_cli::parse::short(&warning, warn_room), t.warn),
                Span::raw(ringo_cli::parse::short(&detail, room - warn_room)),
            ]).style(row));
            for (i, sub) in job.subs.iter().enumerate() {
                let branch = if i + 1 == job.subs.len() { "└" } else { "├" };
                let (mark, mark_style) = self.sub_mark(sub.ended);
                let label = format!("     {branch} {:<34} ", ringo_cli::parse::short(&sub.label, 34));
                let room = width.saturating_sub(label.chars().count() + 4);
                lines.push(Line::from(vec![
                    Span::styled(label, t.dim),
                    Span::styled(mark.to_string(), mark_style),
                    Span::raw(format!(" {}", ringo_cli::parse::short(&sub.activity, room))),
                ]));
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled("no jobs yet: /help", t.dim));
        }
        lines
    }

    /// The side column: each job as a short card (who, mark, time and tokens; then what it is doing, any
    /// same-files warning, and one line per subagent), so it reads in a narrow column.
    fn side_lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let short = |text: &str, room: usize| ringo_cli::parse::short(text, room);
        let mut lines = Vec::new();
        for job in &self.s.jobs {
            let JobParts { mark, mark_style, who, who_style, detail, warning, stats } = self.job_parts(job);
            let row = if self.selected == job.id { t.selected } else { Style::new() };
            let id = format!("#{} ", job.id);
            let room = width.saturating_sub(id.chars().count() + stats.chars().count() + 3);
            let who = format!("{:<room$}", short(&who, room));
            lines.push(Line::from(vec![
                Span::styled(id, t.dim.patch(row)),
                Span::styled(who, who_style.patch(row)),
                Span::styled(format!(" {mark} "), mark_style.patch(row)),
                Span::styled(stats, t.dim.patch(row)),
            ]).style(row));
            let inner = width.saturating_sub(3);
            if !detail.is_empty() {
                lines.push(Line::from(Span::raw(format!("   {}", short(&detail, inner)))).style(row));
            }
            if !warning.is_empty() {
                lines.push(Line::from(Span::styled(format!("   {}", short(&warning, inner)), t.warn)));
            }
            for (i, sub) in job.subs.iter().enumerate() {
                let branch = if i + 1 == job.subs.len() { "└" } else { "├" };
                let (mark, mark_style) = self.sub_mark(sub.ended);
                let room = width.saturating_sub(7);
                let text = if sub.activity.is_empty() { sub.label.clone() } else { format!("{}: {}", sub.label, sub.activity) };
                lines.push(Line::from(vec![
                    Span::styled(format!("   {branch} "), t.dim),
                    Span::styled(format!("{mark} "), mark_style),
                    Span::styled(short(&text, room), t.dim),
                ]));
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled("no jobs yet: /help", t.dim));
        }
        lines
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let t = self.theme;
        let [top, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
        let header = Line::from(vec![
            Span::styled(" ringo ", t.brand),
            Span::styled(format!(" {}  ·  {} running  ·  {} tokens this session  ·  pipelines: {}", self.label,
                                 self.s.running(), fmt_tokens(self.s.tokens()), self.s.backend), t.header),
        ]);
        f.render_widget(Paragraph::new(header).style(t.header), top);
        let block = |title: String, focused: bool| Block::bordered().border_type(BorderType::Rounded)
            .border_style(if focused { t.focus } else { t.border }).title(Span::styled(title, t.title));

        // Wide terminals: the agents in a column on the left, the chat beside it. Narrow ones: stacked.
        let (log, input) = if area.width >= SIDE_MIN_WIDTH {
            let side_w = (area.width * 32 / 100).clamp(34, 52);
            let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Min(40)]).areas(body);
            let panel = self.side_lines(side.width.saturating_sub(2) as usize);
            let skip = panel.len().saturating_sub(side.height.saturating_sub(2) as usize); // newest jobs stay visible
            f.render_widget(Paragraph::new(panel[skip..].to_vec()).block(block(" agents ".into(), false)), side);
            let [log, input] = Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(main);
            (log, input)
        } else {
            let panel = self.panel_lines(area.width.saturating_sub(2) as usize);
            let panel_h = (panel.len() as u16 + 2).clamp(3, (body.height / 2).max(3));
            let [jobs, log, input] = Layout::vertical([Constraint::Length(panel_h), Constraint::Min(3),
                Constraint::Length(3)]).areas(body);
            let skip = panel.len().saturating_sub(panel_h as usize - 2);
            f.render_widget(Paragraph::new(panel[skip..].to_vec()).block(block(" agents ".into(), false)), jobs);
            (log, input)
        };

        let width = log.width.saturating_sub(2) as usize;
        let (title, source): (String, Vec<&String>) = match self.s.job(self.selected) {
            Ok(job) if self.selected > 0 => (format!(" #{} {} ", job.id, job.title), job.log.iter().collect()),
            _ => (" console ".into(), self.console.iter().collect()),
        };
        let height = log.height.saturating_sub(2) as usize;
        self.log_height = height;
        let mut rows: Vec<String> = Vec::new(); // only the visible tail is wrapped
        for line in source.iter().rev() {
            let mut wrapped = wrap(line, width);
            wrapped.reverse();
            rows.extend(wrapped);
            if rows.len() >= height + self.scroll {
                break;
            }
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
        let visible: Vec<Line> = rows.iter().skip(self.scroll).take(height).rev().map(|r| Line::raw(r.clone())).collect();
        let title = if title.chars().count() > width { ringo_cli::parse::short(&title, width.saturating_sub(2)) } else { title };
        f.render_widget(Paragraph::new(visible).block(block(title, self.selected > 0)), log);

        let hint = " ask anything · @codex or @claude for an agent · Tab jobs · /help ".to_string();
        let room = input.width.saturating_sub(2).max(1) as usize;
        let start = self.cursor.saturating_sub(room - 1);
        let shown: String = self.input.iter().skip(start).take(room).collect();
        f.render_widget(Paragraph::new(shown).block(block(hint, true)), input);
        f.set_cursor_position((input.x + 1 + (self.cursor - start) as u16, input.y + 1));
    }
}

struct JobParts {
    mark: &'static str,
    mark_style: Style,
    who: String,
    who_style: Style,
    detail: String,
    warning: String,
    stats: String,
}

/// From this width the agents panel is a column beside the chat; below it, a strip above the chat.
const SIDE_MIN_WIDTH: u16 = 90;

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
    let mut app = App { s: session, input: Vec::new(), cursor: 0, history: Vec::new(), hist: 0, selected: 0, scroll: 0,
                        log_height: 0, console: VecDeque::new(), quit_armed: false, quit: false,
                        setup: false, label, agents, theme: Theme::load(), started: Instant::now(),
                        route: crate::theme::setting("route").and_then(|v| v.as_bool()).unwrap_or(false), suggested: None };
    let found: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
    app.say(format!("Ringo shell in {}. Agents: {} (/setup adds another).", app.s.repo.display(), found.join(", ")));
    app.say(format!("Private state: {}", app.s.dir.display()));
    app.help();

    let mut terminal = ratatui::init();
    let (mut dirty, mut tick) = (true, Instant::now());
    while !app.quit {
        if dirty {
            if terminal.draw(|f| app.draw(f)).is_err() {
                break;
            }
            dirty = false;
        }
        let wait = if app.s.running() > 0 { SPIN_MS as u64 } else { 250 };
        if event::poll(Duration::from_millis(wait)).unwrap_or(false) {
            match event::read() {
                Ok(Term::Key(k)) if k.kind == KeyEventKind::Press => {
                    app.key(k);
                    dirty = true;
                }
                Ok(Term::Resize(..)) => dirty = true,
                _ => {}
            }
        }
        if app.setup {
            ratatui::restore(); // the agents' own sign-in runs in the plain terminal
            app.agents = crate::setup::run();
            terminal = ratatui::init();
            app.setup = false;
            dirty = true;
            let names: Vec<&str> = app.agents.iter().map(|a| a.name()).collect();
            app.say(format!("  agents set up: {}", names.join(", ")));
        }
        dirty |= app.s.pump();
        if app.s.running() > 0 && tick.elapsed() >= Duration::from_millis(SPIN_MS as u64) {
            dirty = true; // the spinner and the running clocks
            tick = Instant::now();
        }
    }
    ratatui::restore();
    app.s.shutdown();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routes_and_commands() {
        use Agent::*;
        let all = [Codex, Claude, Grok];
        let parse = |line: &str, _: Agent| super::parse(line, &all);
        assert_eq!(parse("why is it slow?", Codex), Ok(Cmd::Ask("why is it slow?".into())));
        assert!(parse("   ", Codex).is_err());
        assert_eq!(parse("@codex why is it slow?", Codex), Ok(Cmd::Agents(vec![(Codex, false)], "why is it slow?".into())));
        assert_eq!(parse("@claude! fix it", Codex), Ok(Cmd::Agents(vec![(Claude, true)], "fix it".into())));
        assert_eq!(parse("@codex @claude! do x", Grok), Ok(Cmd::Agents(vec![(Codex, false), (Claude, true)], "do x".into())));
        assert_eq!(parse("@all compare", Codex), Ok(Cmd::Agents(vec![(Codex, false), (Claude, false)], "compare".into())));
        assert!(parse("@nobody x", Codex).is_err());
        assert!(parse("@codex", Codex).is_err());
        assert_eq!(parse("/review #3", Codex), Ok(Cmd::ReviewJob(3)));
        assert_eq!(parse("/review main", Codex), Ok(Cmd::ReviewRepo("main".into(), "HEAD".into())));
        assert_eq!(parse("/apply 2", Codex), Ok(Cmd::Apply(2)));
        assert_eq!(parse("/design a cache for X", Codex), Ok(Cmd::Design("a cache for X".into())));
        assert!(parse("/frobnicate", Codex).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_tokens(870), "870");
        assert_eq!(fmt_tokens(12_400), "12.4k");
        assert_eq!(fmt_age(Duration::from_secs(75)), "1:15");
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
    }
}

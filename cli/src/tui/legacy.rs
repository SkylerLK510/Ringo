//! The first restart after upgrading still comes from the old executable: it can save only rendered text.
//! Recover its display, including plain history subsequently carried inside a new structured snapshot. Saved
//! replies supply Markdown only when their complete rendered text matches; unknown text stays readable as-is.

use super::*;
use std::collections::HashMap;

const EXPORT_WIDTH: usize = 100;

fn raw(entry: &Out) -> Option<String> {
    match entry {
        Out::Line(line, _) if line.style == Style::default()
            && line.spans.iter().all(|s| s.style == Style::default()) => Some(line.to_string()),
        _ => None,
    }
}

fn heading(line: &str, t: &Theme) -> Option<Line<'static>> {
    let (who, tail) = line.strip_prefix(' ')?.split_once("  #")?;
    let id = tail.split([' ', '·']).next()?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let name = who.strip_suffix(" (own copy)").unwrap_or(who);
    let style = match name {
        "codex" => t.codex,
        "claude" => t.claude,
        "ringo" => t.ringo,
        _ if name.starts_with("ringo ") => t.ringo,
        _ if ringo_cli::valid_name(name) => t.grok,
        _ => return None,
    };
    Some(Line::from(vec![Span::styled(format!(" {who} "), style.add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                         Span::styled(format!(" #{tail}"), t.dim)]))
}

fn rendered(entry: &Out, t: &Theme) -> Vec<String> {
    entry.rows(EXPORT_WIDTH, t).iter().map(|l| l.to_string().trim_end().to_string()).collect()
}

/// Only undo a wrap when fitting the combined line reproduces every original row at the export width. This
/// keeps deliberate short lines, code indentation, and unrecognized output intact, including wide characters.
fn unwrap(lines: &[String], indent: usize, t: &Theme) -> (String, usize) {
    let mut joined = lines[0].clone();
    let mut count = 1;
    let prefix = " ".repeat(indent);
    while let Some(next) = lines.get(count).filter(|s| s.starts_with(&prefix) && !s.trim().is_empty()) {
        let rest = &next[indent..];
        if rest.starts_with(' ') || (indent == 2 && ["✓ ", "✗ ", "◌ ", "· ", "⚠ ", "↳ "]
            .iter().any(|mark| rest.starts_with(mark))) { break; }
        let combined = [" ", ""].into_iter().map(|sep| format!("{joined}{sep}{rest}"))
            .find(|s| rendered(&Out::Line(Line::raw(s.clone()), indent), t) == lines[..=count]);
        let Some(combined) = combined else { break };
        joined = combined;
        count += 1;
    }
    (joined, count)
}

/// Raw replies from this folder's saved sessions. No event replay or provider request: this only reads reply.md.
/// Index by the first rendered row; longest matches first so a short answer cannot consume a longer one's prefix.
#[derive(Default)]
struct Replies(HashMap<String, Vec<(Vec<String>, String)>>);

impl Replies {
    fn load(path: &Path, repo: &Path, t: &Theme) -> Self {
        let mut replies = Self::default();
        let Some(session) = path.parent() else { return replies };
        let sessions = session.parent().into_iter().flat_map(std::fs::read_dir).flatten().flatten();
        for dir in sessions.filter_map(|e| e.file_type().ok().filter(|k| k.is_dir()).map(|_| e.path())) {
            let same_folder = ["resume.json", "resumed.json"].into_iter().any(|name| {
                std::fs::read(dir.join(name)).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|v| v["folder"].as_str().map(|p| Path::new(p) == repo)).unwrap_or(false)
            });
            if dir != session && !same_folder { continue; }
            for job in std::fs::read_dir(dir.join("jobs")).into_iter().flatten().flatten() {
                if let Ok(text) = std::fs::read_to_string(job.path().join("reply.md")) {
                    let text = clean(&text);
                    let mut rows = rendered(&Out::Reply(text.clone()), t);
                    while rows.last().is_some_and(String::is_empty) { rows.pop(); }
                    if let Some(first) = rows.first().filter(|s| !s.is_empty()) {
                        replies.0.entry(first.clone()).or_default().push((rows, text));
                    }
                }
            }
        }
        for candidates in replies.0.values_mut() {
            candidates.sort_by_key(|(rows, _)| std::cmp::Reverse(rows.len()));
            candidates.dedup();
        }
        replies
    }

    fn matching(&self, lines: &[String]) -> Option<(Out, usize)> {
        self.0.get(&lines[0])?.iter().find(|(rows, _)| lines.starts_with(rows)
            && lines.get(rows.len()).is_none_or(String::is_empty))
            .map(|(rows, text)| (Out::Reply(text.clone()), rows.len()))
    }
}

fn parse(lines: &[String], t: &Theme, replies: &Replies) -> Vec<Out> {
    let (mut out, mut at) = (Vec::new(), 0);
    while at < lines.len() {
        let line = &lines[at];
        if let Some((reply, count)) = replies.matching(&lines[at..]) {
            out.push(reply);
            at += count;
            continue;
        }
        if let Some(head) = heading(line, t) {
            out.push(Out::Line(head, 0));
        } else if line.starts_with(" you  ") {
            let mut first = true;
            loop {
                let (joined, count) = unwrap(&lines[at..], 6, t);
                let mut spans = if first {
                    vec![Span::styled(" you ", t.focus.add_modifier(Modifier::BOLD | Modifier::REVERSED)), Span::raw(" ")]
                } else { vec![Span::raw("      ")] };
                spans.push(Span::styled(joined[6..].to_string(), t.selected));
                out.push(Out::Line(Line::from(spans), 6));
                at += count;
                first = false;
                if !lines.get(at).is_some_and(|s| s.starts_with("      ")) { break; }
            }
            continue;
        } else if let Some((mark, body)) = ["✓", "✗", "◌", "·"].into_iter()
            .find_map(|mark| line.strip_prefix(&format!("  {mark} ")).map(|body| (mark, body))) {
            let marker = if mark == "✓" { String::new() } else { format!("{mark} ") };
            let mut step = format!("{marker}{body}");
            // A result (exit status or edit counts) was right-aligned instead of keeping its ` · ` separator.
            // Recover that separator only when step_row reproduces the complete original export exactly.
            for (gap, _) in body.match_indices("  ") {
                let candidate = format!("{marker}{} · {}", body[..gap].trim_end(), body[gap..].trim_start());
                if rendered(&Out::Step(candidate.clone()), t) == [line.clone()] {
                    step = candidate;
                    break;
                }
            }
            at += 1;
            while let Some(diff) = lines.get(at).and_then(|l| l.strip_prefix("      "))
                .filter(|d| d.starts_with(['+', '-'])) {
                step.push('\n');
                step.push_str(diff);
                at += 1;
            }
            out.push(Out::Step(step));
            continue;
        } else if line.starts_with("── ") {
            let (joined, count) = unwrap(&lines[at..], 2, t);
            let tail = &joined["── ".len()..];
            let (head, tail) = tail.split_once(" · ").unwrap_or((tail, ""));
            let style = match head {
                "✓ done" => t.ok, "✗ failed" => t.fail, "■ stopped" => t.cancelled, _ => t.dim,
            };
            out.push(Out::Line(Line::from(vec![Span::styled("── ", t.border), Span::styled(head.to_string(), style),
                Span::styled(if tail.is_empty() { String::new() } else { format!(" · {tail}") }, t.dim)]), 2));
            at += count;
            continue;
        } else if line.starts_with("  ") && !line.starts_with("   ") {
            let (joined, count) = unwrap(&lines[at..], 2, t);
            let style = if joined.starts_with("  ⚠ ") { t.warn } else { Style::default() };
            out.push(Out::Line(Line::styled(joined, style), 2));
            at += count;
            continue;
        } else {
            out.push(Out::text(line));
        }
        at += 1;
    }
    out
}

pub(super) fn recover(entries: Vec<Out>, path: &Path, repo: &Path, t: &Theme) -> Vec<Out> {
    // Structured history already has the right display. Inspect only stretches of unstyled lines containing an
    // actual speaker header; normal system messages and new structured replies never enter the legacy parser.
    let (mut out, mut entries, mut replies) = (Vec::new(), entries.into_iter().peekable(), None);
    while let Some(entry) = entries.next() {
        let Some(first) = raw(&entry) else { out.push(entry); continue };
        let mut plain = vec![entry];
        let mut lines = vec![first];
        while let Some(line) = entries.peek().and_then(raw) {
            lines.push(line);
            plain.push(entries.next().unwrap());
        }
        if lines.iter().any(|line| heading(line, t).is_some() || line.starts_with(" you  ")) {
            let replies = replies.get_or_insert_with(|| Replies::load(path, repo, t));
            out.extend(parse(&lines, t, replies));
        } else {
            out.extend(plain);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> App {
        let root = std::env::temp_dir().join(format!("ringo-legacy-{}-{name}", std::process::id()));
        let (repo, state) = (root.join("folder"), root.join("state"));
        std::fs::create_dir_all(&repo).unwrap();
        let session = Session::with_state_home(&repo, &state, &root).unwrap();
        App::new(session, vec![Agent::from_name("codex").unwrap()], "folder".into(),
                 Theme::named("midnight", crate::theme::Depth::TrueColor).unwrap(), Vec::new())
    }

    fn rows(app: &App, width: usize) -> Vec<Line<'static>> {
        app.out.iter().flat_map(|o| o.rows(width, &app.theme)).collect()
    }

    fn cells(lines: &[Line<'static>], width: usize) -> ratatui::buffer::Buffer {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, width as u16, lines.len() as u16));
        ratatui::widgets::Widget::render(Paragraph::new(lines.to_vec()), buffer.area, &mut buffer);
        buffer
    }

    fn old_save(app: &App) -> PathBuf {
        let path = app.save().unwrap();
        std::fs::remove_file(path.with_extension("json")).unwrap();
        path
    }

    fn sample(app: &mut App) -> String {
        app.echo("Please keep the speaker badges and wrapping of this conversation, even when I upgrade and restart from the previous executable. 漢字🙂\nAnother deliberate line.\n  indented text");
        app.gap();
        app.out.push(Out::Line(heading(" codex  #3 · gpt-6.1-sol", &app.theme).unwrap(), 0));
        app.out.push(Out::Step("run cargo test".into()));
        app.out.push(Out::Step("✗ run command · exit 8".into()));
        app.out.push(Out::Step("Edit main.rs · +1 −1\n- old code\n+ new code".into()));
        app.out.push(Out::Looked([2, 3, 0]));
        app.gap();
        app.say("  A long commentary paragraph should flow across the available terminal width after restart instead of keeping the hard line breaks from its old one-hundred-column text export.");
        let reply = "**The update is ready.** Keep `code`, [this link](https://example.com), and emphasis.\n\n- Speaker badges and tool results keep their colours.\n- Paragraphs wrap to the new width.\n\n| Check | Result |\n|---|---|\n| Earlier reply | recovered |";
        app.reply(reply);
        app.out.push(Out::Line(Line::from(vec![Span::styled("── ", app.theme.border),
            Span::styled("✓ done", app.theme.ok), Span::styled(" · 2:57 · 777.2k tokens · change ready: /diff 3 shows it, /apply 3 keeps it, with a long explanation that wraps", app.theme.dim)]), 2));
        let job = app.s.dir.join("jobs/3");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("reply.md"), reply).unwrap();
        reply.into()
    }

    #[test]
    fn the_first_restart_from_an_old_executable_recovers_the_display() {
        let mut app = fixture("upgrade");
        let reply = sample(&mut app);
        let before = app.out.clone();
        let path = old_save(&app);
        app.restore_conversation(&path);
        assert!(app.out.iter().any(|o| matches!(o, Out::Reply(s) if s == &reply)));
        for width in [30, 80, 100, 180] {
            let expected: Vec<_> = before.iter().flat_map(|o| o.rows(width, &app.theme)).collect();
            assert_eq!(cells(&rows(&app, width), width), cells(&expected, width),
                       "old executable → new executable at {width} columns");
        }
        // Once migrated, the normal structured sidecar carries the display without needing reply artifacts again.
        let path = app.save().unwrap();
        std::fs::remove_dir_all(app.s.dir.join("jobs")).unwrap();
        app.restore_conversation(&path);
        assert!(app.out.iter().any(|o| matches!(o, Out::Reply(s) if s == &reply)));
        app.theme = Theme::mono();
        app.restore_conversation(&path);
        assert!(rows(&app, 80).iter().all(|l| l.style.fg.is_none() && l.style.bg.is_none()
            && l.spans.iter().all(|s| s.style.fg.is_none() && s.style.bg.is_none())));
    }

    #[test]
    fn plain_history_already_carried_in_a_sidecar_is_recovered_too() {
        let mut app = fixture("carried");
        sample(&mut app);
        let path = old_save(&app);
        // This is what #68 did on its first restart after installation: plain legacy lines, later saved in JSON.
        let text = std::fs::read_to_string(&path).unwrap();
        app.out = text.lines().map(Out::text).collect();
        app.echo("A new message written by the updated executable.");
        let native = app.out.last().unwrap().clone();
        let path = app.save().unwrap();
        app.restore_conversation(&path);
        assert!(app.out.iter().any(|o| matches!(o, Out::Reply(s) if s.starts_with("**The update"))));
        assert_eq!(serde_json::to_value(app.out.last().unwrap()).unwrap(), serde_json::to_value(native).unwrap());
        assert!(rows(&app, 180).iter().any(|l| l.spans.iter().any(|s|
            s.content == " codex " && s.style.add_modifier.contains(Modifier::REVERSED))));
    }

    #[test]
    fn unknown_text_and_nonmatching_replies_are_not_rewritten() {
        let mut app = fixture("unknown");
        let text = " you  show this\n\n codex  #1\n\n  A short deliberate line.\n  Another short line.\n    code  spacing\n    codex  #99\n  ✓ literal text in code stays indented above\n\n  Ready. More than the saved answer.\n\nunknown legacy text\n";
        let path = app.s.dir.join("conversation.txt");
        std::fs::write(&path, text).unwrap();
        let job = app.s.dir.join("jobs/1");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(job.join("reply.md"), "**Ready.**").unwrap();
        app.restore_conversation(&path);
        assert!(!app.out.iter().any(|o| matches!(o, Out::Reply(_))));
        let restored: Vec<_> = rows(&app, 100).iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert_eq!(restored, text.lines().collect::<Vec<_>>());
        let deliberate = rows(&app, 180);
        assert!(deliberate.iter().any(|l| l.to_string() == "  A short deliberate line."));
        assert!(deliberate.iter().any(|l| l.to_string() == "    code  spacing"));
    }

    #[test]
    fn raw_markdown_is_read_only_from_sessions_for_this_folder() {
        let mut app = fixture("siblings");
        let reply = "**Saved Markdown** in an earlier session.";
        let expected = rendered(&Out::Reply(reply.into()), &app.theme).join("\n");
        let path = app.s.dir.join("conversation.txt");
        std::fs::write(&path, format!(" codex  #1\n\n{expected}\n")).unwrap();
        let sibling = app.s.dir.parent().unwrap().join("earlier");
        std::fs::create_dir_all(sibling.join("jobs/1")).unwrap();
        std::fs::write(sibling.join("jobs/1/reply.md"), reply).unwrap();
        let metadata = sibling.join("resumed.json");
        std::fs::write(&metadata, serde_json::json!({"folder": "some-other-folder"}).to_string()).unwrap();
        app.restore_conversation(&path);
        assert!(!app.out.iter().any(|o| matches!(o, Out::Reply(_))));
        std::fs::write(&metadata, serde_json::json!({"folder": app.s.repo}).to_string()).unwrap();
        app.restore_conversation(&path);
        assert!(app.out.iter().any(|o| matches!(o, Out::Reply(s) if s == reply)));
    }

    /// Render the owner's legacy export without starting Ringo or an agent; output stays outside the repository.
    #[test]
    #[ignore]
    fn export_actual_legacy_preview() {
        let path = PathBuf::from(std::env::var_os("RINGO_LEGACY_CONVERSATION").unwrap());
        let dir = PathBuf::from(std::env::var_os("RINGO_TUI_PREVIEW_DIR").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = fixture("preview");
        app.s.repo = PathBuf::from(std::env::var_os("RINGO_LEGACY_FOLDER").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let start = text.rfind(" you  Please merge and install").unwrap_or(0);
        let text = &text[start..];
        app.out = text.lines().map(Out::text).collect();
        app.spoke = true;
        for name in ["before", "recovered"] {
            if name == "recovered" {
                app.out = recover(std::mem::take(&mut app.out), &path, &app.s.repo, &app.theme);
            }
            for width in [100, 180] {
                let cells: Vec<_> = rows(&app, width).iter().map(|line| {
                    let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, width as u16, 1));
                    ratatui::widgets::Widget::render(Paragraph::new(line.clone()), buffer.area, &mut buffer);
                    buffer.content.iter().map(|c| serde_json::json!({"text": c.symbol(), "fg": format!("{:?}", c.fg),
                        "bg": format!("{:?}", c.bg), "bold": c.modifier.contains(Modifier::BOLD),
                        "reverse": c.modifier.contains(Modifier::REVERSED), "dim": c.modifier.contains(Modifier::DIM)})).collect::<Vec<_>>()
                }).collect();
                std::fs::write(dir.join(format!("{name}-{width}.json")), serde_json::to_vec(&cells).unwrap()).unwrap();
            }
        }
    }
}

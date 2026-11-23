//! Claude Code's `@path` imports in the project's CLAUDE.md or AGENTS.md. Ringo's Claude turns run with Claude's own
//! CLAUDE.md loading off (`CLAUDE_CODE_DISABLE_CLAUDE_MDS`, or safe mode) and append the repository's file to the
//! system prompt themselves, so Claude never sees the imports expanded: a CLAUDE.md that is just `@AGENTS.md` (the
//! usual way to share one set of instructions between agents) reached Claude as that literal line. Checked with
//! Claude Code 2.1.330: `claude -p --safe-mode --append-system-prompt-file CLAUDE.md`, CLAUDE.md holding `@AGENTS.md`
//! and AGENTS.md a codeword: Claude answered NONE. This expands them the way Claude Code reads them, within the
//! repository only.
//!
//! An import is `@path` at the start of a line or after whitespace, outside fenced code blocks and inline code spans,
//! and naming an existing file inside the repository (its path read from the importing file's folder, symlinks
//! resolved: a link out of the repository is refused; `~/` paths are never read). A line that is only an import is
//! replaced by the file's text; an import inside a line leaves the line as it is and the file's text goes after the
//! document, under a one-line heading naming it. Imports nest to `MAX_DEPTH`, a file is included once (a cycle, or
//! `CLAUDE.md` importing what it was itself read from, adds nothing), and what is added stops at `MAX_ADDED` bytes.
//! The text is bytes throughout, as the file may not be UTF-8.

use std::path::{Path, PathBuf};

/// How many files deep imports are followed.
pub const MAX_DEPTH: usize = 5;
/// The most bytes imports may add to the document; an import that would pass it is left as written.
pub const MAX_ADDED: usize = 64 * 1024;

pub struct Expanded {
    /// The document with its imports expanded; the file's own bytes when nothing was imported.
    pub text: Vec<u8>,
    /// The files that were imported, in the order they were taken in.
    pub imported: Vec<PathBuf>,
    /// What to say in the job's log: an import left out for its size.
    pub notes: Vec<String>,
}

struct State {
    root: PathBuf,
    included: Vec<PathBuf>,
    added: usize,
    notes: Vec<String>,
}

/// `file`'s text with its imports expanded. `root` is the repository (any folder `file` is in); nothing outside it
/// is read.
pub fn expand(file: &Path, root: &Path) -> std::io::Result<Expanded> {
    let bytes = std::fs::read(file)?;
    let mut state = State { root: root.canonicalize()?, included: file.canonicalize().into_iter().collect(), added: 0, notes: Vec::new() };
    let first = state.included.len(); // the document itself is not an import
    let text = expand_text(&bytes, file.parent().unwrap_or(Path::new(".")), 0, &mut state);
    Ok(Expanded { text, imported: state.included.split_off(first), notes: state.notes })
}

/// A code fence's opening or closing line: its character, its length, and whether anything but the fence is on the
/// line (an opening may name a language; a closing may not).
fn fence_of(line: &[u8]) -> Option<(u8, usize, bool)> {
    let line = line.trim_ascii_end();
    let indent = line.iter().take_while(|b| **b == b' ').count();
    if indent > 3 {
        return None; // four spaces make a code block of their own, not a fence
    }
    let rest = &line[indent..];
    let c = *rest.first().filter(|c| matches!(**c, b'`' | b'~'))?;
    let n = rest.iter().take_while(|b| **b == c).count();
    (n >= 3).then_some((c, n, rest.len() == n))
}

/// Where each `@path` import starts (the `@`) and ends in a line: at the line's start or after whitespace, and not
/// in an inline code span.
fn import_tokens(line: &[u8]) -> Vec<(usize, usize)> {
    let (mut found, mut code, mut i) = (Vec::new(), false, 0);
    while i < line.len() {
        match line[i] {
            b'`' => code = !code,
            b'@' if !code && (i == 0 || line[i - 1].is_ascii_whitespace()) && line.get(i + 1).is_some_and(|b| !b.is_ascii_whitespace()) => {
                let end = line[i..].iter().position(|b| b.is_ascii_whitespace()).map_or(line.len(), |n| i + n);
                found.push((i, end));
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    found
}

/// The file a token names, when it is one that may be imported: it exists, is a file, and once its links are
/// resolved is inside the repository. Read from the importing file's folder; `~` paths are never followed. A token
/// that ends in punctuation ("see @AGENTS.md.") names the file without it.
fn resolve(token: &[u8], folder: &Path, state: &State) -> Option<PathBuf> {
    let token = std::str::from_utf8(token).ok().filter(|t| !t.starts_with('~') && !t.contains('\0'))?;
    let mut name = token;
    loop {
        if let Ok(file) = folder.join(name).canonicalize() && file.is_file() && file.starts_with(&state.root) {
            return Some(file);
        }
        name = name.strip_suffix(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'])?;
        if name.is_empty() {
            return None;
        }
    }
}

/// A file's text with its own imports expanded, once: None when it was taken in already (a cycle, or twice), could
/// not be read, or would take what is added past `MAX_ADDED`.
fn include(file: &Path, depth: usize, state: &mut State) -> Option<Vec<u8>> {
    if state.included.iter().any(|f| f == file) {
        return None;
    }
    let bytes = std::fs::read(file).ok()?;
    if state.added + bytes.len() > MAX_ADDED {
        if state.notes.is_empty() {
            let shown = file.strip_prefix(&state.root).unwrap_or(file).display();
            state.notes.push(format!("@import {shown} is left as written: the imports would add more than {} KiB", MAX_ADDED / 1024));
        }
        return None;
    }
    state.added += bytes.len();
    state.included.push(file.to_path_buf());
    Some(expand_text(&bytes, file.parent().unwrap_or(Path::new(".")), depth + 1, state))
}

fn expand_text(bytes: &[u8], folder: &Path, depth: usize, state: &mut State) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut after: Vec<(String, Vec<u8>)> = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        if let Some((c, n, bare)) = fence_of(content) {
            match fence {
                None => fence = Some((c, n)),
                Some((open, len)) if c == open && n >= len && bare => fence = None,
                _ => {}
            }
            out.extend_from_slice(line);
            continue;
        }
        if fence.is_some() || depth >= MAX_DEPTH {
            out.extend_from_slice(line);
            continue;
        }
        let found: Vec<(usize, usize, PathBuf)> = import_tokens(content).into_iter()
            .filter_map(|(start, end)| Some((start, end, resolve(&content[start + 1..end], folder, state)?))).collect();
        if found.is_empty() {
            out.extend_from_slice(line);
        } else if found.len() == 1 && content.trim_ascii() == &content[found[0].0..found[0].1] {
            // the line is the import alone: the file's text stands where it was
            match include(&found[0].2, depth, state) {
                Some(text) => {
                    out.extend_from_slice(&text);
                    if line.ends_with(b"\n") && !text.ends_with(b"\n") {
                        out.push(b'\n');
                    }
                }
                None => out.extend_from_slice(line),
            }
        } else {
            out.extend_from_slice(line);
            for (_, _, file) in found {
                if let Some(text) = include(&file, depth, state) {
                    after.push((file.strip_prefix(&state.root).unwrap_or(&file).display().to_string(), text));
                }
            }
        }
    }
    for (shown, text) in after {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(format!("\n## Imported from {shown}\n\n").as_bytes());
        out.extend_from_slice(&text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh repository folder for one test.
    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ringo-imports-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn text(repo: &Path, file: &str) -> String {
        String::from_utf8(expand(&repo.join(file), repo).unwrap().text).unwrap()
    }

    #[test]
    fn a_claude_md_that_is_only_an_import_becomes_the_file_once() {
        let r = repo("whole");
        fs::write(r.join("AGENTS.md"), "Rule one.\nRule two.\n").unwrap();
        fs::write(r.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        let got = expand(&r.join("CLAUDE.md"), &r).unwrap();
        assert_eq!(String::from_utf8(got.text).unwrap(), "Rule one.\nRule two.\n");
        assert_eq!(got.imported.len(), 1);
        // beside other lines, and the file with no final newline keeps the line break after it
        fs::write(r.join("AGENTS.md"), "Rule.").unwrap();
        fs::write(r.join("CLAUDE.md"), "# Mine\n\n  @AGENTS.md  \nafter\n").unwrap();
        assert_eq!(text(&r, "CLAUDE.md"), "# Mine\n\nRule.\nafter\n");
        // a file with no imports comes back as it is
        let plain = expand(&r.join("AGENTS.md"), &r).unwrap();
        assert!(plain.imported.is_empty() && plain.text == b"Rule.");
    }

    #[test]
    fn an_import_inside_a_line_goes_after_the_document_under_a_heading() {
        let r = repo("inline");
        fs::create_dir_all(r.join("docs")).unwrap();
        fs::write(r.join("docs/style.md"), "Use tabs.\n").unwrap();
        fs::write(r.join("CLAUDE.md"), "Follow @docs/style.md, always.\nSee also @docs/style.md.\nEnd.\n").unwrap();
        // the line stays; the file is added once, after the document (trailing punctuation is not part of the name)
        assert_eq!(text(&r, "CLAUDE.md"),
                   "Follow @docs/style.md, always.\nSee also @docs/style.md.\nEnd.\n\n## Imported from docs/style.md\n\nUse tabs.\n");
    }

    #[test]
    fn imports_nest_from_the_importing_files_folder_and_cycles_add_nothing() {
        let r = repo("nested");
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::write(r.join("CLAUDE.md"), "root\n@a/one.md\n").unwrap();
        fs::write(r.join("a/one.md"), "one\n@b/two.md\n").unwrap();
        // two.md imports ../one.md (a cycle) and ../../CLAUDE.md (the document itself): both skipped, left as written
        fs::write(r.join("a/b/two.md"), "two\n@../one.md\n@../../CLAUDE.md\n").unwrap();
        assert_eq!(text(&r, "CLAUDE.md"), "root\none\ntwo\n@../one.md\n@../../CLAUDE.md\n");
        // five hops are followed, a sixth is not
        for n in 0..8 {
            fs::write(r.join(format!("d{n}.md")), format!("level {n}\n@d{}.md\n", n + 1)).unwrap();
        }
        fs::write(r.join("deep.md"), "@d0.md\n").unwrap();
        let deep = text(&r, "deep.md");
        assert!(deep.contains("level 4\n") && deep.ends_with("level 4\n@d5.md\n"), "{deep}");
    }

    #[test]
    fn nothing_outside_the_repository_is_read() {
        let r = repo("outside");
        let outside = repo("outside-secret");
        fs::write(outside.join("secret.md"), "SECRET\n").unwrap();
        fs::write(r.join("inside.md"), "fine\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), r.join("link.md")).unwrap();
        std::os::unix::fs::symlink(&outside, r.join("dir")).unwrap();
        fs::write(r.join("CLAUDE.md"), format!("@link.md\n@dir/secret.md\n@../{0}/secret.md\n@{1}/secret.md\n@~/.bashrc\n@inside.md\n",
                                               outside.file_name().unwrap().to_string_lossy(), outside.display())).unwrap();
        let got = text(&r, "CLAUDE.md");
        assert!(!got.contains("SECRET"), "{got}");
        assert!(got.contains("@link.md\n@dir/secret.md\n") && got.contains("@~/.bashrc\n") && got.ends_with("fine\n"), "{got}");
    }

    #[test]
    fn code_blocks_and_spans_and_addresses_are_not_imports() {
        let r = repo("code");
        fs::write(r.join("x.md"), "X\n").unwrap();
        let doc = "In `@x.md` it is code.\n```\n@x.md\n```\n~~~md\n@x.md\n~~~\nmail me@x.md or \"@x.md\" or @missing.md\n\n    @x.md\n@x.md\n";
        fs::write(r.join("CLAUDE.md"), doc).unwrap();
        // the indented line (alone on it) is the first real import and takes the file in; the last line names it
        // again, and a file is included once, so that one stays as written
        let got = text(&r, "CLAUDE.md");
        assert!(got.starts_with("In `@x.md` it is code.\n```\n@x.md\n```\n~~~md\n@x.md\n~~~\nmail me@x.md or \"@x.md\" or @missing.md\n"), "{got}");
        assert_eq!(got.matches("X\n").count(), 1, "{got}");
    }

    #[test]
    fn a_missing_file_stays_as_written_and_bytes_need_not_be_utf8() {
        let r = repo("bytes");
        fs::write(r.join("CLAUDE.md"), b"keep \xff\xfe bytes\n@nothing.md\n@bin.md\n").unwrap();
        fs::write(r.join("bin.md"), b"\xc3\x28 raw\n").unwrap();
        let got = expand(&r.join("CLAUDE.md"), &r).unwrap().text;
        assert_eq!(got, b"keep \xff\xfe bytes\n@nothing.md\n\xc3\x28 raw\n");
    }

    #[test]
    fn what_imports_add_is_capped() {
        let r = repo("cap");
        fs::write(r.join("big.md"), "x".repeat(MAX_ADDED - 10)).unwrap();
        fs::write(r.join("more.md"), "y".repeat(100)).unwrap();
        fs::write(r.join("CLAUDE.md"), "@big.md\n@more.md\n").unwrap();
        let got = expand(&r.join("CLAUDE.md"), &r).unwrap();
        assert_eq!(got.imported.len(), 1, "the second would pass the cap");
        assert!(got.text.ends_with(b"\n@more.md\n") && got.notes.len() == 1 && got.notes[0].contains("more.md"), "{:?}", got.notes);
    }
}

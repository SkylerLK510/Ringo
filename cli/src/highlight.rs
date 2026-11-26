//! Lightweight, zero-dependency syntax and diff highlighter for fenced code blocks in the TUI.
//!
//! Uses a single-pass line tokenizer (no external crates, minimal allocations) so diffs (`+`/`-`/`@@`)
//! and common languages (`rust`, `python`, `ts`/`js`, `go`, `bash`/`sh`, `json`/`toml`/`yaml`) scan
//! cleanly without slowing down terminal streaming or bloating binary size.

use ratatui::style::Modifier;
use ratatui::text::Span;

use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Diff,
    Rust,
    Python,
    JsTs,
    Go,
    Shell,
    Data,
    Plain,
}

fn classify(lang: Option<&str>) -> Lang {
    match lang.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("diff" | "patch") => Lang::Diff,
        Some("rs" | "rust") => Lang::Rust,
        Some("py" | "python" | "python3") => Lang::Python,
        Some("js" | "jsx" | "ts" | "tsx" | "javascript" | "typescript") => Lang::JsTs,
        Some("go" | "golang") => Lang::Go,
        Some("sh" | "bash" | "zsh" | "shell") => Lang::Shell,
        Some("json" | "toml" | "yaml" | "yml") => Lang::Data,
        _ => Lang::Plain,
    }
}

fn is_keyword(lang: Lang, word: &str) -> bool {
    match lang {
        Lang::Rust => matches!(
            word,
            "fn" | "let" | "mut" | "pub" | "struct" | "enum" | "impl" | "trait" | "for" | "in" | "if" | "else"
                | "match" | "return" | "use" | "mod" | "const" | "static" | "async" | "await" | "where" | "type"
                | "self" | "Self" | "true" | "false" | "Some" | "None" | "Ok" | "Err" | "while" | "loop" | "break"
        ),
        Lang::Python => matches!(
            word,
            "def" | "class" | "import" | "from" | "as" | "if" | "elif" | "else" | "for" | "in" | "while" | "return"
                | "yield" | "with" | "try" | "except" | "finally" | "raise" | "pass" | "break" | "continue" | "and"
                | "or" | "not" | "is" | "None" | "True" | "False" | "async" | "await" | "lambda"
        ),
        Lang::JsTs => matches!(
            word,
            "function" | "const" | "let" | "var" | "return" | "if" | "else" | "for" | "of" | "in" | "while"
                | "switch" | "case" | "break" | "import" | "export" | "from" | "default" | "class" | "extends"
                | "interface" | "type" | "async" | "await" | "try" | "catch" | "throw" | "new" | "true" | "false"
                | "null" | "undefined"
        ),
        Lang::Go => matches!(
            word,
            "func" | "package" | "import" | "type" | "struct" | "interface" | "var" | "const" | "if" | "else"
                | "for" | "range" | "switch" | "case" | "return" | "defer" | "go" | "chan" | "select" | "map"
                | "true" | "false" | "nil"
        ),
        Lang::Shell => matches!(
            word,
            "if" | "then" | "else" | "elif" | "fi" | "for" | "do" | "done" | "while" | "case" | "esac" | "in"
                | "export" | "local" | "return" | "exit" | "set"
        ),
        Lang::Data => matches!(word, "true" | "false" | "null"),
        Lang::Diff | Lang::Plain => false,
    }
}

fn line_comment_prefix(lang: Lang) -> Option<&'static str> {
    match lang {
        Lang::Rust | Lang::JsTs | Lang::Go => Some("//"),
        Lang::Python | Lang::Shell | Lang::Data => Some("#"),
        Lang::Diff | Lang::Plain => None,
    }
}

/// Highlights one line of code or diff into styled ratatui spans.
pub fn highlight_line(lang: Option<&str>, line: &str, theme: &Theme) -> Vec<Span<'static>> {
    let kind = classify(lang);
    let base = theme.ringo;
    if kind == Lang::Plain {
        return vec![Span::styled(line.to_string(), base)];
    }
    if kind == Lang::Diff {
        let style = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
            theme.title.add_modifier(Modifier::BOLD)
        } else if line.starts_with("@@") {
            theme.focus
        } else if line.starts_with('+') {
            theme.ok
        } else if line.starts_with('-') {
            theme.fail
        } else {
            theme.dim
        };
        return vec![Span::styled(line.to_string(), style)];
    }

    let kw_style = theme.focus.add_modifier(Modifier::BOLD);
    let str_style = theme.ok;
    let num_style = theme.title;
    let cmt_style = theme.dim;
    let comment = line_comment_prefix(kind);

    let bytes = line.as_bytes();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    let mut plain_start = 0;

    let flush_plain = |out: &mut Vec<Span<'static>>, start: usize, end: usize| {
        if end > start {
            out.push(Span::styled(line[start..end].to_string(), base));
        }
    };

    while i < bytes.len() {
        // Line comment to end of line
        if let Some(pfx) = comment
            && line[i..].starts_with(pfx)
        {
            flush_plain(&mut out, plain_start, i);
            out.push(Span::styled(line[i..].to_string(), cmt_style));
            return out;
        }
        let b = bytes[i];
        // Quoted string literal
        if b == b'"' || b == b'\'' || (kind == Lang::JsTs && b == b'`') {
            flush_plain(&mut out, plain_start, i);
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Span::styled(line[start..i].to_string(), str_style));
            plain_start = i;
            continue;
        }
        // Identifier / keyword
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if is_keyword(kind, word) {
                flush_plain(&mut out, plain_start, start);
                out.push(Span::styled(word.to_string(), kw_style));
                plain_start = i;
            }
            continue;
        }
        // Numeric literal (only when preceded by non-identifier byte)
        if b.is_ascii_digit() && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')) {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            flush_plain(&mut out, plain_start, start);
            out.push(Span::styled(line[start..i].to_string(), num_style));
            plain_start = i;
            continue;
        }
        // Advance to next UTF-8 char boundary
        let ch_len = line[i..].chars().next().map_or(1, char::len_utf8);
        i += ch_len;
    }

    flush_plain(&mut out, plain_start, bytes.len());
    if out.is_empty() {
        out.push(Span::styled(String::new(), base));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Depth;

    #[test]
    fn highlights_diff_lines_with_semantic_colors() {
        let t = Theme::named("siamese", Depth::TrueColor).unwrap();
        let added = highlight_line(Some("diff"), "+let x = 1;", &t);
        let removed = highlight_line(Some("diff"), "-let x = 0;", &t);
        let hunk = highlight_line(Some("diff"), "@@ -1,2 +1,2 @@", &t);
        assert_eq!(added[0].style, t.ok);
        assert_eq!(removed[0].style, t.fail);
        assert_eq!(hunk[0].style, t.focus);
    }

    #[test]
    fn highlights_rust_keywords_strings_numbers_and_comments_without_losing_text() {
        let t = Theme::named("siamese", Depth::TrueColor).unwrap();
        let src = r#"let count = 42; // "not a string""#;
        let spans = highlight_line(Some("rust"), src, &t);
        let rejoined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rejoined, src);
        assert!(spans.iter().any(|s| s.content == "let" && s.style == t.focus.add_modifier(Modifier::BOLD)));
        assert!(spans.iter().any(|s| s.content == "42" && s.style == t.title));
        assert!(spans.iter().any(|s| s.content.starts_with("//") && s.style == t.dim));
    }

    #[test]
    fn preserves_utf8_and_plain_fallback() {
        let t = Theme::named("siamese", Depth::TrueColor).unwrap();
        let src = r#"msg = "héllo 🌍" # note"#;
        let spans = highlight_line(Some("python"), src, &t);
        let rejoined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rejoined, src);
        assert!(spans.iter().any(|s| s.content == "\"héllo 🌍\"" && s.style == t.ok));
        let plain = highlight_line(None, "raw text", &t);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].content, "raw text");
    }
}

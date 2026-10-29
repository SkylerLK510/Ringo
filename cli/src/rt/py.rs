//! Python string semantics the ported modules rely on, where Rust's std differs: `str.isspace` (it also counts
//! U+001C..U+001F), `str.strip()`, `str.split()`, `str.splitlines()`, code-point lengths and slices, `repr(str)`,
//! and the text of an `OSError`.

/// Python's `str.isspace()` for one character: Unicode White_Space plus the four information separators.
pub fn isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(isspace)
}

pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(isspace)
}

pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(isspace)
}

/// `str.split()` with no arguments: runs of whitespace separate, and none are empty.
pub fn split(s: &str) -> Vec<&str> {
    s.split(isspace).filter(|p| !p.is_empty()).collect()
}

fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `str.splitlines(keepends)`: every Python line boundary, `\r\n` as one.
pub fn splitlines(s: &str, keepends: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if !line_break(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r'
            && let Some(&(j, '\n')) = it.peek() {
                it.next();
                end = j + 1;
            }
        out.push(if keepends { &s[start..end] } else { &s[start..i] });
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `len(s)`: code points, not bytes.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

/// `s[:n]` by code points.
pub fn head(s: &str, n: usize) -> &str {
    &s[..byte_at(s, n)]
}

/// `s[-n:]` by code points (`s[-0:]` is the whole string in Python, and so it is here).
pub fn tail(s: &str, n: usize) -> &str {
    let count = len(s);
    if n == 0 || n >= count {
        return s;
    }
    &s[byte_at(s, count - n)..]
}

/// `s[a:b]` by code points, for non-negative bounds.
pub fn slice(s: &str, a: usize, b: usize) -> &str {
    let start = byte_at(s, a);
    let end = byte_at(s, b.max(a));
    &s[start..end]
}

/// `repr(s)` for the strings Ringo puts in messages (paths, names): quotes chosen as Python chooses them, the
/// escapes Python uses for backslash, quote, \n \r \t, other control characters as \xNN, and U+007F..U+00A0 as \xNN.
/// Printable non-ASCII text stays as it is. (Python also escapes other non-printable code points such as format
/// characters; std has no Unicode categories, so those pass through here.)
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..=0xa0).contains(&(c as u32)) || c == '\u{ad}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The text Python prints for an OSError: `[Errno N] <strerror>` and, given a file name, `: '<name>'`.
pub fn os_error(e: &std::io::Error, filename: Option<&str>) -> String {
    let base = match e.raw_os_error() {
        Some(code) => format!("[Errno {code}] {}", strerror(code)),
        None => e.to_string(),
    };
    match filename {
        Some(name) => format!("{base}: {}", repr(name)),
        None => base,
    }
}

fn strerror(code: i32) -> String {
    #[cfg(unix)]
    unsafe {
        let p = libc::strerror(code);
        if !p.is_null() {
            return std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        }
    }
    let text = std::io::Error::from_raw_os_error(code).to_string();
    text.strip_suffix(&format!(" (os error {code})")).unwrap_or(&text).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_string_semantics() {
        assert_eq!(strip("\u{1c} a b\u{a0}\n"), "a b");
        assert_eq!(split("  a\u{1f}b\t c "), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\r\nb\rc\u{2028}d\n", false), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\r\nb", true), vec!["a\r\n", "b"]);
        assert_eq!(splitlines("", false), Vec::<&str>::new());
        assert_eq!(splitlines("\n", false), vec![""]);
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("ab", 0), "ab");
        assert_eq!(slice("héllo", 1, 3), "él");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'\"\n\x7f"), "'a\\'\"\\n\\x7f'");
    }
}

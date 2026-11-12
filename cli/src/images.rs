//! Image attachments live outside projects. Their markers survive queues, retries and /restart.
use crate::Adapter;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PREFIX: &str = "[image:";
const MAX_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMAGES: usize = 8;
static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn has_images(text: &str) -> bool {
    text.contains(PREFIX)
}

pub fn marker(path: &Path) -> String {
    format!("{PREFIX}{}]", path.display())
}

fn read(path: &Path) -> Result<(Vec<u8>, &'static str, &'static str), String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A mistakenly attached pipe must not freeze the shell before we can check its type.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("attach an image file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("each image must be at most 20 MiB".into());
    }
    let (mime, ext) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", "png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        ("image/jpeg", "jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        ("image/gif", "gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        ("image/webp", "webp")
    } else {
        return Err("attach a PNG, JPEG, GIF or WebP image".into());
    };
    Ok((bytes, mime, ext))
}

/// A single path, including paths quoted or escaped by the terminal when a file is dragged into it.
pub fn path(text: &str, cwd: &Path) -> PathBuf {
    let text = text.trim();
    let text = if text.len() >= 2
        && ((text.starts_with('\'') && text.ends_with('\''))
            || (text.starts_with('"') && text.ends_with('"')))
    {
        text[1..text.len().saturating_sub(1)].to_string()
    } else {
        let mut chars = text.chars();
        let mut out = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' {
                out.push(chars.next().unwrap_or(c));
            } else {
                out.push(c);
            }
        }
        out
    };
    let expanded = match text.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(text),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Copy once, so moving or deleting the original cannot change a queued attachment.
pub fn attach(source: &Path, session: &Path) -> Result<String, String> {
    let (bytes, _, ext) = read(source)?;
    let dir = session.join("images");
    crate::private_dir(&dir)?;
    let path = dir.join(format!(
        "{}-{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        ext
    ));
    if path.to_str().is_none_or(|s| s.contains([']', '\n', '\r'])) {
        return Err(
            "the image storage path cannot contain ], newlines or non-Unicode characters".into(),
        );
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(marker(&path))
}

/// Native provider input, shared by initial messages and messages sent during a running turn.
/// `text` may include Ringo's handoff; only the current owner's message supplies new image blocks.
pub fn content(adapter: Adapter, text: &str, message: &str) -> Result<Value, String> {
    let mut paths = Vec::new();
    let mut rest = message;
    while let Some((_, tail)) = rest.split_once(PREFIX) {
        let (path, tail) = tail
            .split_once(']')
            .ok_or("incomplete image attachment; attach the image again")?;
        if !paths.contains(&path) {
            paths.push(path);
        }
        rest = tail;
    }
    if paths.is_empty() {
        return Ok(match adapter {
            Adapter::Claude => json!(text),
            _ => json!([{"type": "text", "text": text}]),
        });
    }
    if !matches!(adapter, Adapter::Claude | Adapter::Codex) {
        return Err(
            "image attachments are supported by @claude and @codex; choose one of them".into(),
        );
    }
    if paths.len() > MAX_IMAGES {
        return Err("send at most 8 images in one message".into());
    }
    let mut blocks = vec![json!({"type": "text", "text": text})];
    for path in paths {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err("image attachment paths must be absolute; use /attach PATH".into());
        }
        let (bytes, mime, _) = read(path)?;
        blocks.push(match adapter {
            Adapter::Codex => json!({"type": "localImage", "path": path}),
            _ => json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": STANDARD.encode(bytes)}}),
        });
    }
    Ok(json!(blocks))
}

/// Clipboard access is explicit (Ctrl-V or /attach clipboard), never polled in the background.
#[cfg(target_os = "macos")]
pub fn clipboard(session: &Path) -> Result<String, String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let dir = session.join("images");
    crate::private_dir(&dir)?;
    let temporary = dir.join(format!(
        "clipboard-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let script = r#"on run argv
set target to open for access POSIX file (item 1 of argv) with write permission
try
    set eof target to 0
    write (the clipboard as «class PNGf») to target
    close access target
on error problem
    close access target
    error problem
end try
end run"#;
    let result = (|| {
        let mut child = Command::new("osascript")
            .args(["-e", script])
            .arg(&temporary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if !status.success() {
                    return Err(
                        "no image on the clipboard; copy a screenshot, or use /attach PATH".into(),
                    );
                }
                return attach(&temporary, session);
            }
            if start.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("clipboard access timed out; use /attach PATH".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard(_: &Path) -> Result<String, String> {
    Err("clipboard images are supported on macOS; use /attach PATH here".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real one-pixel PNG, with no private screenshot content.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aOmsAAAAASUVORK5CYII=";

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ringo-images-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn attachment_is_private_and_independent_of_the_original_and_encodes_for_both_providers() {
        let dir = dir();
        let source = dir.join("Screenshot with spaces.png");
        fs::write(&source, STANDARD.decode(PNG).unwrap()).unwrap();
        let session = dir.join("private");
        let marker = attach(&source, &session).unwrap();
        fs::remove_file(source).unwrap();
        let text = format!("What went wrong? {marker}");
        let codex = content(Adapter::Codex, &text, &text).unwrap();
        let copied = Path::new(codex[1]["path"].as_str().unwrap());
        assert!(copied.starts_with(&session));
        assert_eq!(codex[1]["type"], "localImage");
        assert_eq!(STANDARD.encode(fs::read(copied).unwrap()), PNG);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(copied).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(copied.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let claude = content(Adapter::Claude, &text, &text).unwrap();
        assert_eq!(claude[0]["text"], text);
        assert_eq!(
            claude[1],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG}})
        );
        let duplicate = format!("{marker} {marker}");
        assert_eq!(
            content(Adapter::Codex, &duplicate, &duplicate)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_missing_large_and_unsupported_attachments_fail_clearly() {
        let dir = dir();
        let source = dir.join("fake.png");
        fs::write(&source, b"these are not image bytes").unwrap();
        assert!(attach(&source, &dir).unwrap_err().contains("PNG"));
        assert!(attach(&dir, &dir).is_err());
        assert!(attach(&dir.join("missing.png"), &dir).is_err());
        #[cfg(unix)]
        {
            let fifo = dir.join("pipe.png");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(attach(&fifo, &dir).unwrap_err().contains("image file"));
        }
        let large = fs::File::create(&source).unwrap();
        large.set_len(MAX_BYTES + 1).unwrap();
        assert!(attach(&source, &dir).unwrap_err().contains("20 MiB"));
        assert!(
            content(Adapter::Codex, "[image:broken", "[image:broken")
                .unwrap_err()
                .contains("incomplete")
        );
        assert!(
            content(
                Adapter::Codex,
                "[image:relative.png]",
                "[image:relative.png]"
            )
            .unwrap_err()
            .contains("absolute")
        );
        assert!(
            content(Adapter::Agy, "[image:/unused]", "[image:/unused]")
                .unwrap_err()
                .contains("@claude")
        );
        let many: String = (0..9).map(|n| format!("[image:/image{n}.png] ")).collect();
        assert!(
            content(Adapter::Codex, &many, &many)
                .unwrap_err()
                .contains("at most 8")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dragged_quoted_escaped_and_relative_paths_keep_spaces_and_unicode() {
        let cwd = Path::new("/project");
        for text in [
            "'/tmp/Screen shot 猫.png' ",
            "\"/tmp/Screen shot 猫.png\"",
            "/tmp/Screen\\ shot\\ 猫.png ",
        ] {
            assert_eq!(path(text, cwd), Path::new("/tmp/Screen shot 猫.png"));
        }
        assert_eq!(path("shot.png", cwd), cwd.join("shot.png"));
        assert_eq!(path("'", cwd), cwd.join("'"));
    }
}

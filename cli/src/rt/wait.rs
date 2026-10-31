//! `ringo wait FOLDER`, ported from `ringo.py wait`: block until a run's state.json leaves 'running', then print
//! it as one line. A deterministic wait: one command instead of an agent re-entering its model to sleep and poll.

use super::pyjson;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// wait's exit codes: ready or done 0, finished without a ready result 1, failed or gone 2, still running 3.
fn exit_code(status: &str) -> i32 {
    match status {
        "done" | "ready_for_review" => 0,
        "needs_revision" | "repeated_candidate" | "budget_exhausted" => 1,
        _ => 2,
    }
}

/// pathlib's spelling of a path: no empty or '.' parts, no trailing slash, '.' for nothing.
fn pure(path: &str) -> PathBuf {
    let absolute = path.starts_with('/');
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let text = format!("{}{}", if absolute { "/" } else { "" }, parts.join("/"));
    PathBuf::from(if text.is_empty() { ".".to_string() } else { text })
}

fn read(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    pyjson::loads(&text).ok()
}

fn running(state: &Option<Value>) -> bool {
    state.as_ref().and_then(|s| s.get("status")).and_then(|s| s.as_str()) == Some("running")
}

/// Whether the run's process is gone: Some(true) gone, Some(false) alive, None when it cannot be asked.
fn gone(state: &Value) -> Option<bool> {
    let pid = state.get("pid")?.as_i64()?;
    if !(0..=i32::MAX as i64).contains(&pid) {
        return None;
    }
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            return Some(false);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Some(true),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    None
}

pub fn main(argv: &[String]) -> i32 {
    let usage = "usage: ringo wait [-h] [--timeout TIMEOUT] folder";
    let (mut folder, mut timeout) = (None, 540.0f64);
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{usage}\n\nBlock until a run's state.json leaves 'running', then print it as one line.");
                return 0;
            }
            "--timeout" => match it.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) => timeout = v,
                None => {
                    eprintln!("{usage}\nringo wait: error: argument --timeout: expected a number");
                    return 2;
                }
            },
            a if a.starts_with("--timeout=") => match a[10..].parse::<f64>() {
                Ok(v) => timeout = v,
                Err(_) => {
                    eprintln!("{usage}\nringo wait: error: argument --timeout: expected a number");
                    return 2;
                }
            },
            a if folder.is_none() && !a.starts_with('-') => folder = Some(a.to_string()),
            a => {
                eprintln!("{usage}\nringo wait: error: unrecognized arguments: {a}");
                return 2;
            }
        }
    }
    let Some(folder) = folder else {
        eprintln!("{usage}\nringo wait: error: the following arguments are required: folder");
        return 2;
    };
    let path = pure(&folder).join("state.json");
    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let mut state;
    loop {
        state = read(&path);
        if running(&state) && gone(state.as_ref().unwrap()) == Some(true) {
            state = read(&path); // it may have finished between the read and the check: read again
            if running(&state) {
                let s = state.as_mut().unwrap().as_object_mut().unwrap();
                s.insert("status".into(), "interrupted".into());
                s.insert("error".into(), "the run process is gone".into());
            }
        }
        if (state.is_some() && !running(&state)) || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    let Some(Value::Object(mut state)) = state else {
        let parent = path.parent().unwrap().to_string_lossy().into_owned();
        println!("{}", pyjson::dumps(&serde_json::json!({"status": "not_started", "folder": parent})));
        return 3;
    };
    state.shift_remove("version");
    state.shift_remove("pid");
    let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("").to_string();
    println!("{}", pyjson::dumps_compact(&Value::Object(state)));
    if status == "running" { 3 } else { exit_code(&status) }
}

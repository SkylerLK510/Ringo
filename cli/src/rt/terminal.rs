//! Terminal-only progress and wall-time throughput shared by the native review/design/work commands.
use super::{
    harness::{self, Run},
    py, pyjson,
};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub fn rate(usage: Option<&Value>, seconds: Option<&Value>) -> Value {
    let count = usage
        .and_then(|u| u.get("output"))
        .and_then(py::int_of)
        .filter(|n| *n >= 0);
    let seconds = seconds
        .and_then(harness::valid_price)
        .map(|n| n.as_f64())
        .filter(|n| *n > 0.0);
    match (count, seconds) {
        (Some(n), Some(s)) => pyjson::float_value(py::round(n as f64 / s, 2)),
        _ => Value::Null,
    }
}

pub fn is_terminal() -> bool {
    std::io::stderr().is_terminal() && std::env::var("TERM").ok().as_deref() != Some("dumb")
}

pub fn summary_text(s: &Map<String, Value>, style: &str) -> String {
    if style == "json" || (style == "auto" && !is_terminal()) {
        return format!("harness: {}", pyjson::dumps(&Value::Object(s.clone())));
    }
    let number = |k: &str| s.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let integer = |k: &str| s.get(k).and_then(py::int_of).unwrap_or(0);
    let calls = |n: i128| format!("{n} call{}", if n == 1 { "" } else { "s" });
    let total: i128 = harness::TOKEN_KEYS.iter().map(|k| integer(k)).sum();
    let mut tokens = format!("{} total tokens", grouped(total));
    if integer("untokened_calls") > 0 {
        tokens += &format!(
            " (partial; {} unreported)",
            calls(integer("untokened_calls"))
        );
    } else if integer("calls") == 0 {
        tokens += " (no calls)";
    } else if integer("unfinished_calls") > 0 {
        tokens += " (partial; unfinished call)";
    }
    tokens += &format!(
        " [{} input + {} cache read + {} cache write + {} output]",
        grouped(integer("input")),
        grouped(integer("cache_read")),
        grouped(integer("cache_write")),
        grouped(integer("output"))
    );
    let speed = s
        .get("output_tokens_per_second")
        .and_then(Value::as_f64)
        .map_or_else(
            || "output tok/s unknown".into(),
            |r| format!("{r:.1} output tok/s"),
        );
    let mut price = format!("${:.4}", number("cost_usd"));
    if integer("unpriced_calls") > 0 {
        price += &format!(" + {} unpriced", calls(integer("unpriced_calls")));
    }
    format!(
        "ringo: {} | {:.2}s | {tokens} | {speed} (wall time) | {price}",
        calls(integer("calls")),
        number("elapsed_s")
    )
}

fn grouped(n: i128) -> String {
    let text = n.to_string();
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            if i > 0 && (text.len() - i) % 3 == 0 {
                format!(",{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn write_raw(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

pub struct Progress {
    animated: bool,
    started: Instant,
    active: Mutex<BTreeMap<(String, String), String>>,
    stopped: Mutex<bool>,
    wake: Condvar,
}
impl Progress {
    fn draw(&self, active: &BTreeMap<(String, String), String>) {
        if let Some(label) = active.values().next() {
            let line = format!(
                "ringo: {label} | {} active | {:.1}s elapsed",
                active.len(),
                self.started.elapsed().as_secs_f64()
            );
            #[cfg(unix)]
            let width = unsafe {
                let mut size: libc::winsize = std::mem::zeroed();
                if libc::ioctl(2, libc::TIOCGWINSZ, &mut size) == 0 {
                    size.ws_col as usize
                } else {
                    80
                }
            };
            #[cfg(not(unix))]
            let width = 80;
            write_raw(&format!(
                "\r\x1b[K{}",
                line.chars()
                    .take(width.saturating_sub(1))
                    .collect::<String>()
            ));
        }
    }
    pub fn update(&self, rec: &Map<String, Value>) {
        let step = rec
            .get("step")
            .map(py::value_str)
            .unwrap_or_else(|| "call".into());
        let label: String = step
            .chars()
            .take(60)
            .map(|c| if py::isprintable(c) { c } else { '?' })
            .collect();
        let attempt = rec
            .get("attempt")
            .map(py::value_str)
            .unwrap_or_else(|| "1".into());
        let status = rec.get("status").and_then(Value::as_str).unwrap_or("");
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let key = (step, attempt.clone());
        if status == "started" || status == "reserved" {
            let text = format!("{label} (attempt {attempt})");
            active.insert(key, text.clone());
            if self.animated {
                self.draw(&active);
            } else {
                harness::print_stderr(&format!("ringo: {text} started"));
            }
        } else {
            active.remove(&key);
            let mut detail = rec
                .get("seconds")
                .and_then(Value::as_f64)
                .map_or(String::new(), |s| format!(" | {s:.2}s"));
            if let Some(r) = rec.get("output_tokens_per_second").and_then(Value::as_f64) {
                detail += &format!(" | {r:.1} output tok/s (wall time)");
            }
            let prefix = if self.animated { "\r\x1b[K" } else { "" };
            harness::print_stderr(&format!("{prefix}ringo: {label} {status}{detail}"));
            if self.animated {
                self.draw(&active);
            }
        }
    }
}

pub struct Guard {
    run: Arc<Run>,
    progress: Option<Arc<Progress>>,
    worker: Option<JoinHandle<()>>,
}
impl Guard {
    pub fn new(run: &Arc<Run>, mode: &str) -> Self {
        let animated = is_terminal();
        let enabled = mode == "always" || (mode == "auto" && animated);
        let progress = enabled.then(|| {
            Arc::new(Progress {
                animated,
                started: run.clock_started,
                active: Mutex::new(BTreeMap::new()),
                stopped: Mutex::new(false),
                wake: Condvar::new(),
            })
        });
        run.set_progress(progress.clone());
        let worker = progress.as_ref().filter(|p| p.animated).map(|p| {
            let p = Arc::clone(p);
            std::thread::spawn(move || {
                let mut stopped = p.stopped.lock().unwrap_or_else(|e| e.into_inner());
                while !*stopped {
                    let result = p
                        .wake
                        .wait_timeout(stopped, Duration::from_millis(200))
                        .unwrap_or_else(|e| e.into_inner());
                    stopped = result.0;
                    if !*stopped {
                        p.draw(&p.active.lock().unwrap_or_else(|e| e.into_inner()));
                    }
                }
            })
        });
        Self {
            run: Arc::clone(run),
            progress,
            worker,
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.run.set_progress(None);
        if let Some(p) = &self.progress {
            *p.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
            p.wake.notify_all();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            if p.animated {
                write_raw("\r\x1b[K");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rates_use_reported_output_and_elapsed_wall_time() {
        assert_eq!(
            rate(Some(&json!({"output": 40})), Some(&json!(2.5))),
            json!(16.0)
        );
        assert_eq!(
            rate(Some(&json!({"output": 0})), Some(&json!(2))),
            json!(0.0)
        );
        for usage in [json!({}), json!({"output": true}), json!({"output": -1})] {
            assert!(rate(Some(&usage), Some(&json!(2))).is_null());
        }
        assert!(rate(Some(&json!({"output": 40})), Some(&json!(0))).is_null());
    }
    #[test]
    fn overlapping_calls_and_retries_use_the_whole_run_duration() {
        let run = Run::new(None, None, None).unwrap();
        for status in ["error", "ok"] {
            run.reserve().unwrap();
            let record = json!({"status": status, "seconds": 2.0,
                "usage": {"input": 10, "cache_read": 20, "cache_write": 0, "output": 20}});
            run.record(record.as_object().unwrap(), None, None).unwrap();
        }
        let s = run.summary_elapsed(2.5);
        assert_eq!(s["output_tokens_per_second"], json!(16.0));
        assert_eq!(s["cache_read"], json!(40));
        assert_eq!(s["unfinished_calls"], json!(0));
        assert!(summary_text(&s, "text").contains("100 total tokens"));
        assert!(summary_text(&s, "text").contains("2 calls unpriced"));
    }

    #[test]
    fn incomplete_usage_or_an_unfinished_call_has_unknown_aggregate_rate() {
        let run = Run::new(None, None, None).unwrap();
        assert!(run.summary_elapsed(2.0)["output_tokens_per_second"].is_null());
        run.reserve().unwrap();
        assert_eq!(run.summary_elapsed(2.0)["unfinished_calls"], json!(1));
        assert!(
            summary_text(&run.summary_elapsed(2.0), "text").contains("partial; unfinished call")
        );
        let record = json!({"status": "ok", "usage": {"output": 20}, "seconds": 1.0});
        run.record(record.as_object().unwrap(), None, None).unwrap();
        let s = run.summary_elapsed(2.0);
        assert_eq!(s["unfinished_calls"], json!(0));
        assert_eq!(s["output"], json!(20));
        assert!(s["output_tokens_per_second"].is_null());
        assert!(summary_text(&s, "text").contains("partial; 1 call unreported"));
    }

    #[test]
    fn forced_redirected_progress_finishes_and_detaches_without_escape_codes() {
        let output = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&output);
        let previous = harness::set_stderr(Some(Box::new(move |text| {
            sink.lock().unwrap().push_str(text)
        })));
        let run = Arc::new(Run::new(None, None, None).unwrap());
        {
            let _display = Guard::new(&run, "always");
            run.reserve().unwrap();
            run.record(
                json!({"step": "writer", "attempt": 1, "status": "reserved"})
                    .as_object()
                    .unwrap(),
                None,
                None,
            )
            .unwrap();
            run.record(
                json!({"step": "writer", "attempt": 1, "status": "ok", "seconds": 2.0,
                "usage": {"input": 10, "cache_read": 0, "cache_write": 0, "output": 20}})
                .as_object()
                .unwrap(),
                None,
                None,
            )
            .unwrap();
        }
        let before = output.lock().unwrap().clone();
        run.record(
            json!({"step": "later", "status": "reserved"})
                .as_object()
                .unwrap(),
            None,
            None,
        )
        .unwrap();
        harness::set_stderr(previous);
        assert_eq!(*output.lock().unwrap(), before);
        assert!(before.contains("writer (attempt 1) started"));
        assert!(before.contains("writer ok | 2.00s | 10.0 output tok/s (wall time)"));
        assert!(!before.contains('\u{1b}'));
    }
}

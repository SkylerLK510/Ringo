//! `ringo wait` against `ringo.py wait` on the same state files: identical stdout and exit code.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn run(program: &[&str], folder: &Path, extra: &[&str]) -> (String, i32) {
    let out = Command::new(program[0]).args(&program[1..]).arg("wait").arg(folder).args(extra)
        .env_remove("RINGO_CHILD").output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code().unwrap_or(-1))
}

#[test]
fn wait_prints_and_exits_as_python_does() {
    let home = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = home.join("ringo.py");
    let tmp = std::env::temp_dir().join(format!("ringo-gate-wait-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let dead = { let mut c = Command::new("true").spawn().unwrap(); let id = c.id(); c.wait().unwrap(); id };
    let me = std::process::id();
    let cases: Vec<(&str, String)> = vec![
        ("done", r#"{"version": 1, "status": "done", "pid": 1, "calls": 2, "note": "é ✓", "cost_usd": 0.5}"#.into()),
        ("ready", r#"{"version":1,"status":"ready_for_review","pid":1}"#.into()),
        ("revise", r#"{"status":"needs_revision","error":"x"}"#.into()),
        ("failed", r#"{"status":"failed","error":"JudgeError: boom"}"#.into()),
        ("odd", r#"{"status":"something_else"}"#.into()),
        ("gone", format!(r#"{{"version":1,"status":"running","pid":{dead},"calls":0}}"#)),
        ("live", format!(r#"{{"version":1,"status":"running","pid":{me}}}"#)),
        ("garbage", "not json".into()),
    ];
    for (name, text) in &cases {
        let folder = tmp.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("state.json"), text).unwrap();
    }
    let folders: Vec<_> = cases.iter().map(|(n, _)| tmp.join(n)).chain([tmp.join("missing"), tmp.join("./done/")]).collect();
    for folder in folders {
        let py = run(&["python3", script.to_str().unwrap()], &folder, &["--timeout", "0"]);
        let rs = run(&[env!("CARGO_BIN_EXE_ringo")], &folder, &["--timeout", "0"]);
        assert_eq!(py, rs, "{}", folder.display());
    }
    std::fs::remove_dir_all(&tmp).unwrap();
}

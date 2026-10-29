//! The release gate for the native runtime, end to end: a native release staged by releases.py with this build's
//! binary runs only as that release's own bundled binary, through `ringo.py release preview` or with a
//! RINGO_PINNED that names it; every forged or mismatched RINGO_PINNED is refused before anything runs.
#![cfg(unix)]

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn home() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(program: &Path, args: &[&str], state: &Path, pinned: Option<&serde_json::Value>) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args).env("RINGO_RELEASE_HOME", state).env("RINGO_HOME", home()).env_remove("RINGO_CHILD")
        .env_remove("RINGO_PINNED");
    if let Some(meta) = pinned {
        cmd.env("RINGO_PINNED", meta.to_string());
    }
    cmd.output().unwrap()
}

fn refused(out: &Output, why: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains(&format!("RINGO_PINNED refused: {why}")), "{err}");
    assert!(out.stdout.is_empty());
}

#[test]
fn only_the_staged_releases_own_binary_runs_pinned() {
    let tmp = std::env::temp_dir().join(format!("ringo-gate-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let state = tmp.join("state");
    let built = PathBuf::from(env!("CARGO_BIN_EXE_ringo"));
    // releases.py stages this build as a native release, with a passing gate report naming it
    let stage = format!(
        "import json, sys; sys.path.insert(0, {home:?}); import releases, latency\n\
         from pathlib import Path\n\
         binary = Path({built:?}); report = Path({tmp:?}) / 'gate.json'\n\
         report.write_text(json.dumps({{'version': 1, 'binary_sha256': releases.file_hash(binary),\n\
           'reference_sha256': latency.sha(releases.inventory(Path({home:?}))), 'tasks': ['t'], 'mismatches': []}}))\n\
         print(releases.stage(Path({home:?}), Path({state:?}), native=binary, gate=report))",
        home = home().to_str().unwrap(), built = built.to_str().unwrap(), tmp = tmp.to_str().unwrap(),
        state = state.to_str().unwrap());
    let out = Command::new("python3").arg("-c").arg(&stage).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let bundle = state.canonicalize().unwrap().join("bundles").join(&rid);
    let pinned_binary = bundle.join("bin/ringo");
    let meta = |mode: &str, id: &str, root: &Path| json!({"mode": mode, "id": id, "root": root.to_str().unwrap(),
        "approval": null, "quality_claim": null, "runtime": "native"});

    // the real chain: ringo.py verifies the bundle, then runs its binary, which runs the pipeline natively
    let out = Command::new("python3").arg(home().join("ringo.py"))
        .args(["release", "preview", "--release", &rid, "--", "design", "--help"])
        .env("RINGO_RELEASE_HOME", &state).env_remove("RINGO_CHILD").env_remove("RINGO_PINNED").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: ringo.py design"));
    let ok = run(&pinned_binary, &["design", "--help"], &state, Some(&meta("preview", &rid, &bundle)));
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));

    // forgeries
    refused(&run(&built, &["design", "--help"], &state, Some(&meta("preview", &rid, &bundle))),
            "this program is not the release's bundled binary");
    let link = tmp.join("link");
    std::os::unix::fs::symlink(&bundle, &link).unwrap();
    refused(&run(&pinned_binary, &["design", "--help"], &state, Some(&meta("preview", &rid, &link))),
            "its root is not this release state's bundle");
    let fake = tmp.join("fake/bin");
    std::fs::create_dir_all(&fake).unwrap();
    std::os::unix::fs::symlink(&built, fake.join("ringo")).unwrap();
    refused(&run(&fake.join("ringo"), &["design", "--help"], &state, Some(&meta("preview", &rid, &tmp.join("fake")))),
            "its root is not this release state's bundle");
    refused(&run(&pinned_binary, &["design", "--help"], &state, Some(&meta("preview", &"0".repeat(64), &bundle))),
            "its release is not staged here");
    refused(&run(&pinned_binary, &["design", "--help"], &state, Some(&meta("release", &rid, &bundle))),
            "no active release");
    refused(&run(&pinned_binary, &["design", "--help"], &state, Some(&meta("checkout", &rid, &bundle))),
            "unknown mode");
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&tmp).status();
    std::fs::remove_dir_all(&tmp).unwrap();
}

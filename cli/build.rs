//! Builds in the commit this binary comes from (RINGO_COMMIT) and where its prompts and pipelines live
//! (RINGO_BUILD_HOME), for the shell's updater. The updater builds from a copy without its own history, so it passes
//! both; a build in a checkout reads the commit from git and its home is the checkout. Without git the commit is
//! empty, and such a binary never updates itself.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=RINGO_COMMIT");
    println!("cargo:rerun-if-env-changed=RINGO_BUILD_HOME");
    let commit = std::env::var("RINGO_COMMIT").ok().filter(|c| !c.is_empty()).or_else(|| {
        // the commit moves with HEAD (a detached checkout) or its branch's ref, so either one changing reruns this
        let path = |p: &str| git(&["rev-parse", "--path-format=absolute", "--git-path", p]);
        for p in [path("HEAD"), path("packed-refs"), path("refs/heads")].into_iter().flatten() {
            println!("cargo:rerun-if-changed={p}");
        }
        git(&["rev-parse", "HEAD"])
    });
    println!("cargo:rustc-env=RINGO_COMMIT={}", commit.unwrap_or_default());
    println!("cargo:rustc-env=RINGO_BUILD_HOME={}", std::env::var("RINGO_BUILD_HOME").unwrap_or_default());
}

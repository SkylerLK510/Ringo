//! The shell told to end from outside while a turn works: its terminal closing (SIGHUP) and `kill` (SIGTERM). The
//! real `ringo` runs on a pty against a stand-in agent, with a state folder and a home of its own. And, on the same
//! pty, a reply as the shell's own loop shows it while it is written.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A claude that, given a turn, works for ten seconds without a word (a long tool call), marking each quarter second
/// in `$0.work`, then says it finished. Any other start (its version, its plan) ends at once.
const WORKS_ON: &str = r#"#!/bin/sh
case "$*" in *no-session-persistence*) exit 0 ;; *--input-format*) ;; *) echo '2.1.318 (Claude Code)'; exit 0 ;; esac
head -n 1 > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s-end","apiKeySource":"none"}'
echo '{"type":"assistant","message":{"id":"m1","content":[],"usage":{"input_tokens":1200,"output_tokens":40}},"parent_tool_use_id":null}'
i=0; while [ $i -lt 40 ]; do echo tick >> "$0.work"; sleep 0.25; i=$((i+1)); done
echo FINISHED >> "$0.work"
echo '{"type":"result","subtype":"success","is_error":false,"result":"built"}'
"#;

/// A claude that writes its reply in pieces, stopping in the middle of a word and of a table, and marks where it is
/// in `$0.work`.
const WRITES: &str = r#"#!/bin/sh
case "$*" in *no-session-persistence*) exit 0 ;; *--input-format*) ;; *) echo '2.1.318 (Claude Code)'; exit 0 ;; esac
head -n 1 > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s-flow","apiKeySource":"none"}'
piece() { printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"%s"}},"parent_tool_use_id":null}\n' "$1"; }
echo start >> "$0.work"; echo start >> "$0.work"
piece 'Counting the quokk'
sleep 1.5
echo resumed >> "$0.work"
piece 'as now.\n\n| Name | Size |\n|---|---|\n| wombat | 1 |\n| num'
sleep 0.7
piece 'bat | 22 |\n\nThat is all of them. '
sleep 0.5
printf '%s\n' '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Counting the quokkas now.\n\n| Name | Size |\n|---|---|\n| wombat | 1 |\n| numbat | 22 |\n\nThat is all of them."}],"usage":{"input_tokens":1200,"output_tokens":40}},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"done"}'
echo FINISHED >> "$0.work"
"#;

/// One shell at a time: a shell started while another test opens its pty would be handed that pty's descriptors.
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What the shell has written to its terminal since the last read (the pty must be read, or the shell waits on it).
fn seen(terminal: &mut fs::File, all: &mut Vec<u8>) {
    let mut buf = [0u8; 65536];
    while let Ok(n) = terminal.read(&mut buf) {
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
}

/// Lets `ms` pass, reading what the shell draws meanwhile: a shell whose terminal is not read stops at a full pty, and
/// then takes every key typed since as one burst.
fn pass(terminal: &mut fs::File, all: &mut Vec<u8>, ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        seen(terminal, all);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn work(dir: &Path) -> String {
    fs::read_to_string(dir.join("bin/claude.work")).unwrap_or_default()
}

/// `ringo` on a pty in a project of its own with a turn of the stand-in claude under way: the shell, the terminal's
/// side of the pty, and the folder that holds its state, home and project.
fn shell_at_work(name: &str, claude: &str) -> (Child, fs::File, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ringo-end-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let (bin, state, project) = (dir.join("bin"), dir.join("state"), dir.join("project"));
    for d in [&bin, &state, &project, &dir.join("home")] {
        fs::create_dir_all(d).unwrap();
    }
    assert!(Command::new("git").arg("-C").arg(&project).args(["init", "-q"]).status().unwrap().success());
    fs::write(state.join("agents.json"), r#"{"agents":["claude"]}"#).unwrap();
    fs::write(bin.join("claude"), claude).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    let (mut terminal, mut shell_side) = (0, 0);
    let mut size = libc::winsize { ws_row: 40, ws_col: 140, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty fills the two descriptors, which this test then owns; the name and settings may be null.
    let (mut terminal, shell_side) = unsafe {
        assert_eq!(libc::openpty(&mut terminal, &mut shell_side, std::ptr::null_mut(), std::ptr::null_mut(), &mut size), 0);
        libc::fcntl(terminal, libc::F_SETFL, libc::O_NONBLOCK);
        // the shell must not be handed the terminal's side: a pty hangs up only when every copy of it is closed
        libc::fcntl(terminal, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(shell_side, libc::F_SETFD, libc::FD_CLOEXEC);
        (fs::File::from_raw_fd(terminal), OwnedFd::from_raw_fd(shell_side))
    };
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ringo"));
    cmd.current_dir(&project).env("PATH", path).env("HOME", dir.join("home")).env("RINGO_RELEASE_HOME", &state)
        .env("TERM", "xterm-256color").env("RINGO_NO_UPDATE_CHECK", "1")
        .env_remove("RINGO_CHILD").env_remove("RINGO_HOME").env_remove("CODEX_HOME")
        .stdin(Stdio::from(shell_side.try_clone().unwrap())).stdout(Stdio::from(shell_side.try_clone().unwrap()))
        .stderr(Stdio::from(shell_side));
    // SAFETY: setsid and ioctl are async-signal-safe. The pty becomes the shell's controlling terminal, so closing
    // the terminal's side hangs the shell up, as closing a terminal window does.
    unsafe {
        cmd.pre_exec(|| match libc::setsid() != -1 && libc::ioctl(0, libc::TIOCSCTTY as _, 0) != -1 {
            true => Ok(()),
            false => Err(std::io::Error::last_os_error()),
        });
    }
    let shell = cmd.spawn().unwrap();
    drop(cmd); // its copies of the shell's side of the pty
    // The first frame is drawn before a key is typed: it ends by showing the cursor in the input. Keys typed while
    // the shell is still asking the terminal what it can do (up to two seconds, unanswered here) would all arrive
    // at once, and an Enter right behind another key is a paste's, not a line being sent.
    let (mut all, deadline) = (Vec::new(), Instant::now() + Duration::from_secs(30));
    while !all.windows(6).any(|w| w == b"\x1b[?25h") {
        assert!(Instant::now() < deadline, "the shell did not open: {}", String::from_utf8_lossy(&all));
        seen(&mut terminal, &mut all);
        std::thread::sleep(Duration::from_millis(20));
    }
    pass(&mut terminal, &mut all, 300);
    for key in "@claude build it".bytes() {
        terminal.write_all(&[key]).unwrap();
        pass(&mut terminal, &mut all, 10);
    }
    pass(&mut terminal, &mut all, 300);
    terminal.write_all(b"\r").unwrap();
    while work(&dir).lines().count() < 2 {
        assert!(Instant::now() < deadline, "the turn did not start: {}", String::from_utf8_lossy(&all));
        pass(&mut terminal, &mut all, 20);
    }
    (shell, terminal, dir)
}

/// The shell is gone within ten seconds, the turn stopped with it, counted, and kept for `ringo --resume`.
fn ended_as_quit_does(shell: &mut Child, dir: &Path, mut each: impl FnMut()) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while shell.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = shell.kill(); // not left behind by the failure
            panic!("the shell is still there ten seconds after it was told to end");
        }
        each();
        std::thread::sleep(Duration::from_millis(20));
    }
    let marks = work(dir);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(work(dir), marks, "the agent's step went on after the shell was gone");
    assert!(!marks.contains("FINISHED"));
    let usage = fs::read_to_string(dir.join("state/usage.jsonl")).unwrap_or_default();
    assert!(usage.lines().count() == 1 && usage.contains("claude"), "the stopped turn is in the usage record: {usage:?}");
    let kept = fs::read_dir(dir.join("state/shell")).unwrap().flatten().filter(|e| e.path().join("resume.json").is_file()).count();
    assert_eq!(kept, 1, "the conversation is kept for ringo --resume");
}

#[test]
fn a_closed_terminal_ends_the_shell_and_the_turn_at_work() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut shell, terminal, dir) = shell_at_work("hup", WORKS_ON);
    drop(terminal); // the terminal window closes
    ended_as_quit_does(&mut shell, &dir, || {});
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_killed_shell_stops_its_turn_and_puts_the_terminal_back() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut shell, mut terminal, dir) = shell_at_work("term", WORKS_ON);
    let mut all = Vec::new();
    seen(&mut terminal, &mut all);
    all.clear();
    // SAFETY: sends a signal to the shell this test started.
    unsafe { libc::kill(shell.id() as libc::pid_t, libc::SIGTERM) };
    ended_as_quit_does(&mut shell, &dir, || seen(&mut terminal, &mut all));
    seen(&mut terminal, &mut all);
    assert!(all.windows(8).any(|w| w == b"\x1b[?1049l"), "the alternate screen is left: {}", String::from_utf8_lossy(&all));
    assert!(terminal.as_raw_fd() >= 0);
    let _ = fs::remove_dir_all(&dir);
}

/// The loop itself shows what a reply being written kept back: its last word once the pieces pause, and a table
/// only when it is whole, never as rows of bars.
#[test]
fn a_reply_shows_its_last_word_at_a_pause_and_a_table_once_it_is_whole() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut shell, mut terminal, dir) = shell_at_work("flow", WRITES);
    let has = |all: &[u8], text: &str| all.windows(text.len()).any(|w| w == text.as_bytes());
    let (mut all, deadline) = (Vec::new(), Instant::now() + Duration::from_secs(20));
    // the word the pieces stopped in shows during the pause, before the next piece comes
    while !has(&all, "quokk") {
        assert!(!work(&dir).contains("resumed"), "the word waited for the next piece: {}", String::from_utf8_lossy(&all));
        assert!(Instant::now() < deadline, "the reply never showed: {}", String::from_utf8_lossy(&all));
        pass(&mut terminal, &mut all, 20);
    }
    while !work(&dir).contains("FINISHED") {
        assert!(Instant::now() < deadline, "the turn did not finish: {}", String::from_utf8_lossy(&all));
        pass(&mut terminal, &mut all, 20);
    }
    pass(&mut terminal, &mut all, 400);
    assert!(has(&all, "a table is being written"), "one line stood in for the table as it came");
    // (single words: the terminal is sent only the cells that changed, and a space that stayed blank is skipped)
    assert!(has(&all, "wombat") && has(&all, "numbat") && has(&all, "them."), "and then all of it showed");
    assert!(!has(&all, "| Name") && !has(&all, "|---"), "its rows never showed as bars: {}", String::from_utf8_lossy(&all));
    // SAFETY: sends a signal to the shell this test started.
    unsafe { libc::kill(shell.id() as libc::pid_t, libc::SIGTERM) };
    let gone = Instant::now() + Duration::from_secs(10);
    while shell.try_wait().unwrap().is_none() {
        if Instant::now() >= gone {
            let _ = shell.kill();
            panic!("the shell is still there ten seconds after it was told to end");
        }
        pass(&mut terminal, &mut all, 20);
    }
    let _ = fs::remove_dir_all(&dir);
}

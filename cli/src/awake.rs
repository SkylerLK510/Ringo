//! Keep the computer awake while a paid model call is running.
//!
//! A laptop that sleeps in the middle of a turn drops the backend's connection, and the turn then fails with an
//! error that looks like the backend's fault. T3 Code holds a sleep inhibitor while its agents work (its
//! `background/KeepAwake.ts`); this is the same idea, narrower: the hold is kept only while at least one job has a
//! running backend process and none of the running jobs is waiting on the owner (a pending question or approval is
//! the owner's time, not the machine's), and it is released 60 seconds after the last such job ends, so two turns
//! back to back do not flap the hold on and off.
//!
//! How the hold is made, per host (a missing tool or a failed spawn is one log line, never an error):
//! - WSL: `powershell.exe` calls `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` and then blocks reading
//!   its stdin. logind inside the WSL VM cannot hold off Windows sleep, and Windows cannot wait on a Linux pid, so the
//!   hold ends when stdin closes, which also happens if Ringo dies.
//! - Linux: `systemd-inhibit --what=sleep --mode=block cat`, held the same way through stdin.
//! - macOS: `caffeinate -i -w <ringo pid>`.
//! - `RINGO_KEEP_AWAKE=0` turns it off.
//!
//! The state machine (`KeepAwake`) takes its spawner and its clock as arguments, so the tests drive it with a fake
//! spawner and made-up instants: no real powershell, no real wait.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the hold outlives the last running call (T3's RELEASE_GRACE, also 60 s).
pub const RELEASE_GRACE: Duration = Duration::from_secs(60);

/// A computer sleeping this much longer by the wall clock than by the monotonic clock is reported as having slept.
pub const SLEPT_MIN_SECS: u64 = 60;

/// ES_CONTINUOUS | ES_SYSTEM_REQUIRED (0x80000001), held by the calling thread until the script ends.
const WINDOWS_HOLD_SCRIPT: &str = "$t = Add-Type -Name Power -Namespace Ringo -PassThru -MemberDefinition \
    '[DllImport(\"kernel32.dll\")] public static extern uint SetThreadExecutionState(uint f);'; \
    [void]$t::SetThreadExecutionState([uint32]2147483649); [void][Console]::In.ReadToEnd()";

/// A live hold: dropping it releases the inhibitor.
pub trait Hold {
    /// The tool's name once its process has exited on its own (so nothing is held any more); None while it runs.
    fn exited(&mut self) -> Option<String> {
        None
    }
}

/// Makes holds. `Err` is the one log line to give; the hold is then not tried again this session.
pub trait Spawner {
    fn spawn(&mut self) -> Result<Box<dyn Hold>, String>;
}

/// The hold/release state machine.
pub struct KeepAwake<S: Spawner> {
    spawner: S,
    hold: Option<Box<dyn Hold>>,
    /// When the last wanted tick was seen, once nothing is wanted any more (None while wanted or not held).
    idle_since: Option<Instant>,
    /// The spawner failed once: no more tries, no more lines.
    gave_up: bool,
    grace: Duration,
}

impl<S: Spawner> KeepAwake<S> {
    pub fn new(spawner: S) -> Self {
        KeepAwake { spawner, hold: None, idle_since: None, gave_up: false, grace: RELEASE_GRACE }
    }

    pub fn held(&self) -> bool {
        self.hold.is_some()
    }

    /// One look at whether a call is running (`wanted`) at `now`. Returns a log line when something happened worth
    /// saying (the hold failed; the hold ended is not worth a line).
    pub fn tick(&mut self, wanted: bool, now: Instant) -> Option<String> {
        // A helper that died by itself (powershell killed, systemd-inhibit refused) holds nothing, whatever the hold
        // says: look at it each tick, say so once, and do not start another this session (no respawn loop).
        if let Some(tool) = self.hold.as_mut().and_then(|h| h.exited()) {
            self.hold = None;
            self.idle_since = None;
            self.gave_up = true;
            return Some(format!("could not keep the computer awake: {tool} exited"));
        }
        if wanted {
            self.idle_since = None;
            if self.hold.is_none() && !self.gave_up {
                match self.spawner.spawn() {
                    Ok(hold) => self.hold = Some(hold),
                    Err(why) => {
                        self.gave_up = true;
                        return Some(format!("cannot keep the computer awake during model calls: {why}"));
                    }
                }
            }
        } else if self.hold.is_some() {
            let since = *self.idle_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.grace {
                self.hold = None; // dropped: released
                self.idle_since = None;
            }
        }
        None
    }
}

/// A real inhibitor process; dropping it closes its stdin (the release for powershell and cat) and then kills it
/// (the release for caffeinate, which has no stdin to wait on).
struct Process(Child, String);

impl Hold for Process {
    fn exited(&mut self) -> Option<String> {
        matches!(self.0.try_wait(), Ok(Some(_)) | Err(_)).then(|| self.1.clone())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The host's real spawner.
pub struct System {
    wsl: bool,
}

impl System {
    pub fn new(wsl: bool) -> Self {
        System { wsl }
    }
}

/// The program and arguments that hold the machine awake on this host, and whether they read stdin to know when to
/// stop; None where there is no way (an unknown platform).
fn command(wsl: bool, os: &str, pid: u32) -> Option<(String, Vec<String>, bool)> {
    let own = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    if wsl {
        return Some(("powershell.exe".into(), own(&["-NoProfile", "-NonInteractive", "-Command", WINDOWS_HOLD_SCRIPT]), true));
    }
    match os {
        "macos" => Some(("caffeinate".into(), vec!["-i".into(), "-w".into(), pid.to_string()], false)),
        "linux" => Some(("systemd-inhibit".into(), own(&["--what=sleep", "--who=Ringo", "--why=Ringo: model call running",
                                                          "--mode=block", "cat"]), true)),
        _ => None,
    }
}

impl Spawner for System {
    fn spawn(&mut self) -> Result<Box<dyn Hold>, String> {
        let (program, args, stdin) = command(self.wsl, std::env::consts::OS, std::process::id())
            .ok_or_else(|| format!("no sleep inhibitor known for {}", std::env::consts::OS))?;
        if !crate::installed(std::path::Path::new(&program)) {
            return Err(format!("{program} is not installed"));
        }
        let child = Command::new(&program).args(&args)
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().map_err(|e| format!("{program}: {e}"))?;
        Ok(Box::new(Process(child, program)))
    }
}

/// Whether the owner turned the hold off (`RINGO_KEEP_AWAKE=0`).
pub fn disabled(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("0" | "false" | "off" | "no"))
}

/// How long the computer slept during a turn, in seconds, when it slept for more than `SLEPT_MIN_SECS`: the wall clock
/// kept running through the sleep and the monotonic clock (CLOCK_MONOTONIC on Linux) did not.
pub fn slept_secs(monotonic: Duration, wall: Duration) -> Option<u64> {
    let extra = wall.checked_sub(monotonic)?.as_secs();
    (extra > SLEPT_MIN_SECS).then_some(extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Log {
        spawned: usize,
        released: usize,
        dead: bool,
    }

    struct Fake {
        log: Rc<RefCell<Log>>,
        fail: bool,
    }

    struct Live(Rc<RefCell<Log>>);

    impl Hold for Live {
        fn exited(&mut self) -> Option<String> {
            // the fake helper dies when the test says so
            self.0.borrow().dead.then(|| "powershell.exe".to_string())
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.0.borrow_mut().released += 1;
        }
    }

    impl Spawner for Fake {
        fn spawn(&mut self) -> Result<Box<dyn Hold>, String> {
            if self.fail {
                return Err("powershell.exe is not installed".into());
            }
            self.log.borrow_mut().spawned += 1;
            Ok(Box::new(Live(self.log.clone())))
        }
    }

    fn machine(fail: bool) -> (KeepAwake<Fake>, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        (KeepAwake::new(Fake { log: log.clone(), fail }), log)
    }

    #[test]
    fn holds_while_wanted_and_releases_only_after_the_grace() {
        let (mut k, log) = machine(false);
        let t0 = Instant::now();
        assert!(!k.held());
        k.tick(false, t0);
        assert_eq!(log.borrow().spawned, 0, "nothing running: no hold");
        k.tick(true, t0);
        k.tick(true, t0 + Duration::from_secs(5));
        assert!(k.held());
        assert_eq!(log.borrow().spawned, 1, "one hold for any number of ticks");
        // the call ends: still held through the grace
        k.tick(false, t0 + Duration::from_secs(10));
        k.tick(false, t0 + Duration::from_secs(69));
        assert!(k.held() && log.borrow().released == 0);
        k.tick(false, t0 + Duration::from_secs(70));
        assert!(!k.held());
        assert_eq!((log.borrow().spawned, log.borrow().released), (1, 1));
    }

    #[test]
    fn a_turn_inside_the_grace_keeps_the_same_hold_and_restarts_the_grace() {
        let (mut k, log) = machine(false);
        let t0 = Instant::now();
        k.tick(true, t0);
        k.tick(false, t0 + Duration::from_secs(1));
        k.tick(true, t0 + Duration::from_secs(50)); // back to back
        k.tick(false, t0 + Duration::from_secs(100)); // the grace starts again here, not at second 1
        k.tick(false, t0 + Duration::from_secs(150));
        assert!(k.held(), "50 s into the second grace");
        k.tick(false, t0 + Duration::from_secs(160));
        assert!(!k.held());
        assert_eq!((log.borrow().spawned, log.borrow().released), (1, 1));
        k.tick(true, t0 + Duration::from_secs(200));
        assert_eq!(log.borrow().spawned, 2, "a new turn after the release holds again");
    }

    #[test]
    fn a_helper_that_dies_is_one_log_line_and_not_respawned() {
        let (mut k, log) = machine(false);
        let t0 = Instant::now();
        assert_eq!(k.tick(true, t0), None);
        assert!(k.held());
        log.borrow_mut().dead = true; // the helper exits on its own while a call is wanted
        let line = k.tick(true, t0 + Duration::from_secs(1)).expect("one line");
        assert_eq!(line, "could not keep the computer awake: powershell.exe exited");
        assert!(!k.held());
        assert_eq!(log.borrow().released, 1, "the dead hold is dropped");
        // no respawn loop, no second line
        assert_eq!(k.tick(true, t0 + Duration::from_secs(2)), None);
        assert_eq!(k.tick(true, t0 + Duration::from_secs(90)), None);
        assert_eq!(log.borrow().spawned, 1);
    }

    #[test]
    fn a_failed_spawn_is_one_log_line_and_never_retried() {
        let (mut k, _) = machine(true);
        let t0 = Instant::now();
        let line = k.tick(true, t0).expect("one line");
        assert!(line.contains("cannot keep the computer awake") && line.contains("not installed"), "{line}");
        assert_eq!(k.tick(true, t0 + Duration::from_secs(1)), None);
        assert!(!k.held());
    }

    #[test]
    fn the_commands_per_host() {
        let (p, a, stdin) = command(true, "linux", 7).unwrap();
        assert_eq!(p, "powershell.exe");
        assert_eq!(&a[..3], ["-NoProfile", "-NonInteractive", "-Command"]);
        assert!(stdin && a[3].contains("SetThreadExecutionState") && a[3].contains("2147483649") && a[3].contains("ReadToEnd"));
        let (p, a, stdin) = command(false, "linux", 7).unwrap();
        assert_eq!((p.as_str(), stdin), ("systemd-inhibit", true));
        assert!(a.contains(&"--what=sleep".to_string()) && a.contains(&"--mode=block".to_string()) && a.last().unwrap() == "cat");
        let (p, a, stdin) = command(false, "macos", 42).unwrap();
        assert_eq!((p.as_str(), a, stdin), ("caffeinate", vec!["-i".to_string(), "-w".into(), "42".into()], false));
        assert!(command(false, "windows", 1).is_none());
    }

    #[test]
    fn the_owner_can_opt_out() {
        assert!(disabled(Some("0")) && disabled(Some(" off ")));
        assert!(!disabled(None) && !disabled(Some("1")) && !disabled(Some("")));
    }

    #[test]
    fn host_sleep_is_the_wall_clock_running_ahead_of_the_monotonic_one() {
        let s = Duration::from_secs;
        assert_eq!(slept_secs(s(100), s(100)), None);
        assert_eq!(slept_secs(s(100), s(160)), None, "exactly 60 s is not over 60");
        assert_eq!(slept_secs(s(100), s(1900)), Some(1800));
        assert_eq!(slept_secs(s(100), s(50)), None, "a clock set back is not sleep");
    }
}

//! Explicit, bounded, zero-inference task checks. A durable launch claim precedes task attachment and spawn.
//! A claim is never reused to execute, even after a crash. An unfinished record without a live handle is unknown.
use crate::task::{ArtifactRef, RepositoryBinding, Task, TaskStore, TaskUpdate};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, io::{Read, Write}, path::{Path, PathBuf}, process::{Command, Stdio},
          sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, Instant}};

type Result<T> = std::result::Result<T, String>;
const OUTPUT_LIMIT: usize = 512 * 1024;
static LAUNCH: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct Handle { cancelled: AtomicBool, launch: Mutex<()> }
static LIVE: OnceLock<Mutex<HashMap<PathBuf, Arc<Handle>>>> = OnceLock::new();
fn live() -> &'static Mutex<HashMap<PathBuf, Arc<Handle>>> { LIVE.get_or_init(Default::default) }
impl Handle {
    fn cancel(&self) { let _guard = self.launch.lock().unwrap_or_else(|e| e.into_inner()); self.cancelled.store(true, Ordering::SeqCst); }
}

fn digest(bytes: &[u8]) -> String { crate::task_check::hex(&Sha256::digest(bytes)) }
fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source { pub head: String, pub tree: String }
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Output { pub bytes: u64, pub truncated: bool, pub error: Option<String> }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub schema_version: u32,
    pub id: String,
    pub task_id: String,
    pub repository: RepositoryBinding,
    pub task_revision: u64,
    pub command: String,
    pub seconds: u64,
    pub source: Source,
    pub completed_source: Option<Source>,
    pub created_unix: u64,
    pub finished_unix: Option<u64>,
    pub outcome: String,
    pub exit_code: Option<i32>,
    pub detail: Option<String>,
    pub stdout: Output,
    pub stderr: Output,
}

#[derive(Clone)]
pub struct CheckStore { root: PathBuf, repository: RepositoryBinding }
impl CheckStore {
    pub fn new(root: &Path, repository: &RepositoryBinding) -> Result<Self> {
        // Reuse task storage's private-state/repository checks.
        TaskStore::new(root, repository.path.clone(), repository.git).map_err(|e| e.to_string())?;
        Ok(Self { root: root.into(), repository: repository.clone() })
    }
    pub fn directory(&self, task: &Task, id: &str) -> Result<PathBuf> {
        if task.repository != self.repository || !valid_id(id) {
            return Err("invalid check identity or repository binding".into());
        }
        // Validate the task ID with the task store before constructing a private path.
        TaskStore::new(&self.root, self.repository.path.clone(), self.repository.git)
            .map_err(|e| e.to_string())?.load(&task.id).map_err(|e| e.to_string())?;
        Ok(self.root.join("task-checks").join(digest(self.repository.path.as_os_str().as_encoded_bytes()))
            .join(&task.id).join(id))
    }
    pub fn load(&self, task: &Task, id: &str) -> Result<Check> {
        let dir = self.directory(task, id)?;
        let mut file = crate::evidence::open_regular_beneath_strict(&dir, Path::new("record.json"))
            .map_err(|e| format!("check record unavailable: {e}"))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(65537).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if bytes.len() > 65536 { return Err("check record exceeds its limit".into()); }
        let check: Check = serde_json::from_slice(&bytes).map_err(|e| format!("invalid check record: {e}"))?;
        if check.schema_version != 1 || check.task_id != task.id || check.repository != task.repository || check.id != id {
            return Err("check record identity mismatch".into());
        }
        Ok(check)
    }
    /// The caller supplies the observed task revision. Exclusive directory creation claims execution; a receipt
    /// protects the task mutation. No failure/crash path releases the execution claim for reuse.
    pub fn run(&self, task: &Task, request: &str, seconds: u64, command: &str) -> Result<Check> {
        if !cfg!(unix) || !self.repository.git {
            return Err("task checks currently require a Unix Git repository; no command was started".into());
        }
        if request.is_empty() || request.len() > 128 || !request.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || !(1..=3600).contains(&seconds) || command.trim().is_empty() || command.len() > 16384 || command.contains('\0') {
            return Err("check needs REQUEST_ID (letters, digits, - or _), SECONDS (1..3600), and COMMAND (at most 16 KiB)".into());
        }
        if serde_json::to_vec(command).map_err(|e| e.to_string())?.len() > 32768 {
            return Err("JSON-escaped check command exceeds 32 KiB; no command was started".into());
        }
        let id = format!("c-{}", digest(request.as_bytes()));
        let dir = self.directory(task, &id)?;
        if dir.exists() {
            let check = self.load(task, &id)?;
            if check.command != command || check.seconds != seconds { return Err("check request was already used with different arguments".into()); }
            return Ok(check);
        }
        let store = TaskStore::new(&self.root, self.repository.path.clone(), self.repository.git).map_err(|e| e.to_string())?;
        let receipt = format!("check-{}", digest(request.as_bytes()));
        let latest = store.load(&task.id).map_err(|e| e.to_string())?;
        if latest.requests.iter().any(|r| r.request_id == receipt) {
            return Err("check was previously reserved but its record is missing; use a new request ID only after inspecting the previous execution".into());
        }
        let _launch = LAUNCH.try_lock().map_err(|_| "another check launch is being reserved; retry")?;
        if live().lock().map_err(|_| "check handle lock failed")?.len() >= 3 {
            return Err("three checks are already running; wait or cancel one".into());
        }
        let source = Source { head: "pending".into(), tree: "pending".into() };
        private_tree(dir.parent().unwrap())?;
        fs::create_dir(&dir).map_err(|e| format!("check launch already claimed or unavailable; retry to inspect it: {e}"))?;
        private_tree(&dir)?;
        let mut check = Check { schema_version: 1, id, task_id: task.id.clone(), repository: self.repository.clone(),
            task_revision: task.revision, command: command.into(), seconds, source, completed_source: None, created_unix: now(),
            finished_unix: None, outcome: "reserved".into(), exit_code: None, detail: None,
            stdout: Output::default(), stderr: Output::default() };
        save(&dir, &check)?;
        let identity = command_identity(command, seconds);
        if let Err(error) = store.update(&task.id, task.revision, &receipt,
            TaskUpdate::AddArtifact(ArtifactRef { reference: format!("check:{}", check.id), identity: Some(identity) })) {
            check.outcome = "refused".into(); check.detail = Some(error.to_string()); check.finished_unix = Some(now());
            save(&dir, &check)?;
            return Err(error.to_string());
        }
        let cancel = Arc::new(Handle::default());
        check.outcome = "running".into();
        save(&dir, &check)?;
        live().lock().map_err(|_| "check handle lock failed")?.insert(dir.clone(), cancel.clone());
        let result = check.clone();
        let worker_dir = dir.clone();
        if let Err(error) = thread::Builder::new().name("task-check".into()).spawn(move || {
            match crate::task_check::source(&check.repository.path) {
                Ok(identity) => {
                    check.source = identity;
                    execute(&worker_dir, &mut check, cancel);
                }
                Err(error) => { check.outcome = "refused".into(); check.detail = Some(error); check.finished_unix = Some(now()); }
            }
            check.completed_source = crate::task_check::source(&check.repository.path).ok();
            let _ = save(&worker_dir, &check);
            live().lock().unwrap_or_else(|e| e.into_inner()).remove(&worker_dir);
        }) {
            live().lock().unwrap_or_else(|e| e.into_inner()).remove(&dir);
            let mut failed = result.clone(); failed.outcome = "failed".into(); failed.detail = Some(error.to_string());
            failed.finished_unix = Some(now()); save(&dir, &failed)?;
            return Err(format!("could not start check supervisor: {error}"));
        }
        Ok(result)
    }
    pub fn cancel(&self, task: &Task, id: &str) -> Result<()> {
        let dir = self.directory(task, id)?;
        let handles = live().lock().map_err(|_| "check handle lock failed")?;
        let cancel = handles.get(&dir).ok_or("no live check handle here; no process was signalled (inspect progress)")?;
        cancel.cancel();
        Ok(())
    }
    pub fn running(&self) -> usize {
        let prefix = self.root.join("task-checks").join(digest(self.repository.path.as_os_str().as_encoded_bytes()));
        live().lock().unwrap_or_else(|e| e.into_inner()).keys().filter(|p| p.starts_with(&prefix)).count()
    }
    pub fn shutdown(&self) {
        let prefix = self.root.join("task-checks").join(digest(self.repository.path.as_os_str().as_encoded_bytes()));
        for (path, handle) in live().lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if path.starts_with(&prefix) { handle.cancel(); }
        }
        let until = Instant::now() + Duration::from_secs(12);
        while self.running() > 0 && Instant::now() < until { thread::sleep(Duration::from_millis(20)); }
    }
    pub fn status(&self, task: &Task, check: &Check) -> String {
        let active = self.directory(task, &check.id).ok().is_some_and(|p|
            live().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&p));
        if matches!(check.outcome.as_str(), "running" | "reserved") && !active {
            "unknown (unfinished record; no live handle; never automatically rerun)".into()
        } else { check.outcome.clone() }
    }
    pub fn progress(&self, task: &Task) -> Vec<String> {
        let current = if task.artifacts.iter().any(|a| a.reference.starts_with("check:")) {
            sample_async(&self.repository.path)
        } else { Err("no checks".into()) };
        let mut lines = Vec::new();
        for artifact in &task.artifacts {
            let Some(id) = artifact.reference.strip_prefix("check:") else { continue };
            let check = match self.load(task, id) {
                Ok(check) => check,
                Err(error) => { lines.push(format!("    {id}: unavailable ({error})")); continue; }
            };
            let dir = match self.directory(task, id) { Ok(dir) => dir, Err(_) => continue };
            let state = self.status(task, &check);
            let freshness = match &current {
                Ok(identity) if identity == &check.source &&
                    (check.finished_unix.is_none() || check.completed_source.as_ref() == Some(identity)) => "source samples match (not continuous attestation)",
                Ok(_) if check.finished_unix.is_some() && check.completed_source.is_none() => "completion source identity unknown",
                Ok(_) => "STALE: source samples differ",
                Err(_) => "source identity unknown",
            };
            lines.push(format!("    {id}: {state} · {freshness} · zero model calls · {}", check.command));
            for (name, output) in [("stdout", &check.stdout), ("stderr", &check.stderr)] {
                let available = crate::evidence::open_regular_beneath_strict(&dir, Path::new(name)).is_ok();
                lines.push(format!("      {name}: {} · {} bytes observed{}{} · {}", if available { "present" } else { "missing" },
                    output.bytes, if output.truncated { " · truncated" } else { "" },
                    output.error.as_ref().map(|e| format!(" · error: {e}")).unwrap_or_default(), dir.join(name).display()));
            }
            if let Some(detail) = check.detail { lines.push(format!("      {detail}")); }
        }
        if lines.is_empty() { lines.push("    none recorded (/task check run REQUEST_ID SECONDS COMMAND)".into()); }
        lines
    }
}
pub fn command_identity(command: &str, seconds: u64) -> String {
    format!("check-command-sha256:{}", digest(&serde_json::to_vec(&(command, seconds)).expect("command serializes")))
}
fn sample_async(repo: &Path) -> Result<Source> {
    type Samples = HashMap<PathBuf, Option<Result<Source>>>;
    static SAMPLES: OnceLock<Arc<Mutex<Samples>>> = OnceLock::new();
    let samples = SAMPLES.get_or_init(Default::default).clone();
    let mut entries = samples.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = entries.get_mut(repo) {
        if let Some(result) = entry.take() { entries.remove(repo); return result; }
        return Err("source sampling in progress".into());
    }
    entries.insert(repo.into(), None);
    let path = repo.to_path_buf();
    drop(entries);
    let worker_samples = samples.clone();
    if let Err(error) = thread::Builder::new().name("check-source".into()).spawn(move || {
        let result = source(&path);
        worker_samples.lock().unwrap_or_else(|e| e.into_inner()).insert(path, Some(result));
    }) {
        samples.lock().unwrap_or_else(|e| e.into_inner()).remove(repo);
        return Err(error.to_string());
    }
    Err("source sampling in progress; reopen progress for latest sample".into())
}
#[cfg(unix)]
fn git_bounded(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut child = Command::new("git").args(args).current_dir(repo).stdin(Stdio::null())
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let result = (|| {
        let mut pipe = child.stdout.take().unwrap();
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 { return Err("Git pipe configuration failed".into()); }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new(); let mut buffer = [0; 8192];
        loop {
            if Instant::now() >= deadline { return Err("Git source enumeration timed out".into()); }
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => { if output.len() + n > 4 * 1024 * 1024 { return Err("Git source enumeration exceeds 4 MiB".into()); } output.extend_from_slice(&buffer[..n]); }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {},
                Err(e) => return Err(e.to_string()),
            }
        }
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() { Ok(output) } else { Err("Git source enumeration failed".into()) };
            }
            if Instant::now() >= deadline { return Err("Git source enumeration timed out".into()); }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() { let _ = child.kill(); let _ = child.wait(); }
    result
}
#[cfg(not(unix))]
fn git_bounded(_: &Path, _: &[&str]) -> Result<Vec<u8>> { Err("task checks require Unix".into()) }
pub fn valid_id(id: &str) -> bool {
    id.len() == 66 && id.starts_with("c-") && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn private_tree(path: &Path) -> Result<()> {
    // Do not follow state-directory symlinks. Set private permissions on all newly introduced parents.
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) { return Err("check state directory is a symlink".into()); }
    if let Some(parent) = path.parent() {
        if !parent.exists() { private_tree(parent)?; }
        for ancestor in parent.ancestors() {
            if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err("check state parent is a symlink".into());
            }
        }
    }
    crate::private_dir(path)
}
fn save(dir: &Path, check: &Check) -> Result<()> {
    let temp = dir.join("record.tmp");
    let mut opts = fs::OpenOptions::new(); opts.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; opts.mode(0o600); }
    let mut file = opts.open(&temp).map_err(|e| e.to_string())?;
    let result = (|| {
        let bytes = serde_json::to_vec(check).map_err(|e| e.to_string())?;
        if bytes.len() > 65536 { return Err("check record exceeds 64 KiB".into()); }
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, dir.join("record.json")).map_err(|e| e.to_string())
    })();
    if result.is_err() { let _ = fs::remove_file(temp); }
    result
}
/// Git HEAD, staged index entries, tracked and non-ignored untracked file contents. Ignored files, services and
/// external inputs are deliberately outside this identity. Oversize/unreadable trees are refused, not guessed.
pub fn source(repo: &Path) -> Result<Source> {
    let head = String::from_utf8(git_bounded(repo, &["rev-parse", "HEAD"])?).map_err(|e| e.to_string())?.trim().to_string();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut hash = Sha256::new();
    hash.update(git_bounded(repo, &["ls-files", "--stage", "-z"])?);
    let names = git_bounded(repo, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
    let mut remaining = 128 * 1024 * 1024u64;
    for (i, name) in names.split(|b| *b == 0).filter(|s| !s.is_empty()).enumerate() {
        if Instant::now() > deadline { return Err("source sampling timed out".into()); }
        if i >= 10000 { return Err("source identity exceeds 10000 files".into()); }
        #[cfg(unix)] let path = { use std::os::unix::ffi::OsStrExt; repo.join(std::ffi::OsStr::from_bytes(name)) };
        #[cfg(not(unix))] let path = repo.join(std::str::from_utf8(name).map_err(|e| e.to_string())?);
        hash.update((name.len() as u64).to_le_bytes()); hash.update(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => { hash.update(b"missing"); continue; }
            Err(e) => return Err(e.to_string()),
        };
        if metadata.file_type().is_symlink() {
            hash.update(b"symlink"); hash.update(fs::read_link(&path).map_err(|e| e.to_string())?.as_os_str().as_encoded_bytes());
        } else if metadata.is_file() {
            let mut bytes = Vec::new();
            // Strict beneath-root opens refuse symlinked parent directories and special files.
            let relative = path.strip_prefix(repo).map_err(|e| e.to_string())?;
            crate::evidence::open_regular_beneath_strict(repo, relative).map_err(|e| e.to_string())?
                .take(remaining + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if bytes.len() as u64 > remaining { return Err("source identity exceeds 128 MiB".into()); }
            remaining -= bytes.len() as u64;
            hash.update(b"file"); hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes);
            #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; hash.update((metadata.permissions().mode() & 0o111).to_le_bytes()); }
        } else { return Err("source includes an unsupported directory/submodule or special file".into()); }
    }
    Ok(Source { head, tree: hex(&hash.finalize()) })
}

#[cfg(unix)]
fn execute(dir: &Path, check: &mut Check, cancel: Arc<Handle>) {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &check.command]).current_dir(&check.repository.path)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let launch = cancel.launch.lock().unwrap_or_else(|e| e.into_inner());
    if cancel.cancelled.load(Ordering::SeqCst) {
        check.outcome = "cancelled".into(); check.finished_unix = Some(now()); return;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => { check.outcome = "failed".into(); check.detail = Some(e.to_string()); check.finished_unix = Some(now()); return; }
    };
    drop(launch);
    let done = Arc::new(AtomicBool::new(false));
    let stdout = capture(child.stdout.take().unwrap(), dir.join("stdout"), done.clone());
    let stderr = capture(child.stderr.take().unwrap(), dir.join("stderr"), done.clone());
    let start = Instant::now();
    loop {
        let cancelled = cancel.cancelled.load(Ordering::SeqCst);
        let timeout = start.elapsed() >= Duration::from_secs(check.seconds);
        if cancelled || timeout {
            // Signal only the unreaped child group we own, never a PID recovered from disk.
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL); }
            let _ = child.wait();
            check.outcome = if cancelled { "cancelled" } else { "timed_out" }.into();
            break;
        }
        // Observe without reaping: the PID/group cannot be reused before descendant cleanup.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe { libc::waitid(libc::P_PID, child.id(), &mut info,
                                           libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        if waited == 0 && unsafe { info.si_pid() } == 0 {
            thread::sleep(Duration::from_millis(20)); continue;
        }
        match if waited == 0 {
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL); }
            child.wait()
        } else { Err(std::io::Error::last_os_error()) } {
            Ok(status) => {
                check.exit_code = status.code();
                check.outcome = if status.success() { "passed" } else { "failed" }.into();
                break;
            }
            Err(e) => {
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL); }
                let _ = child.wait(); check.outcome = "failed".into(); check.detail = Some(e.to_string()); break;
            }
        }
    }
    done.store(true, Ordering::SeqCst);
    check.stdout = stdout.join().unwrap_or_else(|_| Output { error: Some("stdout capture panicked".into()), ..Output::default() });
    check.stderr = stderr.join().unwrap_or_else(|_| Output { error: Some("stderr capture panicked".into()), ..Output::default() });
    check.finished_unix = Some(now());
}
#[cfg(not(unix))]
fn execute(_: &Path, _: &mut Check, _: Arc<Handle>) { unreachable!("launch refused before claim") }

#[cfg(unix)]
fn capture<T: Read + std::os::fd::AsRawFd + Send + 'static>(mut pipe: T, path: PathBuf, done: Arc<AtomicBool>) -> thread::JoinHandle<Output> {
    thread::spawn(move || {
        let mut result = Output::default();
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            result.error = Some("could not make output pipe nonblocking".into()); return result;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
            Ok(file) => Some(file), Err(e) => { result.error = Some(e.to_string()); None }
        };
        let mut kept = 0usize;
        let mut buffer = [0u8; 8192];
        let mut finished = None;
        loop {
            if done.load(Ordering::SeqCst) && finished.is_none() { finished = Some(Instant::now()); }
            if finished.is_some_and(|at: Instant| at.elapsed() > Duration::from_millis(200)) {
                result.truncated = true; result.error.get_or_insert("output pipe stayed open after command ended".into()); break;
            }
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    result.bytes = result.bytes.saturating_add(n as u64);
                    let take = n.min(OUTPUT_LIMIT - kept);
                    if take < n { result.truncated = true; }
                    if let Some(out) = file.as_mut() && let Err(e) = out.write_all(&buffer[..take]) {
                        result.error = Some(e.to_string()); file = None;
                    }
                    kept += take;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {},
                Err(e) => { result.error = Some(e.to_string()); break; }
            }
        }
        if let Some(out) = file && let Err(e) = out.sync_all() { result.error = Some(e.to_string()); }
        result
    })
}

impl crate::Session {
    pub fn check_store(&self) -> Result<CheckStore> {
        CheckStore::new(&self.root, &RepositoryBinding { path: self.repo.clone(), git: self.git })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());
    struct Fixture { base: PathBuf, store: TaskStore, checks: CheckStore, task: Task }
    impl Fixture {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!("ringo-check-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&base); fs::create_dir_all(base.join("repo")).unwrap();
            let base = fs::canonicalize(base).unwrap(); let repo = base.join("repo");
            for args in [vec!["init", "-q"], vec!["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "initial"]] {
                assert!(Command::new("git").args(args).current_dir(&repo).status().unwrap().success());
            }
            let store = TaskStore::new(&base.join("state"), repo.clone(), true).unwrap();
            let task = store.create("owner", "verify changes").unwrap();
            let checks = CheckStore::new(&base.join("state"), &task.repository).unwrap();
            Self { base, store, checks, task }
        }
        fn wait(&self, id: &str) -> Check {
            let until = Instant::now() + Duration::from_secs(8);
            loop {
                let check = self.checks.load(&self.task, id).unwrap();
                if check.finished_unix.is_some() { return check; }
                assert!(Instant::now() < until, "check never finished");
                thread::sleep(Duration::from_millis(20));
            }
        }
        fn latest(&self) -> Task { self.store.load(&self.task.id).unwrap() }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.base); } }
    #[test]
    fn outcomes_duplicate_conflict_and_restart() {
        let _guard = SERIAL.lock().unwrap(); let f = Fixture::new("outcomes");
        let c = f.checks.run(&f.task, "pass", 5, "printf success; printf problem >&2").unwrap();
        assert_eq!(f.wait(&c.id).outcome, "passed");
        let latest = f.latest(); assert_eq!(latest.revision, f.task.revision + 1);
        assert_eq!(latest.objective, f.task.objective); assert_eq!(latest.decisions, f.task.decisions);
        assert_eq!(f.checks.run(&f.task, "pass", 5, &c.command).unwrap().id, c.id);
        assert!(f.checks.run(&latest, "pass", 5, "exit 0").is_err());
        let c2 = f.checks.run(&latest, "fail", 5, "exit 7").unwrap();
        let failed = f.wait(&c2.id); assert_eq!(failed.outcome, "failed"); assert_eq!(failed.exit_code, Some(7));
        let restarted = CheckStore::new(&f.base.join("state"), &f.task.repository).unwrap();
        assert!(restarted.progress(&f.latest()).join("\n").contains("passed"));
        fs::write(f.task.repository.path.join("new.txt"), "changed").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if restarted.progress(&f.latest()).join("\n").contains("STALE") { break; }
            assert!(Instant::now() < until); thread::sleep(Duration::from_millis(20));
        }
        fs::remove_file(f.checks.directory(&f.task, &c.id).unwrap().join("stdout")).unwrap();
        assert!(restarted.progress(&f.latest()).join("\n").contains("stdout: missing"));
        fs::remove_dir_all(f.checks.directory(&f.task, &c.id).unwrap()).unwrap();
        assert!(restarted.run(&f.latest(), "pass", 5, &c.command).unwrap_err().contains("previously reserved"));
    }
    #[test]
    fn cancellation_timeout_and_background_cleanup() {
        let _guard = SERIAL.lock().unwrap(); let f = Fixture::new("cancel");
        let c = f.checks.run(&f.task, "cancel", 5, "sleep 30 & wait").unwrap();
        f.checks.cancel(&f.task, &c.id).unwrap(); assert_eq!(f.wait(&c.id).outcome, "cancelled");
        let c = f.checks.run(&f.latest(), "timeout", 1, "sleep 30 & wait").unwrap();
        assert_eq!(f.wait(&c.id).outcome, "timed_out");
        let c = f.checks.run(&f.latest(), "background", 5, "sleep 30 & printf done").unwrap();
        assert_eq!(f.wait(&c.id).outcome, "passed");
        assert!(f.checks.cancel(&f.task, &c.id).is_err());
    }
    #[test]
    fn prelaunch_cancel_shutdown_and_declared_identity() {
        let _guard = SERIAL.lock().unwrap(); let f = Fixture::new("lifecycle");
        let c = f.checks.run(&f.task, "prepare", 5, "true").unwrap();
        let mut check = f.wait(&c.id);
        let handle = Arc::new(Handle::default()); handle.cancel();
        check.command = "touch NEVER".into();
        execute(&f.checks.directory(&f.task, &c.id).unwrap(), &mut check, handle);
        assert_eq!(check.outcome, "cancelled");
        assert!(!f.task.repository.path.join("NEVER").exists());
        let mut task = f.latest(); task.artifacts[0].identity = Some("wrong".into());
        let evidence = crate::evidence::EvidenceStore::new(&f.base.join("state"), &task.repository).unwrap();
        assert!(evidence.register_task(&task).unwrap().iter().all(|r| r.evidence_id.is_none()));
        let escaped = "\u{1}".repeat(16000);
        assert!(f.checks.run(&f.latest(), "oversize", 5, &escaped).unwrap_err().contains("escaped"));
        let c = f.checks.run(&f.latest(), "shutdown", 30, "sleep 30 & wait").unwrap();
        let assigned = f.latest();
        let assignments = crate::task_assignment::AssignmentStore::new(&f.base.join("state"), &assigned.repository).unwrap();
        let first = assignments.prepare(&assigned, "claude", None, crate::task_assignment::ContextPolicy::bounded()).unwrap().unwrap();
        assignments.acknowledge(&assigned, "claude", "thread", &first.manifest.assignment_id).unwrap();

        assert!(f.checks.running() > 0);
        f.checks.shutdown();
        assert_eq!(f.checks.running(), 0);
        assert_eq!(f.wait(&c.id).outcome, "cancelled");
        let delta = assignments.prepare(&assigned, "claude", Some("thread"), crate::task_assignment::ContextPolicy::bounded()).unwrap().unwrap();
        assert!(delta.text.contains("record.json"));
        assert!(!assignments.allowed_evidence(&assigned, &delta.manifest.assignment_id).unwrap().unwrap().is_empty());
        let mut unfinished = f.wait(&c.id); unfinished.outcome = "running".into(); unfinished.finished_unix = None;
        save(&f.checks.directory(&f.task, &c.id).unwrap(), &unfinished).unwrap();
        let retried = f.checks.run(&f.latest(), "shutdown", 30, "sleep 30 & wait").unwrap();
        assert!(f.checks.status(&f.latest(), &retried).starts_with("unknown"));
    }
    #[test]
    fn stale_revision_crash_claim_and_bounded_evidence() {
        let _guard = SERIAL.lock().unwrap(); let f = Fixture::new("claims");
        f.store.update(&f.task.id, f.task.revision, "new", TaskUpdate::NextAction("next".into())).unwrap();
        assert!(f.checks.run(&f.task, "stale", 5, "touch NEVER").is_err());
        assert!(!f.task.repository.path.join("NEVER").exists());
        let c = f.checks.run(&f.latest(), "output", 5, "head -c 600000 /dev/zero").unwrap();
        let mut done = f.wait(&c.id); assert!(done.stdout.truncated); assert_eq!(done.stdout.bytes, 600000);
        let dir = f.checks.directory(&f.task, &c.id).unwrap();
        assert_eq!(fs::metadata(dir.join("stdout")).unwrap().len(), OUTPUT_LIMIT as u64);
        let evidence = crate::evidence::EvidenceStore::new(&f.base.join("state"), &f.task.repository).unwrap();
        let registrations = evidence.register_task(&f.latest()).unwrap();
        assert!(registrations.iter().any(|r| r.reference == format!("check:{}/stdout", c.id) && r.evidence_id.is_some()));
        done.outcome = "running".into(); done.finished_unix = None; save(&dir, &done).unwrap();
        assert!(f.checks.progress(&f.latest()).join("\n").contains("unknown (unfinished"));
        assert_eq!(f.checks.run(&f.latest(), "output", 5, &c.command).unwrap().outcome, "running");
    }
}

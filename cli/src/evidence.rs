//! Immutable, task-scoped evidence registrations.
//!
//! Artifact references in a task are labels, not read capabilities. Registration verifies the source beneath its
//! authorized root, copies its bytes into Ringo's private state and only then issues an opaque evidence ID. Later
//! readers use that snapshot: changing or deleting the live source never changes what an old assignment referred to.

use crate::task::{RepositoryBinding, Task};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// Registration is deliberately bounded. Larger sources remain available to the owner through the original job, but
/// are not silently uploaded as task evidence.
pub const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum EvidenceError {
    Invalid(String),
    NotFound(String),
    WrongTask(String),
    Io(String),
    Corrupt(String),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) | Self::Io(why) | Self::Corrupt(why) => f.write_str(why),
            Self::NotFound(id) => write!(f, "no registered evidence {id} in this task"),
            Self::WrongTask(id) => write!(f, "evidence {id} belongs to another task"),
        }
    }
}
impl std::error::Error for EvidenceError {}
type Result<T> = std::result::Result<T, EvidenceError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Ready,
    /// The source no longer matches the owner-supplied content identity. No new evidence capability was issued.
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    /// Stable within this registration operation; it names either an owner artifact or one attached job output.
    pub origin: String,
    /// Safe to include in an assignment. Private job folder paths are replaced by stable job/output labels.
    pub reference: String,
    pub declared_identity: Option<String>,
    pub evidence_id: Option<String>,
    pub digest: Option<String>,
    pub bytes: Option<u64>,
    pub status: EvidenceStatus,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceRecord {
    schema_version: u32,
    id: String,
    task_id: String,
    repository: RepositoryBinding,
    origin: String,
    digest: String,
    bytes: u64,
    snapshot: String,
}

#[derive(Clone, Debug)]
pub struct EvidenceStore {
    repository: RepositoryBinding,
    dir: PathBuf,
    root: PathBuf,
}

impl EvidenceStore {
    pub fn new(root: &Path, repository: &RepositoryBinding) -> Result<Self> {
        if !repository.path.is_absolute() {
            return Err(EvidenceError::Invalid("the evidence repository must be an absolute path".into()));
        }
        if fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(EvidenceError::Invalid(format!("the state folder {} cannot be a symlink", root.display())));
        }
        let key = hex(&Sha256::digest(repository.path.as_os_str().as_encoded_bytes()));
        let dir = root.join("task-evidence").join(key);
        if dir.starts_with(&repository.path) {
            return Err(EvidenceError::Invalid("evidence snapshots must stay outside the repository".into()));
        }
        Ok(Self { repository: repository.clone(), dir, root: root.into() })
    }

    /// Register all currently named task artifacts and attached job outputs in stable task order. A source that
    /// disappeared after a successful registration keeps its previous immutable registration.
    pub fn register_task(&self, task: &Task) -> Result<Vec<Registration>> {
        let staged = self.stage_task(task)?;
        self.publish(task, &staged)?;
        Ok(staged)
    }

    /// The first half of `register_task`: write the immutable bytes and records, but publish no latest pointer, so
    /// no evidence identity is handed out yet. A preparation that then fails its budget check leaves the cache it
    /// may reuse but grants nothing and changes no discoverable evidence.
    pub fn stage_task(&self, task: &Task) -> Result<Vec<Registration>> {
        if task.repository != self.repository {
            return Err(EvidenceError::WrongTask(task.id.clone()));
        }
        private_dir(&self.dir.join(&task.id))?;
        let mut out = Vec::new();
        for artifact in &task.artifacts {
            if let Some(id) = artifact.reference.strip_prefix("check:") {
                let store = crate::task_check::CheckStore::new(&self.root, &task.repository);
                match store.and_then(|store| {
                    let check = store.load(task, id)?;
                    if check.finished_unix.is_none() { return Err("check output is not final".into()); }
                    Ok(store.directory(task, id)?)
                }) {
                    Ok(dir) => for name in ["record.json", "stdout", "stderr"] {
                        let label = format!("check:{id}/{name}");
                        out.push(self.register_source(task, label.clone(), label, None,
                                                      &dir, Path::new(name), true, false));
                    },
                    Err(error) => out.push(unavailable(artifact_origin(&artifact.reference), artifact.reference.clone(),
                                                       None, &error)),
                }
                continue;
            }
            let origin = artifact_origin(&artifact.reference);
            out.push(self.register_source(task, origin, artifact.reference.clone(), artifact.identity.clone(),
                                          &task.repository.path, Path::new(&artifact.reference), false, false));
        }
        for job in &task.jobs {
            for (index, artifact) in job.evidence.iter().enumerate() {
                let name = match index {
                    0 => "reply.md",
                    1 => "events.jsonl",
                    _ => {
                        out.push(unavailable(format!("job:{}:{index}", job.id), format!("job:{}/output-{index}", job.id),
                                             artifact.identity.clone(), "only the attached reply and event log are registerable"));
                        continue;
                    }
                };
                let expected = job.directory.join(name);
                if Path::new(&artifact.reference) != expected {
                    out.push(unavailable(format!("job:{}:{name}", job.id), format!("job:{}/{name}", job.id),
                                         artifact.identity.clone(), "the evidence reference is outside its attached job output"));
                    continue;
                }
                out.push(self.register_source(task, format!("job:{}:{name}", job.id), format!("job:{}/{name}", job.id),
                                              artifact.identity.clone(), &job.directory, Path::new(name), true, false));
            }
        }
        Ok(out)
    }

    /// Publish the latest pointer for each ready staged registration. This is the only step that makes an evidence
    /// ID discoverable; it is separate so a failed preparation never publishes one.
    pub fn publish(&self, task: &Task, registrations: &[Registration]) -> Result<()> {
        if task.repository != self.repository {
            return Err(EvidenceError::WrongTask(task.id.clone()));
        }
        for registration in registrations {
            let (Some(id), EvidenceStatus::Ready) = (&registration.evidence_id, &registration.status) else { continue };
            let record = self.load_record(&task.id, id)?;
            if record.origin != registration.origin {
                return Err(EvidenceError::Corrupt("a staged registration does not name its origin".into()));
            }
            // The per-ID marker is what makes direct lookup possible; the latest pointer is only discovery. A
            // staged record without a marker stays unknown even to a caller that can derive its ID.
            self.publish_id(&task.id, id)?;
            self.record_latest(&record)?;
        }
        Ok(())
    }

    fn publish_id(&self, task_id: &str, evidence_id: &str) -> Result<()> {
        valid_id(evidence_id, "e-")?;
        let dir = self.dir.join(task_id).join("published");
        private_dir(&dir)?;
        let path = dir.join(evidence_id);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(EvidenceError::Corrupt("an evidence publication marker is a symlink and was left untouched".into()));
        }
        if !path.exists() {
            write_new(&path, b"published")?;
        }
        Ok(())
    }

    /// A publication marker makes an ID readable directly, including after its origin is superseded. It is checked
    /// without following anything: a marker that is not a regular private file publishes nothing.
    fn is_published(&self, task_id: &str, evidence_id: &str) -> bool {
        if valid_id(evidence_id, "e-").is_err() {
            return false;
        }
        let path = self.dir.join(task_id).join("published").join(evidence_id);
        fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
    }
    /// Metadata for an issued ID, scoped again at lookup. The snapshot path is never part of the public registration.
    /// A record that was staged by a preparation but never published is not issued: it reads as unknown here.
    pub fn registration(&self, task: &Task, evidence_id: &str) -> Result<Registration> {
        let record = self.load_record(&task.id, evidence_id)?;
        if !self.is_published(&task.id, evidence_id) {
            return Err(EvidenceError::NotFound(evidence_id.into()));
        }
        if record.task_id != task.id || record.repository != self.repository || task.repository != self.repository {
            return Err(EvidenceError::WrongTask(evidence_id.into()));
        }
        Ok(Registration {
            origin: record.origin,
            reference: format!("evidence:{evidence_id}"),
            declared_identity: None,
            evidence_id: Some(record.id),
            digest: Some(record.digest),
            bytes: Some(record.bytes),
            status: EvidenceStatus::Ready,
            detail: None,
        })
    }

    /// Every currently latest registration of this task, read-only: this registers nothing. Superseded snapshots
    /// stay readable by their own IDs but are not listed here. A pointer that cannot be trusted is reported as
    /// unavailable, never skipped or followed somewhere else.
    pub fn registered(&self, task: &Task) -> Result<Vec<Registration>> {
        if task.repository != self.repository {
            return Err(EvidenceError::WrongTask(task.id.clone()));
        }
        let latest = self.dir.join(&task.id).join("latest");
        match fs::symlink_metadata(&latest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(EvidenceError::Corrupt("the latest evidence folder is a symlink and was not followed".into()))
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(EvidenceError::Corrupt("the latest evidence path is not a folder".into()))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(EvidenceError::Io(e.to_string())),
        }
        let entries = fs::read_dir(&latest).map_err(|e| EvidenceError::Io(e.to_string()))?;
        let mut names: Vec<_> = entries.filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let pointer = latest.join(&name);
            let label = format!("latest:{}", name.to_string_lossy());
            if fs::symlink_metadata(&pointer).is_ok_and(|meta| meta.file_type().is_symlink()) {
                out.push(unavailable(String::new(), label, None,
                                     "the latest evidence pointer is a symlink and was not followed"));
                continue;
            }
            let id = match fs::read_to_string(&pointer) {
                Ok(id) => id,
                Err(e) => {
                    out.push(unavailable(String::new(), label, None,
                                         &format!("the latest evidence pointer is unreadable: {e}")));
                    continue;
                }
            };
            let id = id.trim();
            match self.load_record(&task.id, id) {
                // A record must resolve inside this task and repository, exactly as a direct lookup requires.
                Ok(record) if record.task_id == task.id && record.repository == self.repository
                    && self.is_published(&task.id, id) => out.push(Registration {
                    origin: record.origin, reference: format!("evidence:{id}"), declared_identity: None,
                    evidence_id: Some(record.id), digest: Some(record.digest), bytes: Some(record.bytes),
                    status: EvidenceStatus::Ready, detail: None,
                }),
                Ok(record) if record.task_id == task.id && record.repository == self.repository => {
                    out.push(unavailable(String::new(), format!("evidence:{id}"), None,
                        "the latest evidence pointer names a record that was never published"))
                }
                Ok(_) => out.push(unavailable(String::new(), format!("evidence:{id}"), None,
                    "the latest evidence pointer names a record outside this task's scope")),
                Err(e) => out.push(unavailable(String::new(), format!("evidence:{id}"), None,
                    &format!("the latest evidence pointer is unreadable and was left untouched: {e}"))),
            }
        }
        Ok(out)
    }

    /// Open the immutable snapshot for a future bounded reader. This resolves no live source and grants no path access.
    pub(crate) fn snapshot(&self, task: &Task, evidence_id: &str) -> Result<fs::File> {
        if !self.is_published(&task.id, evidence_id) {
            return Err(EvidenceError::NotFound(evidence_id.into()));
        }
        self.open_snapshot(task, evidence_id)
    }

    /// Open a staged snapshot for the preparing assignment itself, before publication. Only the preparation path
    /// uses this to measure its claims; every issued read goes through `snapshot`, which requires the marker.
    pub(crate) fn staged_snapshot(&self, task: &Task, evidence_id: &str) -> Result<fs::File> {
        self.open_snapshot(task, evidence_id)
    }

    fn open_snapshot(&self, task: &Task, evidence_id: &str) -> Result<fs::File> {
        let record = self.load_record(&task.id, evidence_id)?;
        open_regular_beneath(&self.dir.join(&task.id).join("blobs"), Path::new(&record.snapshot))
            .map_err(|e| EvidenceError::Io(format!("the evidence snapshot could not be opened: {e}")))
    }

    fn load_record(&self, task_id: &str, evidence_id: &str) -> Result<EvidenceRecord> {
        valid_id(evidence_id, "e-")?;
        let path = self.dir.join(task_id).join("records").join(format!("{evidence_id}.json"));
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => EvidenceError::NotFound(evidence_id.into()),
            _ => EvidenceError::Io(format!("evidence metadata could not be read: {e}")),
        })?;
        let record: EvidenceRecord = serde_json::from_slice(&bytes)
            .map_err(|e| EvidenceError::Corrupt(format!("evidence metadata is corrupt and was left untouched: {e}")))?;
        if record.schema_version != EVIDENCE_SCHEMA_VERSION || record.id != evidence_id || record.task_id != task_id
            || record.repository != self.repository || !valid_digest(&record.digest)
            || record.id != format!("e-{}", &hex(&Sha256::digest(format!("{}\0{}\0{}", task_id, record.origin, record.digest).as_bytes()))[..24])
            || record.bytes > MAX_SNAPSHOT_BYTES
            || record.snapshot != format!("{}.bin", &record.digest[7..]) {
            return Err(EvidenceError::Corrupt("evidence metadata failed its scope or identity check".into()));
        }
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    /// `job` marks an attached job output: a missing source recovers its earlier snapshot, and its directory must
    /// be reached without following symlinks — a redirected job root is refused, never read at its destination.
    fn register_source(
        &self, task: &Task, origin: String, display: String, declared: Option<String>,
        root: &Path, relative: &Path, job: bool, publish: bool,
    ) -> Registration {
        match self.register_one(task, &origin, display.clone(), declared.clone(), root, relative, job, publish) {
            Ok(mut registration) => {
                registration.reference = display;
                registration.declared_identity = declared;
                registration
            }
            Err(EvidenceError::NotFound(_)) if job => {
                match self.previous(task, &origin, display.clone(), declared.clone()) {
                    Ok(previous) => previous,
                    Err(_) => unavailable(origin, display, declared, "the source is missing and no earlier snapshot is registered"),
                }
            }
            Err(why) => unavailable(origin, display, declared, &why.to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn register_one(
        &self, task: &Task, origin: &str, display: String, declared: Option<String>, root: &Path, relative: &Path,
        _job: bool, publish: bool,
    ) -> Result<Registration> {
        if relative.as_os_str().is_empty() {
            return Err(EvidenceError::NotFound(display));
        }
        let mut file = match open_regular_beneath_strict(root, relative) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(EvidenceError::NotFound(display)),
            Err(e) => return Err(EvidenceError::Invalid(format!("evidence source was refused: {e}"))),
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(MAX_SNAPSHOT_BYTES + 1).read_to_end(&mut bytes)
            .map_err(|e| EvidenceError::Io(format!("evidence source could not be read: {e}")))?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(EvidenceError::Invalid(format!("evidence exceeds the {MAX_SNAPSHOT_BYTES}-byte registration limit")));
        }
        let digest = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
        if declared.as_deref().is_some_and(valid_digest) && declared.as_deref() != Some(digest.as_str()) {
            return Ok(Registration {
                origin: origin.into(), reference: display, declared_identity: declared, evidence_id: None,
                digest: Some(digest), bytes: Some(bytes.len() as u64), status: EvidenceStatus::Stale,
                detail: Some("the live content differs from the declared identity; earlier snapshots are unchanged".into()),
            });
        }
        let id = format!("e-{}", &hex(&Sha256::digest(format!("{}\0{origin}\0{digest}", task.id).as_bytes()))[..24]);
        let record = EvidenceRecord {
            schema_version: EVIDENCE_SCHEMA_VERSION, id: id.clone(), task_id: task.id.clone(),
            repository: self.repository.clone(), origin: origin.into(),
            snapshot: format!("{}.bin", &digest[7..]), digest: digest.clone(), bytes: bytes.len() as u64,
        };
        self.persist(&record, &bytes, publish)?;
        Ok(Registration {
            origin: origin.into(), reference: display, declared_identity: declared, evidence_id: Some(id),
            digest: Some(digest), bytes: Some(record.bytes), status: EvidenceStatus::Ready, detail: None,
        })
    }

    fn previous(&self, task: &Task, origin: &str, display: String, declared: Option<String>) -> Result<Registration> {
        let pointer = self.dir.join(&task.id).join("latest").join(format!("{}.txt", hex(&Sha256::digest(origin.as_bytes()))));
        let id = fs::read_to_string(&pointer).map_err(|e| EvidenceError::Io(format!("{}: {e}", pointer.display())))?;
        let record = self.load_record(&task.id, id.trim())?;
        if record.origin != origin {
            return Err(EvidenceError::Corrupt("the latest evidence pointer does not name its origin".into()));
        }
        if !self.is_published(&task.id, &record.id) {
            return Err(EvidenceError::Corrupt("the latest evidence pointer names a record that was never published".into()));
        }
        if declared.as_deref().is_some_and(valid_digest) && declared.as_deref() != Some(record.digest.as_str()) {
            return Ok(unavailable(origin.into(), display, declared, "the only snapshot does not match the declared identity"));
        }
        Ok(Registration {
            origin: origin.into(), reference: display, declared_identity: declared, evidence_id: Some(record.id),
            digest: Some(record.digest), bytes: Some(record.bytes), status: EvidenceStatus::Ready,
            detail: Some("the source is missing; the earlier immutable snapshot remains registered".into()),
        })
    }

    fn persist(&self, record: &EvidenceRecord, bytes: &[u8], publish: bool) -> Result<()> {
        let task = self.dir.join(&record.task_id);
        let blobs = task.join("blobs");
        let records = task.join("records");
        private_dir(&blobs)?;
        private_dir(&records)?;
        let blob = blobs.join(&record.snapshot);
        match fs::symlink_metadata(&blob) {
            Ok(meta) if !meta.file_type().is_file() || meta.file_type().is_symlink() => {
                return Err(EvidenceError::Corrupt("an evidence snapshot is not a regular private file".into()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_new(&blob, bytes)?,
            Err(e) => return Err(EvidenceError::Io(e.to_string())),
        }
        let path = records.join(format!("{}.json", record.id));
        if path.exists() {
            let loaded = self.load_record(&record.task_id, &record.id)?;
            if loaded != *record {
                return Err(EvidenceError::Corrupt("an evidence ID names two different registrations".into()));
            }
        } else {
            let encoded = serde_json::to_vec_pretty(record).map_err(|e| EvidenceError::Invalid(e.to_string()))?;
            write_new(&path, &encoded)?;
        }
        if publish {
            self.record_latest(record)?;
        }
        Ok(())
    }

    /// A missing live source reuses the latest registration for its origin. The pointer is derived state: its bytes
    /// are only ever an ID already in `records/`, and a corrupt pointer never rewrites or deletes evidence.
    fn record_latest(&self, record: &EvidenceRecord) -> Result<()> {
        let latest = self.dir.join(&record.task_id).join("latest").join(format!("{}.txt", hex(&Sha256::digest(record.origin.as_bytes()))));
        fs::create_dir_all(latest.parent().expect("the latest pointer has a parent"))
            .map_err(|e| EvidenceError::Io(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(latest.parent().unwrap(), fs::Permissions::from_mode(0o700)).map_err(|e| EvidenceError::Io(e.to_string()))?;
        }
        if fs::symlink_metadata(&latest).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(EvidenceError::Corrupt(format!("{} is a symlink and was left untouched", latest.display())));
        }
        replace_atomic(&latest, record.id.as_bytes())
    }
}

/// The registration origin of an owner artifact reference. It is derived from the reference itself, never from the
/// artifact's position: removing an earlier artifact then shifts later ones, and a positional origin would let a
/// reader bind a later artifact to an earlier artifact's snapshot between two registrations. The digest prefix keeps
/// a private path out of the value that assignments and the read-only projection expose.
pub fn artifact_origin(reference: &str) -> String {
    format!("artifact:{}", &hex(&Sha256::digest(reference.as_bytes()))[..16])
}

fn unavailable(origin: String, reference: String, declared: Option<String>, why: &str) -> Registration {
    Registration {
        origin, reference, declared_identity: declared, evidence_id: None, digest: None, bytes: None,
        status: EvidenceStatus::Unavailable, detail: Some(why.into()),
    }
}

pub(crate) fn valid_digest(identity: &str) -> bool {
    identity.len() == 71 && identity.starts_with("sha256:") && identity[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_id(id: &str, prefix: &str) -> Result<()> {
    if id.len() != 26 || !id.starts_with(prefix) || !id[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EvidenceError::Invalid(format!("invalid opaque evidence id {id:?}")));
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| EvidenceError::Io(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| EvidenceError::Io(e.to_string()))?;
    }
    Ok(())
}

fn replace_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let part = path.with_extension(format!("replace-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&part).map_err(|e| EvidenceError::Io(e.to_string()))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&part, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&part);
        return Err(EvidenceError::Io(error.to_string()));
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    // A unique temporary name per write: two threads in this process staging the same content must not share a
    // pathname, or one's failed create could delete the other's in-progress file. Only the invocation that created
    // the temporary file cleans it up.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let part = path.with_extension(format!("part-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(&part) {
        Ok(file) => file,
        Err(e) => {
            // This invocation created nothing, so there is nothing of ours to remove. A content-addressed blob may
            // have won a concurrent race; never replace it, its digest is checked on read.
            if path.is_file() {
                return Ok(());
            }
            return Err(EvidenceError::Io(format!("{}: {e}", path.display())));
        }
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&part, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        // A content-addressed blob may have won a concurrent race. Never replace it; its digest will be checked on read.
        if path.is_file() {
            return Ok(());
        }
        return Err(EvidenceError::Io(format!("{}: {e}", path.display())));
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_beneath(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let root = root.canonicalize()?;
    let c_root = CString::new(root.as_os_str().as_bytes()).map_err(|_| std::io::Error::other("NUL in evidence root"))?;
    // SAFETY: the path is NUL-terminated and live for the call; returned descriptors are owned below.
    let fd = unsafe { libc::open(c_root.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd was returned by open and is not owned elsewhere.
    let directory = unsafe { fs::File::from_raw_fd(fd) };
    open_regular_beneath_dir(&directory, relative)
}

/// Like `open_regular_beneath`, but the root itself is reached component by component without following a single
/// symlink: a job directory whose old path now redirects must be refused, not read at its new destination.
#[cfg(unix)]
pub(crate) fn open_regular_beneath_strict(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: the path is NUL-terminated and live for the call; returned descriptors are owned below.
    let fd = unsafe { libc::open(c"/".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd was returned by open and is not owned elsewhere.
    let mut directory = unsafe { fs::File::from_raw_fd(fd) };
    for part in root.components() {
        let Component::Normal(name) = part else {
            if matches!(part, Component::RootDir) { continue; }
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence path escapes its authorized root"));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("NUL in evidence path"))?;
        // SAFETY: directory remains open and no user-controlled pointer is retained.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            // A symlinked component is reported as ELOOP or ENOTDIR depending on the kernel; both refuse it.
            if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the evidence root redirects through a symlink or is not a plain directory"));
            }
            return Err(e);
        }
        // SAFETY: fd was returned by openat and is transferred exactly once.
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    open_regular_beneath_dir(&directory, relative)
}

/// Walks `relative` beneath an already-open, trusted root directory, refusing symlinks and non-regular files.
#[cfg(unix)]
fn open_regular_beneath_dir(root: &fs::File, relative: &Path) -> std::io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut directory = root.try_clone()?; // a fresh handle: the caller's stays at the root
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence path escapes its authorized root"));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("NUL in evidence path"))?;
        let last = parts.peek().is_none();
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: directory remains open, name is terminated, and no user-controlled pointer is retained.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was returned by openat and is transferred exactly once.
        directory = unsafe { fs::File::from_raw_fd(fd) };
        if last {
            let meta = directory.metadata()?;
            if !meta.file_type().is_file() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence is not a regular file"));
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
use std::os::fd::FromRawFd;

#[cfg(not(unix))]
fn open_regular_beneath(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    let root = root.canonicalize()?;
    let mut path = root.clone();
    for part in relative.components() {
        let Component::Normal(name) = part else {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence path escapes its authorized root"));
        };
        path.push(name);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence follows a symlink"));
        }
    }
    if !path.starts_with(&root) || !path.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "evidence is not a regular file beneath its root"));
    }
    fs::File::open(path)
}

/// On non-unix there is no dirfd confinement; the root is checked component by component for symlinks first.
#[cfg(not(unix))]
pub(crate) fn open_regular_beneath_strict(root: &Path, relative: &Path) -> std::io::Result<fs::File> {
    let mut walked = PathBuf::new();
    for part in root.components() {
        walked.push(part);
        // A Windows drive/UNC prefix is not an independently accessible path. Check
        // after RootDir makes it a complete volume root, then every child component.
        if matches!(part, Component::Prefix(_)) { continue; }
        if fs::symlink_metadata(&walked)?.file_type().is_symlink() {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the evidence root redirects through a symlink"));
        }
    }
    open_regular_beneath(root, relative)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ArtifactRef, JobRef, TaskStore, TaskUpdate};

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ringo-evidence-{name}-{}", crate::task::request_id()));
            fs::create_dir_all(path.join("repo")).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn store(&self) -> TaskStore {
            TaskStore::new(&self.0.join("state"), self.0.join("repo").canonicalize().unwrap(), true).unwrap()
        }
        fn task(&self) -> Task {
            self.store().create("evidence-create", "Keep the source identity.").unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn evidence(temp: &Temp, task: &Task) -> EvidenceStore {
        EvidenceStore::new(&temp.0.join("state"), &task.repository).unwrap()
    }

    fn add_artifact(temp: &Temp, task: &Task, reference: &str, identity: Option<String>) -> Task {
        temp.store().update(&task.id, task.revision, &crate::task::request_id(), TaskUpdate::AddArtifact(ArtifactRef {
            reference: reference.into(), identity,
        })).unwrap()
    }

    #[test]
    fn metadata_cannot_rebind_an_existing_evidence_id() {
        let temp = Temp::new("rebind");
        let task = temp.task();
        fs::write(temp.0.join("repo/note.txt"), "first").unwrap();
        let task = add_artifact(&temp, &task, "note.txt", None);
        let store = evidence(&temp, &task);
        let first = store.register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        fs::write(temp.0.join("repo/note.txt"), "second").unwrap();
        let second = store.register_task(&task).unwrap().pop().unwrap().evidence_id.unwrap();
        let mut record = store.load_record(&task.id, &second).unwrap();
        record.id = first.clone();
        fs::write(store.dir.join(&task.id).join("records").join(format!("{first}.json")), serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(store.load_record(&task.id, &first).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn redirected_repository_roots_are_refused() {
        let temp = Temp::new("redirect-repository");
        let task = temp.task();
        fs::write(temp.0.join("repo/note.txt"), "original").unwrap();
        let task = add_artifact(&temp, &task, "note.txt", None);
        let store = evidence(&temp, &task);
        fs::rename(temp.0.join("repo"), temp.0.join("replacement")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("replacement"), temp.0.join("repo")).unwrap();
        assert_eq!(store.register_task(&task).unwrap()[0].status, EvidenceStatus::Unavailable);
    }

    #[test]
    fn latest_replacement_is_atomic_and_reports_failure() {
        let temp = Temp::new("replace");
        let path = temp.0.join("latest");
        fs::write(&path, "old").unwrap();
        replace_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(replace_atomic(&path, b"refused").is_err());
    }

    #[test]
    fn snapshots_and_opaque_ids_survive_reopen_and_source_deletion() {
        let temp = Temp::new("restart");
        let made = temp.task();
        fs::write(temp.0.join("repo/note.txt"), "first 🌍").unwrap();
        let made = add_artifact(&temp, &made, "note.txt", None);
        let registered = evidence(&temp, &made).register_task(&made).unwrap().pop().unwrap();
        assert_eq!(registered.status, EvidenceStatus::Ready);
        assert_eq!(registered.bytes, Some("first 🌍".len() as u64));
        let id = registered.evidence_id.unwrap();
        assert!(!id.contains("note") && !id.contains('/'));
        fs::remove_file(temp.0.join("repo/note.txt")).unwrap();
        let reopened = evidence(&temp, &made).register_task(&made).unwrap().pop().unwrap();
        // Owner artifacts do not silently substitute an old snapshot when their live source disappears.
        assert_eq!(reopened.status, EvidenceStatus::Unavailable);
        let still = evidence(&temp, &made).registration(&made, &id).unwrap();
        assert_eq!(still.digest, registered.digest);
        let mut content = String::new();
        evidence(&temp, &made).snapshot(&made, &id).unwrap().read_to_string(&mut content).unwrap();
        assert_eq!(content, "first 🌍");
    }

    #[test]
    fn job_snapshots_remain_registered_when_the_live_output_disappears() {
        let temp = Temp::new("job");
        let made = temp.task();
        let dir = temp.0.join("private/job");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reply.md"), "an attributed response").unwrap();
        fs::write(dir.join("events.jsonl"), "{}\n").unwrap();
        let id = hex(&Sha256::digest(dir.as_os_str().as_encoded_bytes()));
        let job = JobRef {
            id, shell_id: 1, agent: "claude".into(), write: false, directory: dir.clone(), status_at_attach: "done".into(),
            base: None, before: None, after: None,
            evidence: ["reply.md", "events.jsonl"].map(|name| ArtifactRef {
                reference: dir.join(name).to_string_lossy().into_owned(), identity: None,
            }).into_iter().collect(),
        };
        let made = temp.store().attach(&made.id, made.revision, "job-attach", job).unwrap();
        let first = evidence(&temp, &made).register_task(&made).unwrap();
        assert!(first.iter().all(|r| r.status == EvidenceStatus::Ready));
        assert!(first.iter().all(|r| !r.reference.contains("private")));
        fs::remove_file(dir.join("reply.md")).unwrap();
        let next = evidence(&temp, &made).register_task(&made).unwrap();
        assert_eq!(next[0].evidence_id, first[0].evidence_id);
        assert!(next[0].detail.as_deref().unwrap_or_default().contains("immutable"));
    }

    #[cfg(unix)]
    #[test]
    fn a_redirected_job_directory_is_refused_not_read_through_the_symlink() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new("redirect");
        let made = temp.task();
        let dir = temp.0.join("private/job");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reply.md"), "the real attached reply").unwrap();
        let id = hex(&Sha256::digest(dir.as_os_str().as_encoded_bytes()));
        let job = JobRef {
            id, shell_id: 1, agent: "claude".into(), write: false, directory: dir.clone(), status_at_attach: "done".into(),
            base: None, before: None, after: None,
            evidence: ["reply.md"].map(|name| ArtifactRef {
                reference: dir.join(name).to_string_lossy().into_owned(), identity: None,
            }).into_iter().collect(),
        };
        let made = temp.store().attach(&made.id, made.revision, "job-attach", job).unwrap();
        // The job directory moves and its old path becomes a symlink to an unrelated folder.
        let moved = temp.0.join("elsewhere");
        fs::rename(&dir, &moved).unwrap();
        fs::write(moved.join("reply.md"), "unrelated content").unwrap();
        symlink(&moved, &dir).unwrap();
        let registrations = evidence(&temp, &made).register_task(&made).unwrap();
        let reply = &registrations[0];
        assert_eq!(reply.status, EvidenceStatus::Unavailable, "a redirected job root must not be read: {reply:?}");
        assert!(reply.evidence_id.is_none());
        assert!(reply.detail.as_deref().unwrap_or_default().contains("symlink"), "detail was: {:?}", reply.detail);
        // The unrelated content was never registered under the job's origin.
        assert!(registrations.iter().all(|r| r.status != EvidenceStatus::Ready));
    }

    #[test]
    fn an_artifacts_origin_follows_its_reference_not_its_position() {
        let temp = Temp::new("reindex");
        let made = temp.task();
        fs::write(temp.0.join("repo/first.txt"), "first bytes").unwrap();
        fs::write(temp.0.join("repo/second.txt"), "second bytes").unwrap();
        let made = add_artifact(&temp, &made, "first.txt", None);
        let made = add_artifact(&temp, &made, "second.txt", None);
        let registered = evidence(&temp, &made).register_task(&made).unwrap();
        let (first, second) = (&registered[0], &registered[1]);
        assert_eq!(first.origin, artifact_origin("first.txt"));
        assert_eq!(second.origin, artifact_origin("second.txt"));
        assert_ne!(first.evidence_id, second.evidence_id);
        // Removing the earlier artifact shifts the later one to index 0; its origin and identity do not follow.
        let made = temp.store().update(&made.id, made.revision, &crate::task::request_id(),
                                       TaskUpdate::RemoveArtifact(1)).unwrap();
        let again = evidence(&temp, &made).register_task(&made).unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].origin, artifact_origin("second.txt"));
        assert_eq!(again[0].evidence_id, second.evidence_id);
        assert_ne!(again[0].evidence_id, first.evidence_id);
    }

    #[test]
    fn a_staged_evidence_id_stays_unreadable_until_its_preparation_publishes() {
        let temp = Temp::new("staged");
        let made = temp.task();
        fs::write(temp.0.join("repo/note.txt"), "staged bytes").unwrap();
        let made = add_artifact(&temp, &made, "note.txt", None);
        let store = evidence(&temp, &made);
        let staged = store.stage_task(&made).unwrap().pop().unwrap();
        assert_eq!(staged.status, EvidenceStatus::Ready);
        let id = staged.evidence_id.clone().unwrap();
        // The bytes are cached, but an id is published only by a preparation that passed its budget check. A caller
        // that can derive the id (it hashes the origin) learns nothing from the staging alone.
        assert!(matches!(store.registration(&made, &id), Err(EvidenceError::NotFound(_))));
        assert!(matches!(store.snapshot(&made, &id), Err(EvidenceError::NotFound(_))));
        assert!(store.registered(&made).unwrap().is_empty());
        store.publish(&made, &[staged]).unwrap();
        assert_eq!(store.registration(&made, &id).unwrap().evidence_id.as_deref(), Some(id.as_str()));
        let registered = store.registered(&made).unwrap();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].evidence_id.as_deref(), Some(id.as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn a_redirected_or_unreadable_latest_pointer_is_reported_not_followed() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new("pointer");
        let made = temp.task();
        fs::write(temp.0.join("repo/first.txt"), "first bytes").unwrap();
        fs::write(temp.0.join("repo/second.txt"), "second bytes").unwrap();
        let made = add_artifact(&temp, &made, "first.txt", None);
        let made = add_artifact(&temp, &made, "second.txt", None);
        let registered = evidence(&temp, &made).register_task(&made).unwrap();
        let first_id = registered[0].evidence_id.clone().unwrap();
        let latest = evidence(&temp, &made).dir.join(&made.id).join("latest");
        let mut pointers: Vec<_> = fs::read_dir(&latest).unwrap().map(|e| e.unwrap().path()).collect();
        pointers.sort();
        assert_eq!(pointers.len(), 2);
        // One pointer is redirected, the other no longer names a record. Neither is followed or fatal.
        fs::remove_file(&pointers[0]).unwrap();
        symlink(temp.0.join("elsewhere.txt"), &pointers[0]).unwrap();
        fs::write(&pointers[1], "not an evidence id").unwrap();
        let listed = evidence(&temp, &made).registered(&made).unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(listed.iter().all(|r| r.status == EvidenceStatus::Unavailable && r.evidence_id.is_none()));
        // The genuine snapshot stays readable by its own id.
        let still = evidence(&temp, &made).registration(&made, &first_id).unwrap();
        assert_eq!(still.evidence_id.as_deref(), Some(first_id.as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn traversal_symlink_escape_stale_identity_and_cross_task_access_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new("scope");
        let made = temp.task();
        fs::write(temp.0.join("secret"), "do not register").unwrap();
        symlink(temp.0.join("secret"), temp.0.join("repo/link.txt")).unwrap();
        let escaped = add_artifact(&temp, &made, "link.txt", None);
        let result = evidence(&temp, &escaped).register_task(&escaped).unwrap().pop().unwrap();
        assert_eq!(result.status, EvidenceStatus::Unavailable);
        assert!(result.evidence_id.is_none());
        let traversal = add_artifact(&temp, &escaped, "../secret", None);
        assert_eq!(evidence(&temp, &traversal).register_task(&traversal).unwrap().pop().unwrap().status,
                   EvidenceStatus::Unavailable);

        fs::write(temp.0.join("repo/note.txt"), "new").unwrap();
        let old = format!("sha256:{}", "0".repeat(64));
        let stale = add_artifact(&temp, &traversal, "note.txt", Some(old));
        let result = evidence(&temp, &stale).register_task(&stale).unwrap().pop().unwrap();
        assert_eq!(result.status, EvidenceStatus::Stale);
        assert!(result.evidence_id.is_none());

        let other = temp.store().create("other-create", "Another task").unwrap();
        let valid = add_artifact(&temp, &stale, "note.txt", None);
        let result = evidence(&temp, &valid).register_task(&valid).unwrap().pop().unwrap();
        let id = result.evidence_id.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", temp.0.join("repo/root-link")).unwrap();
        assert!(evidence(&temp, &valid).registration(&other, &id).is_err());
        assert!(evidence(&temp, &valid).registration(&valid, "e-../../../etc").is_err());
    }

    #[test]
    fn concurrent_registration_of_identical_content_does_not_delete_each_others_temp_files() {
        // Two threads staging the same content must not share a temporary pathname: with a per-process name, one
        // thread's failed create could delete the other's in-progress file. Unique names plus self-owned cleanup
        // make every registration succeed and leave the immutable blob intact.
        let temp = Temp::new("concurrent");
        let made = temp.task();
        fs::write(temp.0.join("repo/note.txt"), "shared bytes").unwrap();
        let made = add_artifact(&temp, &made, "note.txt", None);
        let store = evidence(&temp, &made);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| store.register_task(&made))).collect();
            for handle in handles {
                let regs = handle.join().unwrap().unwrap();
                assert_eq!(regs[0].status, EvidenceStatus::Ready);
                assert!(regs[0].evidence_id.is_some());
            }
        });
        // The blob is still readable and its bytes are intact.
        let id = store.registered(&made).unwrap().pop().unwrap().evidence_id.unwrap();
        let mut content = String::new();
        store.snapshot(&made, &id).unwrap().read_to_string(&mut content).unwrap();
        assert_eq!(content, "shared bytes");
    }
}

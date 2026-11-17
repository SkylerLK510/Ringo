//! Persistent, repository-bound owner tasks. Task contents stay in Ringo's private state, separate from provider
//! conversations. Every change reloads under an exclusive lock, checks the expected revision, and replaces the
//! record atomically; a stale writer or damaged record never overwrites newer state.

use crate::{Job, Kind, Status};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
const MAX_TEXT: usize = 64 * 1024;
const MAX_REQUEST_ID: usize = 128;

#[derive(Debug)]
pub enum TaskError {
    NotFound(String),
    WrongRepository { expected: PathBuf, actual: PathBuf },
    Conflict { expected: u64, actual: u64 },
    RequestConflict(String),
    Corrupt(String),
    UnsupportedVersion(u32),
    Invalid(String),
    Io(String),
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no task {id} in this repository"),
            Self::WrongRepository { expected, actual } => write!(f, "task belongs to {}, not {}; select it there (Phase 1 has no rebind)", expected.display(), actual.display()),
            Self::Conflict { expected, actual } => write!(f, "task changed from revision {expected} to {actual}; inspect it and apply the update again"),
            Self::RequestConflict(id) => write!(f, "request {id} was already used for a different task update"),
            Self::Corrupt(why) => write!(f, "task state is corrupt and was left untouched: {why}"),
            Self::UnsupportedVersion(version) => write!(f, "task schema {version} is newer than supported schema {SCHEMA_VERSION}; the file was left untouched"),
            Self::Invalid(why) | Self::Io(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for TaskError {}

type Result<T> = std::result::Result<T, TaskError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBinding {
    pub path: PathBuf,
    pub git: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerText {
    pub text: String,
    /// Phase 1 accepts these values from the owner only; agent claims cannot silently become owner instructions.
    pub provenance: String,
}

impl OwnerText {
    fn owner(text: String) -> Self {
        Self { text, provenance: "owner".into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub text: String,
    pub status: DecisionStatus,
    pub author: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// An owner-supplied repository-relative reference, or another explicit reference; not a file-read capability.
    pub reference: String,
    /// A repository revision or content identity, when the owner supplied one.
    pub identity: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRef {
    /// Stable identity of the private job folder. Shell job numbers restart in each conversation; this does not.
    pub id: String,
    pub shell_id: usize,
    pub agent: String,
    pub write: bool,
    pub directory: PathBuf,
    pub status_at_attach: String,
    pub base: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub evidence: Vec<ArtifactRef>,
}

impl JobRef {
    pub fn from_job(job: &Job) -> Result<Self> {
        let Kind::Agent { agent, write } = &job.kind else {
            return Err(TaskError::Invalid(format!("#{} is a Ringo pipeline, not an agent job", job.id)));
        };
        let status = match &job.status {
            Status::Running => "running".to_string(),
            Status::Done => "done".to_string(),
            Status::Failed(why) => format!("failed: {why}"),
            Status::Cancelled => "cancelled".to_string(),
        };
        let evidence = ["reply.md", "events.jsonl"].into_iter().map(|name| ArtifactRef {
            reference: job.dir.join(name).to_string_lossy().into_owned(), identity: None,
        }).collect();
        Ok(Self {
            id: hex(&Sha256::digest(job.dir.as_os_str().as_encoded_bytes())), shell_id: job.id,
            agent: agent.name().into(), write: *write, directory: job.dir.clone(), status_at_attach: status,
            base: job.base.clone(), before: job.before.clone(), after: job.after.clone(), evidence,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestReceipt {
    pub request_id: String,
    pub fingerprint: String,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub repository: RepositoryBinding,
    pub objective: OwnerText,
    pub constraints: Vec<OwnerText>,
    pub decisions: Vec<Decision>,
    pub next_action: Option<OwnerText>,
    pub artifacts: Vec<ArtifactRef>,
    pub unresolved_issues: Vec<OwnerText>,
    pub jobs: Vec<JobRef>,
    pub requests: Vec<RequestReceipt>,
    pub created_unix: u64,
    pub updated_unix: u64,
}

impl Task {
    pub fn concise_view(&self) -> String {
        let mut out = vec![format!("task {} · revision {} · repository {}", self.id, self.revision, self.repository.path.display()),
                           format!("Objective: {}", self.objective.text)];
        if !self.constraints.is_empty() {
            out.push("Constraints:".into());
            out.extend(self.constraints.iter().enumerate().map(|(i, item)| format!("  {}. {}", i + 1, item.text)));
        }
        if !self.decisions.is_empty() {
            out.push("Decisions:".into());
            out.extend(self.decisions.iter().enumerate().map(|(i, item)| {
                let status = match item.status { DecisionStatus::Proposed => "proposed", DecisionStatus::Accepted => "accepted" };
                format!("  {}. [{}] {} (by {})", i + 1, status, item.text, item.author)
            }));
        }
        out.push(format!("Next action: {}", self.next_action.as_ref().map_or("not set", |next| next.text.as_str())));
        if !self.artifacts.is_empty() {
            out.push("Artifacts:".into());
            out.extend(self.artifacts.iter().enumerate().map(|(i, item)| {
                format!("  {}. {}{}", i + 1, item.reference, item.identity.as_ref().map_or(String::new(), |id| format!(" @ {id}")))
            }));
        }
        if !self.unresolved_issues.is_empty() {
            out.push("Unresolved issues:".into());
            out.extend(self.unresolved_issues.iter().enumerate().map(|(i, item)| format!("  {}. {}", i + 1, item.text)));
        }
        if !self.jobs.is_empty() {
            out.push("Attached agent jobs (status is from attachment time; completion does not accept the task):".into());
            for job in &self.jobs {
                out.push(format!("  #{} {} ({}) · {} · {}", job.shell_id, job.agent, job.status_at_attach,
                                 job.directory.display(), job.id));
                for evidence in &job.evidence {
                    out.push(format!("    evidence: {}", evidence.reference));
                }
            }
        }
        out.join("\n")
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(TaskError::UnsupportedVersion(self.schema_version));
        }
        valid_task_id(&self.id)?;
        if self.revision == 0 || !self.repository.path.is_absolute() {
            return Err(TaskError::Corrupt("task has an invalid revision or repository identity".into()));
        }
        owner_text(&self.objective, "objective")?;
        for item in &self.constraints {
            owner_text(item, "constraint")?;
        }
        if let Some(next) = &self.next_action {
            owner_text(next, "next action")?;
        }
        for item in &self.unresolved_issues {
            owner_text(item, "unresolved issue")?;
        }
        for item in &self.decisions {
            bounded(&item.text, "decision")?;
            bounded(&item.author, "decision author")?;
        }
        for item in &self.artifacts {
            validate_artifact(item)?;
        }
        let mut jobs = std::collections::BTreeSet::new();
        for job in &self.jobs {
            bounded(&job.id, "job identity")?;
            bounded(&job.agent, "agent")?;
            bounded(&job.status_at_attach, "job status")?;
            if !job.directory.is_absolute() || !jobs.insert(&job.id) {
                return Err(TaskError::Corrupt("task has an invalid or duplicate job reference".into()));
            }
            for item in &job.evidence {
                validate_artifact(item)?;
            }
        }
        let mut requests = std::collections::BTreeSet::new();
        for receipt in &self.requests {
            valid_request_id(&receipt.request_id).map_err(|e| TaskError::Corrupt(e.to_string()))?;
            if receipt.revision == 0 || receipt.revision > self.revision || receipt.fingerprint.len() != 64
                || !receipt.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) || !requests.insert(&receipt.request_id) {
                return Err(TaskError::Corrupt("task has an invalid or duplicate request receipt".into()));
            }
        }
        if self.created_unix > self.updated_unix {
            return Err(TaskError::Corrupt("task timestamps are inconsistent".into()));
        }
        Ok(())
    }
}

/// Version 0 stored owner text as plain strings. Version 1 makes provenance explicit and adds distinguished
/// decisions plus request receipts. Migration is in memory on inspection, and persisted only by a later update.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskV0 {
    #[serde(rename = "schema_version")]
    _schema_version: u32,
    id: String,
    revision: u64,
    repository: RepositoryBinding,
    objective: String,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    unresolved_issues: Vec<String>,
    #[serde(default)]
    jobs: Vec<JobRef>,
    created_unix: u64,
    updated_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskUpdate {
    Objective(String),
    NextAction(String),
    AddConstraint(String),
    RemoveConstraint(usize),
    AddDecision { text: String, status: DecisionStatus },
    RemoveDecision(usize),
    AddArtifact(ArtifactRef),
    RemoveArtifact(usize),
    AddIssue(String),
    RemoveIssue(usize),
}

#[derive(Clone, Debug)]
pub struct TaskStore {
    repository: RepositoryBinding,
    dir: PathBuf,
}

impl TaskStore {
    pub fn new(root: &Path, repository: PathBuf, git: bool) -> Result<Self> {
        if !repository.is_absolute() {
            return Err(TaskError::Invalid("a task's repository must be an absolute, resolved path".into()));
        }
        if fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(TaskError::Invalid(format!("the state folder {} cannot be a symlink", root.display())));
        }
        let tasks = resolved(&root.join("tasks"));
        if tasks.starts_with(&repository) {
            return Err(TaskError::Invalid(format!("the task store {} is inside the repository", tasks.display())));
        }
        let key = hex(&Sha256::digest(repository.as_os_str().as_encoded_bytes()));
        Ok(Self { repository: RepositoryBinding { path: repository, git }, dir: tasks.join(key) })
    }

    pub fn create(&self, request_id: &str, objective: &str) -> Result<Task> {
        valid_request_id(request_id)?;
        bounded(objective, "objective")?;
        let id = format!("t-{}", &hex(&Sha256::digest(request_id.as_bytes()))[..24]);
        private_dir(&self.dir)?;
        let dir = self.dir.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => private_dir(&dir)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(TaskError::Io(format!("{}: {e}", dir.display()))),
        }
        self.mutate(&id, None, request_id, create_fingerprint(objective), |_task| {
            Err(TaskError::RequestConflict(request_id.into()))
        }, || {
            let now = now();
            let task = Task {
                schema_version: SCHEMA_VERSION, id: id.clone(), revision: 1, repository: self.repository.clone(),
                objective: OwnerText::owner(objective.into()), constraints: Vec::new(), decisions: Vec::new(),
                next_action: None, artifacts: Vec::new(), unresolved_issues: Vec::new(), jobs: Vec::new(),
                requests: vec![RequestReceipt { request_id: request_id.into(), fingerprint: create_fingerprint(objective), revision: 1 }],
                created_unix: now, updated_unix: now,
            };
            task.validate()?;
            write_task(&dir, &task)?;
            Ok(task)
        })
    }

    pub fn load(&self, id: &str) -> Result<Task> {
        valid_task_id(id)?;
        let task = read_task(&self.dir.join(id), id)?;
        if task.repository != self.repository {
            return Err(TaskError::WrongRepository { expected: task.repository.path, actual: self.repository.path.clone() });
        }
        Ok(task)
    }

    pub fn list(&self) -> Result<Vec<Task>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::Io(format!("{}: {e}", self.dir.display()))),
        };
        let mut tasks = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| TaskError::Io(e.to_string()))?;
            if !entry.file_type().map_err(|e| TaskError::Io(e.to_string()))?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            tasks.push(self.load(&id)?);
        }
        tasks.sort_by(|a, b| (b.updated_unix, &b.id).cmp(&(a.updated_unix, &a.id)));
        Ok(tasks)
    }

    pub fn update(&self, id: &str, expected_revision: u64, request_id: &str, update: TaskUpdate) -> Result<Task> {
        valid_task_id(id)?;
        valid_request_id(request_id)?;
        self.mutate(id, Some(expected_revision), request_id, fingerprint(&update), |mut task| {
            apply(&mut task, update)?;
            Ok(task)
        }, || Err(TaskError::NotFound(id.into())))
    }

    pub fn attach(&self, id: &str, expected_revision: u64, request_id: &str, job: JobRef) -> Result<Task> {
        valid_task_id(id)?;
        valid_request_id(request_id)?;
        let fingerprint = hex(&Sha256::digest(serde_json::to_vec(&job).expect("job references serialize")));
        self.mutate(id, Some(expected_revision), request_id, fingerprint, |mut task| {
            if !task.jobs.iter().any(|old| old.id == job.id) {
                task.jobs.push(job);
            }
            Ok(task)
        }, || Err(TaskError::NotFound(id.into())))
    }

    /// `create` (new_task) already exists or is absent; updates run inside the same locked reload/write critical
    /// section. Receipt lookup precedes the revision check so redelivery after later updates remains idempotent.
    fn mutate(
        &self, id: &str, expected: Option<u64>, request_id: &str, fingerprint: String,
        change: impl FnOnce(Task) -> Result<Task>, new_task: impl FnOnce() -> Result<Task>,
    ) -> Result<Task> {
        let dir = self.dir.join(id);
        let _lock = lock(&dir)?;
        let path = task_json(&dir);
        if !path.exists() {
            return new_task();
        }
        let mut task = read_task(&dir, id)?;
        if task.repository != self.repository {
            return Err(TaskError::WrongRepository { expected: task.repository.path, actual: self.repository.path.clone() });
        }
        if let Some(receipt) = task.requests.iter().find(|r| r.request_id == request_id) {
            if receipt.fingerprint == fingerprint {
                return Ok(task);
            }
            return Err(TaskError::RequestConflict(request_id.into()));
        }
        if let Some(expected) = expected && task.revision != expected {
            return Err(TaskError::Conflict { expected, actual: task.revision });
        }
        let before = task.revision;
        task = change(task)?;
        // Even a no-op must persist its receipt so retries cannot become new mutations.
        task.revision = task.revision.checked_add(1).ok_or_else(|| TaskError::Invalid("task revision overflow".into()))?;
        task.updated_unix = now().max(task.updated_unix);
        task.requests.push(RequestReceipt { request_id: request_id.into(), fingerprint, revision: task.revision });
        task.validate()?;
        if task.revision <= before {
            return Err(TaskError::Invalid("task revision did not increase".into()));
        }
        write_task(&dir, &task)?;
        Ok(task)
    }
}

fn apply(task: &mut Task, update: TaskUpdate) -> Result<()> {
    fn remove<T>(items: &mut Vec<T>, index: usize, what: &str) -> Result<()> {
        if index == 0 || index > items.len() {
            return Err(TaskError::Invalid(format!("no {what} {index}; inspect the task for its numbers")));
        }
        items.remove(index - 1);
        Ok(())
    }
    match update {
        TaskUpdate::Objective(text) => { bounded(&text, "objective")?; task.objective = OwnerText::owner(text); }
        TaskUpdate::NextAction(text) => { bounded(&text, "next action")?; task.next_action = Some(OwnerText::owner(text)); }
        TaskUpdate::AddConstraint(text) => { bounded(&text, "constraint")?; task.constraints.push(OwnerText::owner(text)); }
        TaskUpdate::RemoveConstraint(index) => remove(&mut task.constraints, index, "constraint")?,
        TaskUpdate::AddDecision { text, status } => {
            bounded(&text, "decision")?;
            task.decisions.push(Decision { text, status, author: "owner".into() });
        }
        TaskUpdate::RemoveDecision(index) => remove(&mut task.decisions, index, "decision")?,
        TaskUpdate::AddArtifact(item) => { validate_artifact(&item)?; task.artifacts.push(item); }
        TaskUpdate::RemoveArtifact(index) => remove(&mut task.artifacts, index, "artifact")?,
        TaskUpdate::AddIssue(text) => { bounded(&text, "unresolved issue")?; task.unresolved_issues.push(OwnerText::owner(text)); }
        TaskUpdate::RemoveIssue(index) => remove(&mut task.unresolved_issues, index, "unresolved issue")?,
    }
    Ok(())
}

pub fn request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let input = format!("{}:{nanos}:{}", std::process::id(), sequence);
    format!("r-{}", &hex(&Sha256::digest(input.as_bytes()))[..24])
}

fn create_fingerprint(objective: &str) -> String {
    hex(&Sha256::digest(format!("create:{objective}").as_bytes()))
}

fn fingerprint(update: &TaskUpdate) -> String {
    let wire = match update {
        TaskUpdate::Objective(text) => serde_json::json!(["objective", text]),
        TaskUpdate::NextAction(text) => serde_json::json!(["next_action", text]),
        TaskUpdate::AddConstraint(text) => serde_json::json!(["add_constraint", text]),
        TaskUpdate::RemoveConstraint(index) => serde_json::json!(["remove_constraint", index]),
        TaskUpdate::AddDecision { text, status } => serde_json::json!(["add_decision", text, status]),
        TaskUpdate::RemoveDecision(index) => serde_json::json!(["remove_decision", index]),
        TaskUpdate::AddArtifact(item) => serde_json::json!(["add_artifact", item]),
        TaskUpdate::RemoveArtifact(index) => serde_json::json!(["remove_artifact", index]),
        TaskUpdate::AddIssue(text) => serde_json::json!(["add_issue", text]),
        TaskUpdate::RemoveIssue(index) => serde_json::json!(["remove_issue", index]),
    };
    hex(&Sha256::digest(serde_json::to_vec(&wire).expect("task updates serialize")))
}

fn apply_v0(old: TaskV0) -> Task {
    Task {
        schema_version: SCHEMA_VERSION, id: old.id, revision: old.revision, repository: old.repository,
        objective: OwnerText::owner(old.objective),
        constraints: old.constraints.into_iter().map(OwnerText::owner).collect(), decisions: Vec::new(),
        next_action: old.next_action.map(OwnerText::owner), artifacts: old.artifacts,
        unresolved_issues: old.unresolved_issues.into_iter().map(OwnerText::owner).collect(),
        jobs: old.jobs, requests: Vec::new(), created_unix: old.created_unix, updated_unix: old.updated_unix,
    }
}

fn decode(bytes: &[u8]) -> Result<Task> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| TaskError::Corrupt(e.to_string()))?;
    let version = value.get("schema_version").and_then(serde_json::Value::as_u64)
        .ok_or_else(|| TaskError::Corrupt("missing integer schema_version".into()))?;
    match version {
        version if version == SCHEMA_VERSION as u64 => serde_json::from_slice(bytes).map_err(|e| TaskError::Corrupt(e.to_string())),
        0 => serde_json::from_slice::<TaskV0>(bytes).map(apply_v0).map_err(|e| TaskError::Corrupt(e.to_string())),
        newer if newer > SCHEMA_VERSION as u64 => Err(TaskError::UnsupportedVersion(newer.min(u32::MAX as u64) as u32)),
        _ => Err(TaskError::Corrupt(format!("unknown schema version {version}"))),
    }
}

fn read_task(dir: &Path, id: &str) -> Result<Task> {
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(TaskError::Corrupt(format!("{} is a symlink", dir.display())));
    }
    let path = task_json(dir);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(TaskError::Corrupt(format!("{} is a symlink", path.display())));
    }
    let bytes = fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => TaskError::NotFound(id.into()),
        _ => TaskError::Io(format!("{}: {e}", path.display())),
    })?;
    let task = decode(&bytes)?;
    if task.id != id {
        return Err(TaskError::Corrupt(format!("{} holds task {}, not {id}", path.display(), task.id)));
    }
    task.validate()?;
    Ok(task)
}

fn write_task(dir: &Path, task: &Task) -> Result<()> {
    task.validate()?;
    let bytes = serde_json::to_vec_pretty(task).map_err(|e| TaskError::Invalid(e.to_string()))?;
    let path = task_json(dir);
    let mut last = None;
    for sequence in 0..100 {
        let tmp = dir.join(format!("task.json.tmp-{}-{sequence}", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = match options.open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { last = Some(e); continue; }
            Err(e) => return Err(TaskError::Io(format!("{}: {e}", tmp.display()))),
        };
        let written = (|| -> std::io::Result<()> {
            let mut file = file;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(TaskError::Io(format!("{}: {e}", path.display())));
        }
        return Ok(());
    }
    Err(TaskError::Io(format!("cannot create a temporary task file beside {}: {last:?}", path.display())))
}

fn lock(dir: &Path) -> Result<fs::File> {
    if !dir.is_dir() {
        return Err(TaskError::NotFound(dir.file_name().unwrap_or_default().to_string_lossy().into_owned()));
    }
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(TaskError::Corrupt(format!("{} is a symlink", dir.display())));
    }
    let path = dir.join("task.lock");
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(&path).map_err(|e| TaskError::Io(format!("{}: {e}", path.display())))?;
    lock.lock().map_err(|e| TaskError::Io(format!("{}: {e}", path.display())))?;
    Ok(lock)
}

fn task_json(dir: &Path) -> PathBuf {
    dir.join("task.json")
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| TaskError::Io(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| TaskError::Io(e.to_string()))?;
    }
    Ok(())
}

fn valid_task_id(id: &str) -> Result<()> {
    if id.len() != 26 || !id.starts_with("t-") || !id[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(TaskError::Invalid(format!("invalid task id {id:?}")));
    }
    Ok(())
}

fn valid_request_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_REQUEST_ID || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(TaskError::Invalid("a request id is 1-128 letters, digits, '-' or '_'".into()));
    }
    Ok(())
}

fn owner_text(item: &OwnerText, what: &str) -> Result<()> {
    bounded(&item.text, what)?;
    if item.provenance != "owner" {
        return Err(TaskError::Corrupt(format!("{what} has unsupported provenance {:?}", item.provenance)));
    }
    Ok(())
}

fn bounded(text: &str, what: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > MAX_TEXT {
        return Err(TaskError::Invalid(format!("{what} must be nonempty and at most {MAX_TEXT} UTF-8 bytes")));
    }
    Ok(())
}

fn validate_artifact(item: &ArtifactRef) -> Result<()> {
    bounded(&item.reference, "artifact reference")?;
    if let Some(identity) = &item.identity {
        bounded(identity, "artifact identity")?;
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An absolute path with symlinks resolved, including the existing ancestors of a tail not created yet.
fn resolved(path: &Path) -> PathBuf {
    let path = std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf());
    let mut tail = Vec::new();
    let mut head = path.as_path();
    loop {
        if let Ok(real) = head.canonicalize() {
            return tail.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => { tail.push(name.to_os_string()); head = parent; }
            _ => return path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ringo-task-{name}-{}-{}", std::process::id(), request_id()));
            fs::create_dir_all(path.join("repo")).unwrap();
            Self(path)
        }
        fn store(&self) -> TaskStore {
            TaskStore::new(&self.0.join("state"), self.0.join("repo").canonicalize().unwrap(), true).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn task(temp: &Temp) -> Task {
        temp.store().create("create-request", "  Preserve the original objective.  ").unwrap()
    }

    #[test]
    fn tasks_and_evidence_references_survive_reopening() {
        let temp = Temp::new("round-trip");
        let store = temp.store();
        let made = task(&temp);
        assert_eq!((made.schema_version, made.revision, made.objective.text.as_str()),
                   (1, 1, "  Preserve the original objective.  "));
        let revised = store.update(&made.id, 1, "constraint-1", TaskUpdate::AddConstraint("Do not change auth.".into())).unwrap();
        let revised = store.update(&made.id, revised.revision, "decision-1", TaskUpdate::AddDecision {
            text: "Use the existing context builder.".into(), status: DecisionStatus::Accepted,
        }).unwrap();
        let revised = store.update(&made.id, revised.revision, "next-1", TaskUpdate::NextAction("Add narrow tests.".into())).unwrap();
        let revised = store.update(&made.id, revised.revision, "artifact-1", TaskUpdate::AddArtifact(ArtifactRef {
            reference: "src/main.rs".into(), identity: Some("tree:abc123".into()),
        })).unwrap();
        let revised = store.update(&made.id, revised.revision, "issue-1", TaskUpdate::AddIssue("Need an owner decision on scope.".into())).unwrap();
        drop(store);
        let loaded = temp.store().load(&made.id).unwrap();
        assert_eq!(loaded, revised);
        let view = loaded.concise_view();
        assert!(view.contains(&made.id) && view.contains("Do not change auth.")
                && view.contains("[accepted] Use the existing context builder.")
                && view.contains("src/main.rs @ tree:abc123") && view.contains("Need an owner decision on scope."));
    }

    #[test]
    fn duplicate_requests_are_idempotent_and_cannot_be_reused_for_another_change() {
        let temp = Temp::new("requests");
        let store = temp.store();
        let made = task(&temp);
        assert_eq!(store.create("create-request", "  Preserve the original objective.  ").unwrap(), made);
        assert!(matches!(store.create("create-request", "Another objective"), Err(TaskError::RequestConflict(_))));
        let one = store.update(&made.id, 1, "same-request", TaskUpdate::AddConstraint("one".into())).unwrap();
        let again = store.update(&made.id, 1, "same-request", TaskUpdate::AddConstraint("one".into())).unwrap();
        assert_eq!(one, again);
        assert_eq!(again.revision, 2);
        assert!(matches!(store.update(&made.id, again.revision, "same-request", TaskUpdate::AddConstraint("two".into())),
                         Err(TaskError::RequestConflict(_))));
        assert_eq!(store.load(&made.id).unwrap().constraints.len(), 1);
    }

    #[test]
    fn no_op_requests_keep_their_receipt_after_later_updates() {
        let temp = Temp::new("no-op-requests");
        let store = temp.store();
        let made = task(&temp);
        let update = TaskUpdate::Objective(made.objective.text.clone());
        let accepted = store.update(&made.id, 1, "no-op", update.clone()).unwrap();
        assert_eq!(accepted.revision, 2);
        let later = store.update(&made.id, 2, "later", TaskUpdate::Objective("New objective".into())).unwrap();
        assert_eq!(store.update(&made.id, 1, "no-op", update).unwrap(), later);
        assert!(matches!(store.update(&made.id, later.revision, "no-op", TaskUpdate::AddIssue("different".into())),
                         Err(TaskError::RequestConflict(_))));
    }

    #[test]
    fn simultaneous_stale_updates_cannot_lose_either_constraint_or_decision() {
        let temp = Temp::new("concurrent");
        let made = task(&temp);
        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for (request, update) in [
            ("constraint-race", TaskUpdate::AddConstraint("keep permissions".into())),
            ("decision-race", TaskUpdate::AddDecision { text: "retain undo".into(), status: DecisionStatus::Accepted }),
        ] {
            let store = temp.store();
            let start = start.clone();
            let id = made.id.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                store.update(&id, 1, request, update)
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| matches!(result, Err(TaskError::Conflict { .. }))).count(), 1);
        let current = temp.store().load(&made.id).unwrap();
        assert_eq!(current.revision, 2);
        let loser = if current.constraints.is_empty() {
            TaskUpdate::AddConstraint("keep permissions".into())
        } else {
            assert!(current.decisions.is_empty());
            TaskUpdate::AddDecision { text: "retain undo".into(), status: DecisionStatus::Accepted }
        };
        temp.store().update(&made.id, current.revision, "retry-loser", loser).unwrap();
        let current = temp.store().load(&made.id).unwrap();
        assert_eq!(current.constraints.len(), 1);
        assert_eq!(current.decisions.len(), 1);
    }

    #[test]
    fn corrupt_and_future_records_are_recoverable_and_never_rewritten() {
        let temp = Temp::new("damaged");
        let made = task(&temp);
        let path = temp.store().dir.join(&made.id).join("task.json");
        for (contents, future) in [(b"{\"schema_version\":".as_slice(), false),
                                   (br#"{"schema_version":2,"id":"future"}"#.as_slice(), true)] {
            fs::write(&path, contents).unwrap();
            let error = temp.store().load(&made.id).unwrap_err();
            assert_eq!(matches!(error, TaskError::UnsupportedVersion(2)), future, "{error}");
            let update = temp.store().update(&made.id, made.revision, "must-not-write", TaskUpdate::AddConstraint("lost".into()));
            assert!(update.is_err());
            assert_eq!(fs::read(&path).unwrap(), contents, "inspection and mutation leave the original bytes alone");
        }
    }

    #[test]
    fn strict_v1_validation_and_v0_migration_preserve_state() {
        let temp = Temp::new("schema");
        let made = task(&temp);
        let path = temp.store().dir.join(&made.id).join("task.json");
        let original = fs::read(&path).unwrap();
        let unknown = String::from_utf8(original.clone()).unwrap().replace("\"revision\": 1", "\"revision\": 1,\n  \"unknown\": \"must survive\"");
        fs::write(&path, unknown).unwrap();
        assert!(matches!(temp.store().load(&made.id), Err(TaskError::Corrupt(_))));
        fs::write(&path, &original).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let legacy = serde_json::json!({
            "schema_version": 0, "id": made.id, "revision": 1, "repository": value["repository"],
            "objective": made.objective.text, "constraints": ["keep undo"], "next_action": "continue",
            "artifacts": [], "unresolved_issues": ["question"], "jobs": [],
            "created_unix": made.created_unix, "updated_unix": made.updated_unix,
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let migrated = temp.store().load(&made.id).unwrap();
        assert_eq!(migrated.schema_version, 1);
        assert_eq!(migrated.objective.provenance, "owner");
        assert_eq!(migrated.constraints[0].text, "keep undo");
        assert_eq!(migrated.unresolved_issues[0].text, "question");
        assert!(fs::read(&path).unwrap().starts_with(b"{\"schema_version\":0,") || fs::read(&path).unwrap().starts_with(b"{\n  \"schema_version\": 0"),
                "inspection does not persist migration");
        let updated = temp.store().update(&made.id, 1, "after-migration", TaskUpdate::AddConstraint("also keep mode".into())).unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(temp.store().load(&made.id).unwrap().constraints.len(), 2);
    }

    #[test]
    fn repository_binding_and_private_containment_are_enforced() {
        let temp = Temp::new("repository");
        let made = task(&temp);
        fs::create_dir_all(temp.0.join("other")).unwrap();
        let other = TaskStore::new(&temp.0.join("state"), temp.0.join("other").canonicalize().unwrap(), true).unwrap();
        assert!(matches!(other.load(&made.id), Err(TaskError::NotFound(_))));
        assert!(TaskStore::new(&temp.0.join("repo/state"), temp.0.join("repo").canonicalize().unwrap(), true).is_err());
        let copied = temp.0.join("moved");
        fs::create_dir_all(&copied).unwrap();
        let wrong = TaskStore { repository: RepositoryBinding { path: copied, git: true }, dir: temp.store().dir };
        assert!(matches!(wrong.load(&made.id), Err(TaskError::WrongRepository { .. })));
    }

    #[test]
    fn identifiers_and_owner_updates_are_validated_without_touching_state() {
        let temp = Temp::new("validation");
        let store = temp.store();
        let made = task(&temp);
        assert!(store.load("../other").is_err());
        assert!(store.update(&made.id, 1, "bad request", TaskUpdate::NextAction("x".into())).is_err());
        assert!(store.update(&made.id, 1, "empty", TaskUpdate::Objective("  ".into())).is_err());
        assert_eq!(store.load(&made.id).unwrap().revision, 1);
        assert!(store.update(&made.id, 1, "remove-missing", TaskUpdate::RemoveIssue(1)).is_err());
        let unchanged = store.load(&made.id).unwrap();
        assert_eq!(unchanged, made);
    }
}

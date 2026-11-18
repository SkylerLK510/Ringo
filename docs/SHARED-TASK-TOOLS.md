# Shared tasks, assignments and evidence

Ringo can persist an owner task, hand a bounded assignment to a fresh or resumed agent conversation, register
immutable evidence snapshots and serve both agents the same evidence through a read-only gateway. This page
describes what exists, the state it keeps, the wire shapes and the limits. The shell reference (`SHELL.md`) covers
`/task` itself; the plan is `RINGO-TASK-ENGINE-PLAN.md`.

Scope of this milestone:

| Capability | State |
| --- | --- |
| Persistent repository-bound tasks, owner updates, attached agent jobs | Phase 1, `/task ...` |
| Versioned full and delta assignments with durable acknowledgement | Implemented |
| Registered immutable evidence with opaque, task-scoped IDs | Implemented |
| Read-only `discover`, `task_read`, `evidence_read`, `batch_read` | Implemented |
| `ringo tools` stdin/stdout adapter | Implemented |
| Configured checks and model-producing reviews | Not in this milestone |
| MCP transport, JavaScript runtime, arbitrary shell, RAG, automatic routing, local agent loop | Deliberately absent |

## Where state lives

Everything stays outside the repository, in the state root (`RINGO_RELEASE_HOME`, else
`~/.local/share/ringo`):

```text
tasks/<sha256(repository)>/                    one task per folder
task-assignments/<sha256(repository)>/<task>/  assignment records and native-thread cursors
task-evidence/<sha256(repository)>/<task>/
  blobs/                                       immutable snapshot bytes
  records/                                     one opaque record per snapshot
  latest/                                      one pointer per registration origin
  published/                                   one marker per issued evidence ID
```

The modules are `cli/src/task.rs` (unchanged by this milestone), `cli/src/task_assignment.rs`,
`cli/src/evidence.rs` and `cli/src/gateway.rs`. The shell wires assignments in `cli/src/lib.rs`.

## Assignment context

`AssignmentStore::prepare(task, agent, native_thread, policy)` builds a deterministic assignment and persists it
before dispatch:

- A **full** assignment (no acknowledged cursor on that native thread) carries the exact owner objective,
  constraints, accepted decisions, next action, unresolved issues, registered artifact metadata and whole attributed
  agent replies. Proposed decisions are reported as an omission, never promoted to owner instructions.
- A **delta** assignment (the thread acknowledged an earlier assignment) carries only what changed since that
  acknowledged revision, occurrence-aware, so duplicates and modified sections behave sensibly. A delta with
  unchanged required state is normally not sent; an undelivered claim that now fits under a larger budget does make
  one (a claim-only delta), while a claim that still does not fit is not repeated as an unchanged omission notice.
- Required owner state is never truncated. If it does not fit `DEFAULT_CONTEXT_BUDGET` (32 KiB), preparation fails
  with an actionable `RequiredStateOverflow` error naming the needed and allowed bytes. Nothing is sent, no
  assignment or cursor is written, no evidence identity is published and no model call is made. Immutable snapshot
  bytes may already exist in the private content-addressed cache; they remain undiscoverable without a published
  pointer. As with any launch that fails before dispatch, the shell records the turn as failed (and a write turn may
  already have an unused worktree).
- Optional agent claims are whole, bounded excerpts (4 KiB each). An omitted claim is listed in the manifest with its
  reference, evidence ID, byte count and reason; if the omission row itself would not fit, the oldest included claims
  are evicted with their own omission rows. Only required state plus full honest accounting can fail closed.
- The assignment ID is content-derived (`a-<24 hex>`), and the rendered text includes an inclusion/omission manifest
  plus the exact task revision.

Conflicting delivery acknowledgements invalidate the native thread baseline, so its next assignment is a full refresh.
Acknowledgement is separate from preparation. A prepared assignment advances a native thread's cursor only after a
provider note proves the model worked in that turn — a step, a tool action, a partial reply, a reply, or usage this
turn actually added. A created turn or thread, process start, or a zero-token usage report does not acknowledge
delivery. A cumulative usage report counts as work only when this process has already observed that thread's earlier
total: after a restart the restored watermark can lag a turn that crashed after the provider advanced, so the first
cumulative report is history, not evidence, and that turn needs a model action or reply to acknowledge. A launch that
fails before dispatch — including a failure to create the job's stderr file — leaves the job failed (not running),
and both the cursor and the conversation retryable. Parser events are scoped to the running thread (the
parser is fresh per turn and ignores events whose thread id differs), so notes from another thread cannot
acknowledge a turn; cumulative usage is additionally gated by the per-process observation set described above.

## Evidence identities

A task artifact reference or attached job log path is a **label, not a read capability**. Registration:

1. opens the source beneath its authorized root without following symlinks (repository and job roots use a strict component-by-component
   `openat(O_NOFOLLOW)` walker),
2. copies at most 1 MiB into private state,
3. issues `e-<24 hex>`, derived from task, origin and content digest.

Registration is idempotent for unchanged content. An owner artifact origin is derived from its reference, not its
position, so removing an earlier artifact can never rebind a later one to an old snapshot. A declared identity that
no longer matches the live bytes yields `stale` and issues no new capability. A missing job output recovers its last
snapshot. Superseded snapshots stay readable by their own IDs; there is no automatic deletion in this milestone.
Missing or corrupt snapshots are reported (`missing_snapshot`, `corrupt_snapshot`) and are never replaced by the live
source.

Registration is split into staging and publication. Staging writes the immutable bytes and records but issues no
discoverable identity; publication writes a per-ID marker under `published/` and the latest pointer. An assignment
preparation stages first and publishes only after every budget check passes, so a refused preparation leaves at most
a reusable private cache. A staged ID reads as unknown — even to a caller that can derive it — until its marker
exists; a latest pointer naming a never-published record is reported unavailable, not followed.

`EvidenceStore::registered(task)` lists the latest registration per origin without registering anything. A pointer
that is a symlink, unreadable or names a record outside the task's scope is listed as `unavailable`, not followed.

## Read-only gateway

`Gateway::new(state_root, repository)` is transport-independent. `dispatch(&Value)` accepts a parsed request and
`dispatch_bytes(&[u8])` bounds the raw input before parsing. Neither makes a model call, runs a shell, mutates a task
or starts nested work. The read path is also mutation-free on disk: the stores validate scope and compute paths
without creating directories or locks, and reads only open and validate existing files.

### Envelope

Request:

```json
{ "protocol": 1, "request_id": "r-1", "op": "evidence_read", "params": { "task_id": "t-...", "evidence_id": "e-..." } }
```

Response, success:

```json
{
  "protocol": 1, "request_id": "r-1", "op": "evidence_read", "ok": true, "task_revision": 7,
  "result": {
    "task_id": "t-...", "evidence_id": "e-...", "digest": "sha256:...", "total_bytes": 4096,
    "offset": 0, "bytes_returned": 4096, "truncated": false, "encoding": "utf8", "content": "..."
  }
}
```

Response, refusal:

```json
{ "protocol": 1, "request_id": "r-1", "op": "evidence_read", "ok": false, "task_revision": null,
  "error": { "code": "unknown_evidence", "detail": "no registered evidence e-... belongs to task t-..." } }
```

`ok` is the authority; a structured refusal is still a completed request. A refusal echoes `request_id` and `op`
only when they are within their validated bounds, so an oversized request cannot make its own refusal exceed the
response limit.

### Operations

| Op | Parameters | Result |
| --- | --- | --- |
| `discover` | none | tools, parameters and host limits; kept under 2 KiB |
| `task_read` | `task_id`, `task_revision?` | bounded owner-state projection and registered evidence IDs; no private job paths, directories or request receipts |
| `evidence_read` | `task_id`, `evidence_id`, `max_bytes?`, `offset?`, `cursor?`, `assignment_id?` | one bounded slice of an immutable snapshot |
| `batch_read` | `steps` (≤ 8 `{tool, params}`) | independent `task_read`/`evidence_read` results with per-step status and `complete` |

`evidence_read` details:

- `max_bytes` must be at least 1 and is clamped to the 16 KiB read cap; the default is the cap.
- Every read verifies the whole snapshot (length and digest) before slicing it, so a same-length corruption is
  `corrupt_snapshot` even on a page that does not cover the changed bytes. A snapshot whose file length no longer
  matches its registration is refused the same way, never a short page with a cursor that cannot advance. Text is
  returned as `utf8`, other bytes as unpadded base64url.
- Valid UTF-8 with excessive JSON escaping is also returned as base64, keeping control-character pages within the response limit.
- `truncated` and `next_cursor` describe paging. A cursor binds protocol, task, evidence ID, digest and offset; a
  cursor naming other content or scope is `stale_cursor`. Cursors survive a gateway restart because they name
  immutable content, not process state.
- With `assignment_id`, the read is limited to the evidence IDs that assignment and its acknowledged base chain
  told that agent about. Each assignment persists that cumulative grant, so a long-lived thread's scoped read stays
  one record deep instead of failing at an arbitrary chain depth. An unknown or malformed assignment is refused; a
  broken chain is an error, never a wider grant. Without it, the read is task-scoped, which is the host's coarser
  grant. An `assignment_id` is an opaque capability: knowing a valid one grants the scoped read, because this
  milestone has no host identity to bind it to. The host is expected to pass only the ID it already gave that
  agent, and a future identity-bound milestone can tighten this without changing the wire shape.
- A parameter that is present but malformed (a null `assignment_id`, a string `task_revision`, a numeric `cursor`) is
  refused with `invalid_request`; it is never ignored in favor of a wider scope or an unchecked read.

### Host limits (fixed in code)

| Limit | Value |
| --- | --- |
| Batch steps | 8 |
| Concurrency per gateway instance | 2, excess refused `busy` (no queue) |
| Aggregate response envelope | 32 KiB |
| One read | 16 KiB |
| Request | 16 KiB |
| Request deadline | 10 s |

A batch runs steps in order under one deadline and one payload budget. Steps are independent, may fail or be skipped
individually, and are explicitly **not atomic**. A step that does not fit is reported as `{"status":"skipped",
"reason":"budget_exhausted"|"deadline_exceeded"}`. Each step's `tool` label is bounded to 24 characters, so a long
unknown tool name inside an otherwise valid request cannot push the response past its aggregate bound and erase
earlier successful results. The deadline is a soft cutoff for synchronous I/O: the gateway
stops starting steps, and a step that finishes past the bound keeps its result with `"deadline_exceeded": true` and
makes the batch incomplete (`complete: false`, plus a batch-level `deadline_exceeded`). A host that needs hard
cancellation bounds the process itself. Error detail is bounded to 512 bytes. Evidence content is untrusted data: it
is returned as a JSON field, never interpreted as instructions or paths.

### Refusal codes

`unknown_task`, `wrong_repository`, `corrupt_task`, `stale_task_revision`, `unknown_evidence`, `unknown_assignment`,
`corrupt_assignment`, `missing_snapshot`, `corrupt_snapshot`, `invalid_cursor`, `stale_cursor`, `budget_exhausted`,
`deadline_exceeded`, `busy`, `request_too_large`, `invalid_request`, `internal`.

## Command adapter

`ringo tools` reads exactly one request from stdin and writes exactly one response to stdout. It binds the enclosing
git project (as the shell does) unless `--repo` is given, and uses the normal state root unless `--state` is given.

```sh
$ printf '%s' '{"protocol":1,"request_id":"d","op":"discover"}' | ringo tools
{"protocol":1,"request_id":"d","op":"discover","task_revision":null,"ok":true,"result":{"tools":[...],"limits":{...}}}

$ printf '%s' '{"protocol":1,"request_id":"b","op":"batch_read","params":{"steps":[
    {"tool":"task_read","params":{"task_id":"t-..."}},
    {"tool":"evidence_read","params":{"task_id":"t-...","evidence_id":"e-..."}}]}}' \
  | ringo tools --repo /path/to/project
{"protocol":1,"request_id":"b","op":"batch_read","task_revision":7,"ok":true,
 "result":{"results":[{"tool":"task_read","status":"ok","result":{...}},
                      {"tool":"evidence_read","status":"ok","result":{...}}],"complete":true}}
```

The adapter always exits 0 once it has written a response; exit 2 means it could not (bad arguments, unreadable
input, unwritable output, or a state root that cannot be opened). It is allowed for shell agents under `RINGO_CHILD`.

## Adapter limitations

- There is no MCP server, no JavaScript runtime, no arbitrary shell and no unrestricted MCP federation. Hosts that
  cannot call a command get the rendered assignment text as a bounded textual fallback and nothing more; support is
  not claimed to be identical across providers.
- `task_read` is task-scoped. It reports evidence IDs, digests and sizes, not contents, and it never emits private job
  directories, request receipts or file-read capabilities. Narrow grants belong to `evidence_read`'s
  `assignment_id`.
- No configured checks and no model-producing reviews are part of this milestone; no call count is claimed for them.
- Batches make zero model calls. Any token or round-trip savings are hypotheses to be measured, not promises.

## Migration notes

- `cli/src/task.rs` and its persisted task schema are unchanged; existing `/task` state keeps working.
- The new modules add state only under `task-assignments/` and `task-evidence/`.
- Artifact registration origins are now derived from the artifact reference. If an earlier build of this branch
  registered artifacts positionally (`artifact:<index>`), those pointers remain harmless private state; a later
  registration writes the new origin. Old snapshots stay in the private cache and become readable again once a
  registration publishes them (see the publication note below).
- Evidence IDs are issued by per-ID publication markers. State written by an earlier build of this branch has no
  markers: those IDs read as unknown until a later registration of the same task stages and publishes them again
  (unchanged content reuses the same immutable bytes and IDs). Assignment records written before cumulative grants
  existed are walked once within the reader's bound, then persisted with the grant.
- Superseded snapshots are retained. Deleting them is a future, explicit retention policy, not a silent behavior.

## Validation

```sh
cargo test --lib
cargo test --bins
cargo test --test engine
cargo test --test gate_scenarios --features turnrun
python3 -m unittest discover -s tests -q
```

The integration test `a_read_only_batch_serves_the_same_evidence_across_agents_and_a_restart_without_a_model_call`
covers the acceptance path: task → bounded handoff naming evidence IDs → batch through `ringo tools` → structured
refusal → shell restart → the same revision and evidence bytes, with the fake provider's call log unchanged.

# Ringo task engine: implementation handoff

## Objective

Make Ringo own the task, its evidence, handoffs and verification while keeping
Claude Code, Codex and other supported engines as execution backends. A user
should be able to start a coding task with Claude, continue it with Codex, restart
Ringo, and obtain a checked result without reconstructing the work manually.

This document is an implementation plan for OpenCode. The proposed interfaces
below are designs, not features that exist today. Implement in small, reviewable
increments. Finish the first complete workflow before extending to other task
types or adding autonomous orchestration.

## Read first and preserve

- Follow `AGENTS.md` and `skills/ringo-harness/SKILL.md`.
- Read relevant sections of `docs/SHELL.md`, `docs/TOKEN-USAGE.md`,
  `docs/NATIVE-RUNTIME.md` and `docs/RELEASES.md` as needed.
- Keep private task state, prompts, reports, traces and credentials outside the
  checkout. Synthetic test fixtures may be committed; real task contents may not.
- Preserve subscription authentication, permission modes, process cancellation,
  isolation, turn undo and native resume/compaction behavior.
- Preserve protected evaluation, Council decision boundaries and human adoption.
  Do not activate a release as part of implementation.
- Python runtime changes use Python 3.11+ and the standard library. Maintain
  native/reference parity where shared runtime behavior changes.
- Do not recursively launch a harness from a Ringo child. Keep `RINGO_CHILD`
  protection. Host-mediated operations below are explicit requests to the
  existing supervisor, not permission to spawn Ringo inside an agent.

## Existing building blocks

Inspect these before adding replacements:

| Existing component | Location and intended reuse |
| --- | --- |
| Sessions, jobs, handoffs and agent launch | `cli/src/lib.rs`: `Session`, `Job`, launch and record logic |
| Agent events and usage | `cli/src/parse.rs` and current adapter handling |
| Shell commands and display | `cli/src/tui.rs`, `cli/src/tui/`, `cli/src/main.rs` |
| Agent-to-host connection | `serve_asks`, `ask`, `Session::take_asks` in `cli/src/lib.rs` |
| Runtime and artifact checking | `cli/src/rt/`; reference `harness.py`, `workflow.py`, `context.py`, `backends.py` |
| Review of working changes | Existing shell `/review` snapshot and invocation path |
| Context assembly and completion | Existing repository context builder, blocking `wait`, compact summaries |

Current Claude and Codex adapters run external agent engines. Those engines
still own internal reasoning, tool execution and much of context loading. This
plan improves host ownership without claiming complete control over those loops.

## Architecture and contracts

Introduce a versioned, private task store separate from provider conversations.
A task references existing jobs, sessions and artifacts; it does not duplicate
all their logs. Use a small module boundary rather than expanding `lib.rs` with
another large subsystem. Proposed module names may change to fit the repository.

Minimum task state:

- Stable task ID, schema version, revision and repository identity.
- Owner objective and constraints, retaining original wording and provenance.
- Proposed decisions, accepted decisions and unresolved questions, distinguished
  explicitly. Agent statements never become owner instructions automatically.
- Work items with pending, active, blocked and completed states.
- Artifact references with repository revision or content identity.
- Check/review evidence with command, scope, result, identity and originating job.
- Current assignment, previous assignments and immediate next action.
- Continuation notes attributed to their author; these are claims unless verified.

Store evidence references and bounded excerpts, not transcripts in every field.
Distinguish execution state (running, stopped, failed) from task outcome
(unfinished, blocked, ready for human review, accepted). A process exiting
successfully is insufficient evidence that a task is complete.

All mutations use an expected task revision, atomic persistence and request IDs.
Concurrent or stale updates produce a conflict response; they never silently
overwrite newer state. Repeated delivery of one request must not repeat a check
or model call. Persist operation identity before dispatch and reconcile uncertain
outcomes after restart instead of automatically replaying them.

## Phase 1: persistent tasks and manual continuity

Deliver a task store, schema validation, persistence and a minimal shell interface
to create, inspect, select and resume a task. Attach existing agent jobs to a task.
Continue supporting ordinary conversations without creating a task automatically.

Expose objective, constraints, next action, current artifacts and unresolved
issues in a concise task view. Initially the owner can update these directly;
agents can propose updates in the later tool phase. Define schema migration and
an explicit refusal for unsupported newer formats. Do not silently discard state.

Acceptance:

- A task survives quit/restart with the same ID and evidence references.
- Corrupt or incompatible state reports a recoverable error without overwriting it.
- Two simultaneous updates cannot lose a constraint or decision.
- A task is bound to its repository; selection in another repository is refused
  or requires an explicit rebind with stale evidence marked.
- Existing chat, undo and resume behavior still works without tasks enabled.

## Phase 2: task context and cross-agent handoff

Build one deterministic context assembler for assignments and handoffs. Include
the objective, applicable owner constraints, accepted decisions, active work,
relevant evidence, unresolved issues and requested next action. Keep an inclusion
manifest identifying omitted material and how to retrieve it.

Use native conversation resume for ongoing work. Supply task changes since the
agent's last acknowledged task revision instead of repeating the whole task on
every resumed turn. Send a complete bounded assignment on a new agent/thread.
Do not pretend Ringo can remove history already retained by the provider engine.

Define explicit byte budgets and omission priorities in configuration. Bytes
are not tokens. Never silently truncate a required constraint: if required state
does not fit, report the conflict and ask for scope adjustment. Optional evidence
can be omitted with retrieval references. Detect stale artifact identities before
using evidence; preserve the historical result while marking its applicability.

Acceptance:

- Claude-to-Codex and Codex-to-Claude handoffs preserve the same owner constraints,
  active work and unresolved issues using fake adapters first.
- A resumed agent receives only new task updates, without losing pending updates
  when an assignment fails before delivery.
- Large histories produce bounded assignment text and an accurate manifest.
- Required-state overflow is explicit; Unicode boundaries remain valid.
- A changed artifact makes previous verification visibly stale.

## Phase 3: agent tools connected to the host

Extend the existing agent-to-host protocol. Keep transport changes separate from
task semantics and retain compatibility with existing `ask_agent` behavior.
Suggested operations:

| Operation | Behavior |
| --- | --- |
| `task_read` | Return bounded current state or changes since a known revision |
| `evidence_read` | Fetch bounded evidence by host-issued reference |
| `task_propose_update` | Submit attributed work/decision updates against an expected revision |
| `request_check` | Ask the host to run a configured check on a specified artifact identity |
| `request_review` | Ask the host for the existing bounded review workflow |
| `submit_result` | Submit artifacts, evidence and unresolved issues for human review |

Bind each tool connection to a task and assignment. Reject attempts to access
another task, arbitrary filesystem paths, unknown operations or unsupported
fields. Evidence references are not unrestricted file-read capabilities.
Check commands come from owner-authorized/project-configured definitions;
the tool must not become an arbitrary shell execution bypass. Honor existing
permissions and approvals. Agent proposals cannot change owner constraints,
adopt policy, accept their own results or disable verification requirements.

Return compact structured responses with revision, operation status, evidence
references and explicit errors. Keep long-running host work asynchronous with a
completion notification where supported; otherwise provide one bounded blocking
wait. Do not force repeated model status polling. Advertise actual adapter
capabilities; an unsupported tool path should have an explicit manual fallback.

Before implementing model-producing requests, write down and test their exact
call bounds. Integrate with existing plan/retry limits, not an independent
unlimited queue. One review request consumes one declared review slot for the
assignment/artifact; explicit new authorization is needed to create another slot.
Nested reviewers cannot delegate or request another review. Duplicate delivery,
reconnect, failure or timeout must not replenish slots. Include existing permitted
retry/probe behavior in the tested maximum and surface it in accounting.

Acceptance:

- Duplicate requests return the existing operation/result without rerunning it.
- Cross-task references, path traversal and stale updates are rejected.
- Cancellation stops host work through existing cancellation machinery.
- Restart reconciles an in-flight operation without unapproved reexecution.
- Exact maximum model calls are pinned in tests for success, retry, failure,
  duplicate requests and attempted recursion.

## Phase 4: completion backed by evidence

Connect submitted results to the actual artifact identity, check outcomes,
review findings and unresolved issues. Reuse existing findings formats and
verification behavior. Present one completion view with changes, checks,
remaining issues, usage and evidence links. Read detailed traces on demand.

Distinguish failed checks, missing checks, stale checks and unsupported checks.
Passing checks establish only their declared scope. Submission means ready for
human review; acceptance remains an owner action. Do not introduce Council
scoring into the default coding workflow.

Acceptance: an agent cannot mark an unverified or stale result as verified;
unresolved findings remain visible; failed and interrupted attempts retain their
usage and evidence; accepting a result records the owner's action separately.

## Phase 5: finish one complete workflow

Ship an opt-in path demonstrating:

1. Owner creates a coding task and constraints.
2. Claude makes a change and records a continuation proposal.
3. Owner switches to Codex, which receives current task state and relevant evidence.
4. Codex requests a configured check and one host-managed review.
5. Findings are resolved within the declared workflow bounds; changed artifacts
   invalidate earlier evidence as appropriate.
6. Ringo restarts and restores the task and completed operation references.
7. Owner inspects the final evidence and accepts or requests further work.

Use synthetic offline integration scenarios first. Add reverse-direction handoff,
an interrupted operation, a stale artifact and conflicting updates. Do not require
a real provider connection for routine tests.

Shorten `skills/ringo-harness/SKILL.md` into a routine entry path with focused
references for specialized operations, preserving all requirements. Explain host
tools versus direct CLI invocation clearly. Update user documentation around the
completed workflow, with no performance claims beyond available evidence.

## Measurement and rollout

Instrument assignments, host operations and provider calls with correlated IDs.
Reuse normalized usage accounting; do not count provider cumulative snapshots
twice or add reasoning output a second time. Report missing usage as unknown.
Task totals must distinguish attributed usage from unobservable outer work.

For a later bounded diagnostic study, compare standalone agents, the existing
shell and the opt-in task workflow on matched tasks. Pin models, effort, access,
quality criteria and starting conditions. Include cold starts, continuing work,
handoffs, failures, retries, compaction and recovery. Report task success,
quality, required owner intervention, latency and all reported token categories.
Declare any setup amortization. This plan does not launch a campaign or alter
protected evaluation rules. Missing outer usage prevents a whole-task savings
claim. Only the protected evaluation process can establish a measured improvement
under repository rules; human release adoption remains separate.

Keep the workflow opt-in until continuity and failure recovery are reliable.
Do not automatically route tasks among models, invent a dollar cap, add Council
calls, replace authentication, rewrite the agent engines or switch to direct
provider APIs as part of this implementation. Those require separate designs.

## Implementation and review instructions for OpenCode

Start with Phase 1 and its narrow persistence/compatibility tests; then deliver
each phase in a separate reviewable change. Establish protocol schemas and call
bounds before adding execution. Extend existing facilities instead of building
a second scheduler, context builder, evidence format or usage ledger.

Run `python3 -m unittest discover -s tests -q` after implementation changes.
Run appropriate Rust checks/tests for the affected CLI code, including relevant
integration tests; use `cargo test --manifest-path cli/Cargo.toml` for the complete
CLI suite when validating the integrated workflow. Shared runtime changes also
need the existing native/reference differential checks.

Obtain the required actual bounded harness review for final code when inference
is authorized. Retain reports outside the checkout and report the command,
backend, calls, tokens and evidence path. If executing inside a Ringo child,
request the host review; do not remove or bypass the recursion guard. If no host
review path exists yet, report that limitation explicitly rather than claiming
an offline fixture was a model review.

For every delivered phase, report implemented behavior, checks, remaining
limitations and the next phase. Record no improvement-log entry without eligible
raw measurement evidence and the required recomputation workflow.

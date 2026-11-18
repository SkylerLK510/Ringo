# OpenCode handoff: task checks and review continuity

Finish the next slice of the existing routing usability plan: make explicit checks and reviews visible and durable within a selected task. Read `AGENTS.md`, `skills/ringo-harness/SKILL.md`, `docs/OPENCODE-ROUTING-USABILITY-HANDOFF.md`, `docs/SHARED-TASK-TOOLS.md`, and the current code before changing anything. This is an implementation brief for the remaining workflow, not a replacement task-engine architecture.

## Starting point

Start a separate branch from current `origin/main`. At handoff creation, main is `780f990` and includes PR #84 (bounded task assignments, evidence and read-only gateway) and PR #85 (role assignments, capabilities and explicit worker starts). Verify their presence rather than copying code from old worktrees.

`/task start` runs exactly one worker turn. `/task progress` currently displays checks and review as unrecorded. Reviewer assignment exists, but does not yet launch a task-linked review. Agent job attachment and usage lookup exist; `JobRef::from_job` intentionally rejects pipeline jobs. Preserve that distinction when adding durable operation references.

## Deliver a small first PR

Implement explicit task checks first. Carry the existing plan forward without introducing automatic routing or a repair loop.

- Provide an explicit way for the owner to declare a check command and run it for the selected task. Document the exact command syntax and execution directory. Reuse existing process execution and cancellation facilities where suitable.
- Persist an operation identity, task/repository identity, command, source revision and relevant working-tree identity, timestamps, outcome, and durable output references outside the checkout. Record failures and cancellation as well as success. A commit hash alone must not imply that later uncommitted edits were checked.
- Display those results in `/task progress`, including stale results after edits. After restart, reconcile available evidence; a record that only says running must not become a successful check or be rerun automatically.
- Publish bounded check output through the existing evidence store. Preserve full-output availability or truncation/error information honestly. Keep commands, private output and task records out of committed fixtures and reports.
- Protect task updates against stale revisions and duplicate requests. Retrying the same launch request must not silently execute the command twice.
- Check execution makes zero model calls. A passed check does not accept the task or its artifacts.

Keep this PR independently useful. If safe restart reconciliation or launch idempotency requires a broader change, explain the limitation and narrow the exposed behavior rather than claiming exactly-once execution across a crash.

## Follow-up PR: task-linked review

Once the checks PR is reviewable, connect an explicit task review command to the existing review pipeline. Use the assigned reviewer only when its adapter can actually run that pipeline with the requested model and authentication. Refuse unsupported assignments before inference; never substitute another provider silently.

Link review runs and their findings to the task and reviewed source identity. Show pending, failed, cancelled, complete and stale reviews in progress. Preserve existing pipeline time bounds, retry/probe rules, cancellation cleanup and accounting. A review finding is evidence, not an owner instruction or acceptance decision. Do not automatically launch a worker, retry, repair, review or check because a prior operation completed.

If integrating reviewer selection cannot be done faithfully with the existing backend interface, document the concrete blocker and leave this as a separate follow-up instead of hiding a fallback.

## Constraints and pointers

- Relevant code: `cli/src/tui.rs` (`TaskCmd`, `TASK_USAGE`, `task_progress`, task start), `cli/src/task.rs`, `cli/src/lib.rs` (jobs, process lifecycle and usage), `cli/src/evidence.rs`, `cli/src/task_assignment.rs`, `cli/src/gateway.rs`, and existing native/Python review runners.
- Reuse the private state and evidence contracts. Preserve schema migration, repository binding, atomic writes, bounds, provenance and read-only gateway behavior.
- Preserve explicit model precedence and unknown/partial usage reporting. Checks have no inference usage; unavailable review usage is unknown, never zero.
- Keep `/task start` as one explicit worker turn. Do not activate a local model, download models, start a server, add an API fallback, build a new MCP transport, or change protected evaluation/policy gates.
- Update `docs/SHELL.md`, relevant user-guide commands and `TASK_USAGE` together.

## Acceptance and handback

Use fake processes/backends for ordinary tests. Cover check success, nonzero exit, cancellation and process cleanup, stale source identity, missing output, duplicate launch requests, conflicting updates and restart recovery. Verify private evidence stays outside the repository. For the review follow-up, test capability refusal before inference, explicit reviewer/model propagation and the exact bounded call structure.

Run the relevant Rust library, shell and integration tests and `python3 -m unittest discover -s tests -q`. Investigate failures; identify any baseline failures with a matching baseline run rather than ignoring them.

Before handing back implementation, run the actual bounded Ringo final review required by the skill. Keep material, reports and traces outside the checkout; wait on background work with `python3 ringo.py wait DIR`. Fix confirmed findings. Report the exact command, call count and reported token usage, marking incomplete totals honestly.

Open a PR with the behavior change, manual walkthrough, validation, review evidence and limitations. Do not merge or install unreviewed changes. Return the PR link and a short owner walkthrough: select task → run worker → run check → inspect progress → restart → inspect the same result.

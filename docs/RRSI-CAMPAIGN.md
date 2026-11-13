# Recursive turn-policy experiments: offline implementation

The infrastructure below implements the next, **offline** stage of the revised RRSI plan. It
does not enable live self-improvement. No live baseline, negative control, recursive model search,
policy adoption or release claim is supplied by this change. Existing review evaluation and
release certification remain separate.

The implementation follows Astra's revised order: isolate task execution **and grading**, exercise
actual Session transitions, constrain editable policy, and test the recursive controller before
authorizing an experiment. An inconclusive control is a stop, not permission to weaken the gate.

| Component | Executable behavior |
|---|---|
| `turnrun_isolation.py` | Disposable Docker worker; separate network-disabled checker; safe submission export; parent-death and lifetime cleanup. |
| `turnrun_broker.py` | Authenticated, leased MCP transport exposing one fixed worker tool; no provider launcher or native authentication. |
| `turnrun_certify.py` | Check discrimination against pristine historical snapshots inside the exact pinned grading image, with private hashed evidence. |
| `scenariorun` (`turnrun` feature) | Scripted turns, clear, restart, seeded handoffs, threshold boundaries and Claude compact through the actual `Session`. Offline providers only. |
| `cli/src/turn_policy.rs` | Explicit restricted policy loading. Empty policy preserves the normal runtime. |
| `turnrun_campaign.py` | Frozen three-way splits, controls, two sibling proposals, recursive incumbent advancement, complete dev history, separate budgets, validation and final holdout gates. |

## Isolation and protected grading

Only a trusted controller selects the immutable image ID/digest, task fixture and check. The
model-facing worker tool accepts `command` and `seconds` only. Commands execute inside its one
assigned container; requests cannot choose Docker arguments, mounts, executors or container IDs.

Workers have no task network, credentials, Docker socket, host home or evaluator mount. They run
as an unprivileged UID with a read-only root, all capabilities dropped, no-new-privileges, private
PID namespace, CPU/memory/PID limits and a fixed lifetime. Each trial receives its own volume;
there is no shared writable build cache. Only workspace and scratch storage are writable.

The controller stops the entire worker before copying its submission. An idle container keeps
the quota-backed workspace mounted until export completes. It has no tool endpoint and is removed
with the worker. The bounded archive
extractor rejects traversal, links, special files, duplicate paths and excessive submissions;
it removes `.git` metadata. No Git operation, import, build or other submitted code runs on the
host. A new checker receives only the submission, one read-only protected check and the pinned
runtime image. That check may import or build the submission **inside the checker**.

An independent watchdog removes a worker and its volume if its controller dies or its lease
expires. Ordinary context cleanup removes both immediately. The watchdog starts after container
creation; interruption during early setup may require removal of that specifically labelled
resource. This is not a guarantee against arbitrary Docker-daemon failure.

Each workspace is a daemon-side tmpfs volume with a byte quota (512 MiB by default; controller
may select 1 MiB–1 GiB) and 32,768 inodes. Startup verifies both the volume options and the actual
mounted filesystem limits. Scratch space is separately limited to 256 MiB and 32,768 inodes;
shared memory is 16 MiB, container memory is 2 GiB with swap disabled, and Docker logging is
disabled to prevent unbounded log files. The idle keeper preserves the volume after worker
shutdown. A daemon that cannot establish these mounts fails closed; there is no ordinary-volume
fallback. [Docker tmpfs documentation](https://docs.docker.com/engine/storage/tmpfs/) describes
the memory-backed storage and mount limits.

Exported submissions are capped at 64 MiB and 20,000 entries. Docker and its host remain trusted.
Only the selected image's installed, offline dependencies are available. The Python-only CI image
does not establish that historical Rust checks build. Task code necessarily runs in the same checker as its tests;
containment protects the host, not a proof against all ways of gaming a particular check.

The existing Phase 1 `turnrun.py` remains an offline-only driver, with its original host-side
fixture verification/check path. It is **not** the contained live executor. The new adapter must
be used for any future agent-modified submission. Do not wrap a live CLI under another program
name to evade Phase 1's accidental-launch tripwire.

## Scenario and policy protocol

Build only for evaluation/test use:

```sh
cd cli
cargo build --locked --features turnrun --bin scenariorun
```

A private scenario has `agent`, `actions`, and an optional `seconds` (1–300, default 60).
There are at most 64 actions:

```json
{"agent":"codex","actions":[
  {"op":"turn","message":"Keep the public API stable."},
  {"op":"restart"},
  {"op":"turn","message":"Continue the work."},
  {"op":"clear"},
  {"op":"turn","message":"Start a new task."}
]}
```

`seed` supplies Session `threads` and `(agent, context_tokens, record)` tuples for a reconstructed
conversation; `compact` is supported for Claude. Seeds and expectations are evaluator inputs,
never candidate-editable fields. The output records the action sequence, continuation identities,
per-turn usage, restart state hash, scenario/policy/program/binary hashes. Test stand-ins inspect
actual provider requests to verify the exact new message and transmitted history.

```sh
cli/target/debug/scenariorun --project FIXTURE --scenario PRIVATE_SCENARIO.json \
  --program TRUSTED_FAKE_PROVIDER --state PRIVATE_STATE --home EMPTY_PRIVATE_HOME \
  --policy PRIVATE_POLICY.json
```

Both runners retain `RINGO_CHILD` refusal. Tests run only deterministic stand-ins. Signal
cancellation shuts down Session's separate provider process group. A force kill cannot run a
signal handler; future live execution additionally needs the container supervisor.

The explicit policy permits only `instruction`, `handoff_instruction` (each at most 4096 UTF-8
bytes) and `handoff_at` (integer 16,000–128,000). Unknown fields and Unicode control characters
other than newline/tab are refused. Policy never sets permissions, tools, mode, authentication,
networking, counters, graders, lifecycle or splits. It preserves exact user-message text and
control commands. No environment variable or normal shell startup automatically loads it.

`turnrun.py run --policy FILE` also passes the validated policy through the actual Session path
and pins it for that offline campaign. A policy changing between repetitions is refused.

## Campaign gates and accounting

`Campaign.create(PRIVATE_DIR, plan)` freezes a private version-1 plan and execution/accounting
source hashes. Plans declare dev/validation/holdout task IDs and fix-family clusters, covered
policy fields, baseline/degraded/manual policies, separate controls/search/manual/confirmation
budgets, stop margin, per-call caps, repetitions, at most two initial rounds, seed and thresholds.
Unknown fields fail validation. A task and its cluster belong to exactly one split. The controller
checks actual membership and stage before invoking the evaluator, not just a caller's split label.

The `Campaign` Python API takes trusted evaluator/proposer callables. They are **not** model-facing
extension points or a certified live executor. An evaluator returns protected outcome and usage
records; a proposer returns a hypothesis, complete candidate policy and usage record. The eventual
live adapter must pin binaries/images/checks/provider configuration as well as the sources, enforce
wall limits and supply the isolated broker described below.

Controls interleave unchanged and preregistered degraded arms on dev tasks with at least three
repetitions. Scoring weights fix-family clusters equally. Advancement requires baseline competence
and a positive lower cluster-bootstrap sensitivity bound over at least three clusters. These are
screening gates; a tiny public historical panel cannot establish generalization. Control tasks
are development evidence. A harness control must break a behavior exercised by the scenario,
not merely ask an agent to repair a historical source snapshot containing that behavior.

Each round proposes exactly two siblings from the same incumbent and frozen pre-round development
history. Each proposal changes one covered mechanism. Both proposals are charged even if screening
rejects them. The eligible candidates and incumbent receive contemporary paired development
evaluations. A candidate advances only with worthwhile quality gain within the cost allowance,
or meaningful savings at equivalent quality, while respecting the historical quality floor.
The accepted policy becomes the next round's parent. Two rounds or two no-advance rounds stop the
initial search. Full hypotheses, policy diffs, outcomes, costs and decisions remain in the ledger.

Validation and holdout never enter proposer feedback. Frozen finalists are compared once with the
original baseline and preregistered engineer-selected policy. Failed validation blocks holdout;
search cannot reopen after confirmation. Reports show search-versus-manual quality/token differences
and distinguish equal budget ceilings from actual consumption. Passing the baseline gate does not
by itself establish that automated search beats the engineer-selected policy.

Every call reserves its full cap **before** execution. Cache reads count. Unknown/unfinished usage
keeps at least that reservation; overshoot is recorded at its actual reported total. Unknown
accounting, overshoot, delivery failure or protected regression halts advancement. Quota failures
retain evidence and require a separately reviewed restart protocol; there is no automatic retry or
provider/model/billing switch. A crash after `finish` cannot bypass failure gates on resume. Finished
requests are reusable only with the same phase, cap and metadata. Interrupted attempts never retry
silently. The stop margin is an admission rule, not a hard token-consumption guarantee.

## Run the offline demonstration

From outside a Ringo child, or as an ordinary zero-inference controller check:

```sh
python3 turnrun_campaign.py offline-demo --campaign /tmp/ringo-synthetic-demo
python3 turnrun_campaign.py status /tmp/ringo-synthetic-demo
```

Every observation and token counter in this demonstration is **synthetic**. Provider calls: zero.
It exercises the whole control/search/confirmation state machine, not model quality.

For an actual private protocol, `prepare --plan FILE --campaign DIR` records it without inference.
`live-preflight` exits unsuccessfully and lists blockers; there is no enable-live flag.

## Remaining live prerequisites

`WorkerEndpoint` implements the controller side of the fixed worker transport using
[MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2025-03-26/basic/transports).
It binds to loopback, requires a random per-trial bearer capability, rejects Origin and unexpected
Host headers, and bounds request bodies, socket waits, tool calls and lease admission. Duplicate
request IDs cannot execute a command twice. Only `tools/list`, `tools/call`, initialization and
ping are supported; there is no resource, prompt, sampling, host-file or evaluator route. The
worker never receives the endpoint capability. Backend failure halts the endpoint. Its private
`mcp_config()` result is for the future isolated provider process and must stay outside the repo.

The transport is **not a native provider broker**. No provider process, authentication transfer,
builtin-tool removal, hook/config discovery suppression or provider filesystem/network profile
is certified by it. It exposes neither a live-launch command nor a bypass for `RINGO_CHILD`.

The next execution adapter must provide a pinned native-subscription **broker**, distinct from
the network-disabled worker and checker. It must keep every builtin filesystem/shell/web route,
connector, subagent, hook and project configuration discovery unavailable, while exposing only the
immutable worker tool contract. The broker receives no owner home, clone, inventory, other trial,
Docker socket or checker mount. The worker must not receive provider proxy or authentication access.

Claude is the first candidate adapter. `--tools ""` and strict MCP selection are relevant, but
removing safe mode alone is insufficient: safe mode currently drops MCP tools too. Do not use
`--bare`, which bypasses native OAuth. A dedicated profile needs startup/tool-discovery and
authentication verification before certification. Codex support is deferred until equivalent
containment is established. See the primary [Claude CLI reference](https://code.claude.com/docs/en/cli-reference)
and [Docker network isolation documentation](https://docs.docker.com/engine/network/drivers/none/).

After broker certification, in a standalone session: verify actual image-specific check discrimination,
freeze fresh splits and scenario coverage, check native subscription capacity, then authorize a
numeric budget and stop rules. Begin with a small baseline/degraded pilot (provisionally up to
18M admission tokens within the controls allocation). Recompute the full controls schedule when
the panel changes. The old 80M proposal covers controls only; search needs its own allocation.
Under a separately approved 160M envelope, the proposed split is 80M controls, 25M search, 25M
manual comparison and 30M final evaluation/overhead. None of these figures are spending authorization.

## Verification

Verify private historical graders in the image that would actually grade submissions:

```sh
python3 turnrun_certify.py --tasks PRIVATE_INVENTORY --source TRUSTED_REPO \
  --image sha256:PINNED_IMAGE --work NEW_PRIVATE_EVIDENCE_DIR [--runtime python|rust]
```

This reconstructs both pristine sides, executes their checks only inside separate network-disabled
containers, and records image, inventory/check/source hashes, fixture trees, runtime availability,
exit codes and hashed output logs. Existing evidence directories are refused. Every selected
check must fail at its parent and pass at its fix; timeout, truncation, missing tools, inconsistent
unavailability and infrastructure failure cannot certify the selected panel. Reports remain
`live_certified: false`: historical discrimination does not certify a broker or fresh sealed splits.
Dependency vendoring and image preparation happen before grading; no writable cache is shared
between trials. Any interpreter-sensitive check must be verified on the exact pinned interpreter,
not merely another release with the same minor version.

```sh
python3 -m unittest discover -s tests -q
RINGO_TEST_IMAGE=sha256:YOUR_PINNED_IMAGE python3 -m unittest \
  tests.test_turnrun_isolation tests.test_turnrun_certify tests.test_turnrun_broker -v
cd cli
cargo test --locked --all-features --no-fail-fast
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Docker tests execute real containment attempts, malicious grading, archive attacks, fresh-trial
checks, and controller termination. A dedicated Linux CI job resolves its test image to an immutable
local ID and runs this suite; the ordinary Python matrix skips Docker tests. Session tests
use fake providers and establish transition behavior, not live model sensitivity.

# Approved releases and latency checks

> Publication status (2026-10-24): this guide includes tested local prototype
> capabilities whose runtime changes are pending separate publication. This
> documentation upload does not activate a release or change the default runtime.

`ringo.py` is the stable entrypoint. Before importing application modules it
checks one local active pointer, verifies the selected bundle and approval, and
runs the selected runtime in the same Python process. No downloads, update
queries, scans or additional provider calls occur on launch. Management commands
always use the checkout, so an older active release cannot hide a rollback or
stage command.

The stable bootstrap imports `harness.py`/`workflow.py` directly from the pinned
bundle; it does not reload another CLI or interpreter. Management/provenance
helpers (`releases.py` and `latency.py`) stay with that bootstrap. Each released
run records their separate `bootstrap_sha256` alongside the bundle ID, so it does
not misrepresent those helper modules as version-pinned application code.

State defaults to `~/.local/share/ringo`, outside the checkout. Set
`RINGO_RELEASE_HOME` to use a different private local store, or pass `--state`
after `release` for management commands. A missing pointer uses checkout mode;
a corrupt pointer or bundle fails closed before inference. Merely staging or
previewing a release does not activate it.

## Stage and test

```sh
python3 ringo.py release --state /PRIVATE/STATE stage --source /PATH/TO/ringo
python3 ringo.py release --state /PRIVATE/STATE preview --release RELEASE_ID -- review \
  --input /PRIVATE/material.txt --backend codex-chatgpt --timeout 180
python3 ringo.py release --state /PRIVATE/STATE status
```

Staging copies an explicit runtime inventory and all built-in plans/prompts,
never credentials, evaluation stores or private case material. A canonical
manifest hashes every source and bytecode file; its SHA-256 is the release ID.
Bytecode is compiled at staging with checked source hashes and is itself covered
by the manifest. This avoids reparsing the whole runtime on every invocation
while keeping bundle contents verifiable. Re-stage on a different Python cache
tag/environment and collect new latency evidence rather than extrapolating old
performance. Owner-readonly permissions prevent ordinary accidental writes;
they are not a boundary against a malicious process running as the same user.

The review plan's prompt contents are loaded from the staged source, not an
unrelated checkout. Explicit `--pipeline` overrides still work. Per-run summaries
and trace `implementation.json` record release/checkout ID, plan hash, override
path and matching quality evidence. Checkout IDs include dirty runtime content.
Workflow reports also identify the runtime, task and writer prompt. Provenance
hashing adds local I/O; its cost must be measured, not declared free.

Preview is explicitly unapproved and records `mode: preview`; it makes no quality
claim. Direct calls to `harness.py` or `workflow.py` use that checkout and do not
follow the active pointer. Use the stable `ringo.py` entrypoint for released runs.
Long-running Python processes that already imported application modules cannot
switch releases in place; start a fresh CLI invocation.

## RRSI candidate evidence

```sh
python3 ringo.py release --state /PRIVATE/STATE stage --source /PATH/TO/ringo \
  --candidate-plan /PRIVATE/CAMPAIGN/candidates/N/plan.json \
  --evaluation-store /PRIVATE/STORE --digest EXISTING_TEST_DIGEST
```

Candidate staging requires an existing non-void sealed-test pass from the
protected evaluator and verifies the evaluated candidate's normalized plan and
prompt contents. Activation rechecks that evidence. This module does not run
grading, decide an experiment, change its gates or produce an adoption decision.
The normal deployment route has no RRSI improvement claim. A person still adopts
the change. Changing protected frozen inputs invalidates old experiments; do not
reuse their digests as evidence for a changed candidate.

Quality evidence is scoped to the evaluated review plan and backend/checker
configuration. Design/work runs and explicit pipeline overrides do not inherit
that claim; neither does a review using different backend settings. Backend
aliases are recorded strings, not guarantees that a provider model stayed fixed.
Pin resolved model/settings in the evaluation itself for stronger reproducibility.

## Offline latency assessment

`latency.py` accepts externally measured paired durations, both outcomes,
planned pair count, configuration identities and an explicit protocol. Every
scheduled pair, including failures, must be present. Durations must be finite,
positive and at most one day. Duplicate IDs, invalid protocols and altered
reports are rejected. Timings and approval identity are self-reported local
evidence, not an authenticated external measurement service.

```sh
python3 latency.py --measurements /PRIVATE/raw.json \
  --absolute-ms 5 --relative 0.05 --min-pairs 100 --resamples 2000 \
  --out /PRIVATE/review-latency.json
```

Those numerical budgets are examples and the local experiment runner's
**provisional diagnostic defaults**, not a universal definition of negligible
or a user-approved production SLO. Choose meaningful absolute and relative
budgets before collecting data. Raw samples, assessor source hash, policy hash,
measurement hash, seed and resample count are retained in each report.

The assessor reports p50/p95/p99, mean and p95 deltas in milliseconds and ratios,
and simultaneous paired-bootstrap bounds (Bonferroni across four comparisons,
95% family confidence). Each workload is assessed separately, not pooled into
an average that could conceal one slow path. Failure rates are checked separately;
an added fast failure cannot qualify as an improvement. Equal nonzero failures
and insufficient samples prevent a negligible verdict but cannot erase an
already demonstrated slowdown. p99 is descriptive, not a separately
validated tail SLO; estimating extreme tails reliably needs many more observations.

- **negligible:** every upper bound fits both declared budgets, with sufficient
  samples and no failures on that workload.
- **slower:** a lower bound exceeds a budget, or candidate failures increase.
- **uncertain:** neither conclusion is supported, or the collection is insufficient.

The bootstrap assumes the paired sampling protocol is representative; it does
not remove thermal drift, temporal dependence, workload selection or provider
changes. Randomize order, retain warmups separately, inspect jitter and repeat
under representative load. Python's [performance counter](https://docs.python.org/3/library/time.html#time.perf_counter_ns)
provides a monotonic clock. [pyperf's guidance](https://pyperf.readthedocs.io/en/stable/system.html)
explains why system conditions affect benchmark stability. The runtime remains
standard-library-only; pyperf is research guidance, not a dependency.

## Paired implementation experiment

First save the exact pre-change checkout, including dirty source content, outside
the repository. Then run:

```sh
python3 bench_latency.py --baseline /PRIVATE/BASELINE --out /PRIVATE/NEW_EXPERIMENT \
  --pairs 100 --warmups 5 --absolute-ms 5 --relative 0.05 --execution active-fixture
# For a later deployment, measure the currently active baseline bundle instead:
python3 bench_latency.py --baseline-release ACTIVE_RELEASE_ID --baseline-state /PRIVATE/STATE \
  --out /PRIVATE/NEXT_EXPERIMENT --pairs 100 --warmups 5 --execution active-fixture
```

The runner uses randomized paired fresh CLI processes for startup, review,
tracing, large input, design, one work round with six judges, a fixed backend
delay, an explicit pipeline override and backend failures. Its model and council
bridge are labeled local fixtures. **It makes zero provider calls.** It measures
local dispatch, parsing, hashing, I/O and process overhead; it cannot establish
live provider/model latency, real Council quality or an IDE's responsiveness.
New CLI processes test interpreter/import startup but are not proof of cold disk
caches. Preview includes management parsing; identify it as preview when comparing
results rather than presenting it as the approved-path measurement.

The runner defaults to unapproved preview. `--execution active-fixture` instead
creates a synthetic approval **only in the new experiment's private state** to
measure the actual active-pointer/approval-loading path. That setup uses constant
dummy observations clearly labeled `SYNTHETIC APPROVAL FIXTURE, NOT MEASURED` and
an approver labeled `OFFLINE TEST FIXTURE — NOT HUMAN ADOPTION`. Those dummy
observations are test setup, never measured results or adoption evidence. The
actual paired samples are collected afterwards; reports identify the execution
mode. The normal user's release state is never changed. A later active-release
baseline is verified against its explicitly supplied state pointer.

The runner writes raw execution commands, all outcomes/durations, per-workload
reports and `suite.json` outside the checkout. Diagnostic failure workloads remain
failures even when the expected CLI error occurs. Do not quietly exclude them
from a study or describe their speed as success. Never run performance measurements
concurrently with tests or inference and then call the timing conditions controlled.

## Activate and roll back

```sh
python3 ringo.py release --state /PRIVATE/STATE activate --release RELEASE_ID \
  --latency-report /PRIVATE/suite.json --approved-by HUMAN_NAME --expected-current none
python3 ringo.py release --state /PRIVATE/STATE rollback --expected-current CURRENT_RELEASE_ID
```

Activation needs a suite of recomputable reports covering `startup`, `review`,
`review-trace`, `review-large`, `design` and `work`. All reports must identify the
same baseline and exact candidate release, and bind the bundled default-review
configuration fingerprint. Workload-specific commands/settings remain in the raw
execution log. Once a release is active, the next baseline must be that release.
For an initial deployment there is no prior bundle; the human must verify the
declared checkout baseline. Do not benchmark an unrelated baseline to earn approval.

Demonstrated slowdowns block activation even if an override is supplied. An
uncertain suite requires `--accept-uncertain 'HUMAN RATIONALE'`; the agent must
leave that choice to the user. The full suite and rationale are bound into the
approval record. Activation recomputes assessments instead of trusting a supplied
verdict. Reassessing with different budgets is a new decision, not an unchanged pass.

An exclusive lock serializes management changes; a flushed, atomically replaced
state document records active and previous together. `--expected-current` detects
stale concurrent requests. Rollback verifies the previous bundle and approval;
there is no previous bundle after the first activation. New invocations select
the new pointer; an in-flight invocation retains its initial bundle and loaded
plan/prompt text. Bundles must remain available for those processes.

Approval is a local human attestation, not identity authentication or automatic
policy adoption. Tests and the experiment runner can set up isolated, explicitly
labeled fixtures, but never activate the normal user store. Unsupported
filesystem behavior or a clear performance
regression is a reason to keep this path disabled and change the implementation.

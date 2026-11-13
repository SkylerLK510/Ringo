# Turn runner: offline measurement for harness self-improvement

The follow-up [recursive campaign implementation](RRSI-CAMPAIGN.md) adds a contained worker/checker,
scripted Session scenarios and a gated offline search controller. It keeps live execution disabled;
the host-side check path described here is not a contained live evaluator.

This is Phase 1 of the plan to improve Ringo's shell harness the way RRSI does (Xia et al., 2026,
[arXiv:2609.24972](https://arxiv.org/abs/2609.24972)): **measure first**. It adds a runner that
puts one shell turn through the real `Session`, a driver that turns task commits into fixtures and
keeps a ledger, and a check protocol. It makes no model call, runs no control and no search, and
cannot support an improvement claim. The existing review-pipeline loop (`improve.py`,
`evaluate.py`) is unchanged, and so is the RRSI release claim, which still needs a review
pipeline's sealed test (`releases.py`).

| Piece | Where | What it does |
|---|---|---|
| Runner | `cli/src/bin/turnrun.rs` | One turn through `Session`, under a token cap and a wall cap. Writes one JSON record. |
| Driver | `turnrun.py` | Validates the inventory, builds fixtures, runs the runner, runs the check, keeps the ledger. |
| Inventory | private, outside the repository | `tasks.json` (briefs, commits, clusters) and one check per task. |
| Check protocol | this document | What a check must do and what its exit code means. |

The runner is behind a Cargo feature, so `cargo install` and Ringo's updater build only `ringo`:

    cd cli && cargo build --locked --features turnrun     # target/debug/turnrun

## What a run does

    python3 turnrun.py --tasks PRIVATE_DIR run T05 --campaign CAMPAIGN_DIR \
        --agent claude --program FAKE_AGENT --budget-tokens 1000000 --budget-seconds 900

1. Validates the inventory against the source repository's history (below) and refuses a program
   that is an installed agent CLI.
2. Takes the campaign lock, verifies the ledger's hash chain and every ledgered record's hash, and
   checks the campaign's pins.
3. Refuses a key that already ran, or one whose earlier attempt never finished (see Ledger). With a
   total budget, it refuses to start a run unless the tokens already committed plus this run's cap fit
   (`budget_stop`, exit 3).
4. Builds the fixture: the task's parent commit's tree as a fresh one-commit repository.
5. Ledgers a `start`, then runs the runner in its own process group with the brief as the message,
   in mode `auto` without `--write`, so the agent edits the fixture the check will read. The driver
   requests shutdown if the runner outlives its wall cap by two minutes, then force-stops it
   if it does not exit within ten more seconds.
6. Runs the task's check against the fixture if the turn finished, then ledgers a `run` with the
   class, usage, check exit code and evidence, and the record's sha256.

## Isolation: what is enforced and what is not

Enforced:
- **No live runs.** The runner needs `--program`. It refuses a program whose name is an agent's
  command or that resolves to the file PATH finds for one, and the driver does the same. This is a
  tripwire against an accident, not isolation. No flag turns live runs on.
- **A private state folder and home** for each run. The runner requires an empty home and sets
  `HOME`, `USERPROFILE` and provider/XDG configuration paths before loading agent configuration.
  The turn starts without the owner's configuration, history or keys in those locations. `RINGO_CHILD` is set for the agent, so it cannot start Ringo again. The runner refuses to
  start if `RINGO_CHILD` is set in its own environment.
- **Fixtures without history.** A fixture comes from `git archive` of one commit, extracted with
  tar's `data` filter and committed once, with a fixed identity, date and the message
  `task snapshot`. It has no remote, no alternates and no later objects, so the fix can't be
  reached through git, and the source commit id isn't in it. Its tree must equal the source
  commit's tree byte for byte, and the same task always rebuilds to the same commit.
- **Checks out of the fixture.** Checks live in the private inventory. They run after the turn, with
  the driver's interpreter and a clean environment.

Not enforced, and why live runs stay refused:
- **Network.** The agent's program is its transport to the provider, and nothing here separates that
  transport from the task's network access. **This repository is public, so every fix in the
  inventory is on GitHub.**
- **The file system.** An agent in `auto` mode can read anything the user can. That includes the
  owner's local clones, which hold every fix, and the inventory's checks if it finds them. A live
  phase needs a separate OS user or a container, or at least an audit of each transcript for reads
  outside the fixture. Until then, a live score could be contaminated.
- **Model knowledge.** A model trained after a fix was published may know it.

## Accounting

A run's usage is `input + cache_write + cache_read + output`, the same as `Tokens::total`
(`cli/src/parse.rs`). Codex's raw input includes its cached tokens. Ringo subtracts them, so
nothing is counted twice. The ledger also reports the total without cache reads, so a run can be
compared with earlier figures that left them out (for example Stage C2's in+cw+out). Both
figures are always labelled.

- **Unknown is not zero.** A run's usage is unknown when its record has no usage, when the job was
  never `counted`, or when any call reported no count (`uncounted_calls > 0`). The ledger keeps
  `usage_total: null` for that run and **reserves at least its whole cap**, or the reported
  lower bound when that is larger. An abandoned or still-open attempt
  is reserved the same way. Only a run the runner refused before starting anything counts as zero.
- **Everything counts.** Warm-ups, failed runs, cap breaches and retries all count toward the campaign
  total. `totals` reports `known`, `warmup` (of `known`), `known_without_cache_read`, `reserved` and
  `committed = known + reserved`.
- **Caps are polled.** The runner checks the job's running count (`tokens + live`) every 20 ms.
  How soon it sees a breach depends on the backend:

  | Backend | `cap_detection` | Behaviour |
  |---|---|---|
  | Claude | `live` | Counts arrive per message, so a breach stops the turn mid-way (`cap_breach`). |
  | Codex, agy | `post_turn` | Counts arrive at the end of the turn. A turn can finish past its cap; it stays `done` and its record carries `cap_overshoot`. |
  | plain | `none` | No counts. The usage is unknown and the cap is reserved. |

  A post-turn cap is a detector, not a hard stop. A count received just before completion can
  yield `cap_breach`; a completed turn carries `cap_overshoot` and is judged by its check.
  Admission reserves each run's cap, but overshoot or unknown usage can exceed that reservation.
  **Neither the per-run cap nor the total budget is a guaranteed hard consumption limit.** The
  ledger retains the evidence and refuses later runs that no longer fit the pinned budget.
- **A stop is targeted.** A cap or wall breach kills that turn's process group through `Session`.
  On Unix, the driver's backstop requests shutdown so the runner can stop the separate agent
  group, then force-stops a runner that remains stuck. On Windows it uses `taskkill /T`. Nothing
  kills by name. A wedged runner that cannot handle shutdown may leave a separate Unix agent
  group behind; this fallback is not containment and is another limit of this offline tier.

## Ledger

`CAMPAIGN/ledger.jsonl` is append-only. Each line carries `seq` and `prev`, the sha256 of the line
before it, so an internal change, removal or reordering is found. Truncating a valid tail cannot
be detected without a separate trusted checkpoint; this is an integrity check, not a security boundary. Every `run` line holds its record's
sha256, and the record is checked on every read. Entry kinds:

- `campaign`: the pins.
- `start`: key, task, source commit, setup commit and tree, and budget.
- `run`: class, outcome, usage, check exit and evidence, and the record's hash.
- `abandoned`: an interrupted attempt that was moved aside.
- `budget_stop`: a run the total budget refused.

- **Pins.** The first run pins:
  - the agent and mode
  - the runner binary's sha256 and the agent program's sha256
  - one hash over `tasks.json` and every check
  - the driver's commit and sha256
  - the Python version
  - the total token budget (including whether one was supplied)

  A later run whose pins differ is refused. The runner's own record adds Ringo's commit, the
  program's hash and the backend's cap detection.
- **One count per key.** Repetitions are positive integers. A key is `task:rep`, with `:warmup` added for a warm-up. A key with a
  `run` entry never runs again.
- **Interrupted attempts.** A key with a `start` and no `run` blocks until `--retry-interrupted`.
  The retry moves the old run folder to `*.abandoned`, ledgers `abandoned` (its cap stays reserved)
  and runs once more. A key that was retried once can't be retried again.
- **Same setup.** A task's fixture must rebuild to the same setup commit as its earlier runs.
- **One driver per campaign.** A lock file guards the campaign. A driver that crashed leaves the
  lock behind. Remove it only after checking that no driver is running.

## Outcome classes

| Class | When |
|---|---|
| `pass` | The turn finished and the check exited 0. |
| `treatment_failure` | The turn finished and the symptom is still there, or the tree no longer builds (check exit 1). |
| `quality_failure` | The symptom is gone but a regression test the check names fails (exit 2). |
| `check_unavailable` | The check can't tell on this platform or interpreter (exit 3). |
| `check_error` | The check crashed or ran out of time. |
| `unchecked` | The turn finished and `--no-check` was given. |
| `cap_breach`, `timeout` | The runner stopped the turn at its token cap or wall cap. |
| `quota_pause` | The turn failed with a reason that names the provider's quota. |
| `delivery_failure` | Any other failure, a cancellation, a refusal after launch, or no record. |

## The inventory

The inventory is private because a brief and above all a check are what a task agent must not
see. `AGENTS.md` keeps task contents and hidden tests out of this repository. Name the inventory
with `--tasks DIR` or `TURNRUN_TASKS`. The driver refuses an inventory, campaign, work folder or
report path inside the repository.

`tasks.json` has the form `{"tasks": [...]}`. Each task has these fields:
- `id`: `T` followed by digits
- `cluster`
- `date`
- `runtime`: `python`, `rust` or `rust-pty`
- `title`
- `brief`: a symptom only, never the fix, with no commit id of 7 or more characters
- `parent_commit`
- `fix_commits`: in order
- `check`: a path inside the inventory
- `notes`: optional

The driver checks the history:
- Every commit exists.
- The first fix's parent is `parent_commit`.
- Each fix is an ancestor of the next.
- `date` is the last fix's commit date.

`verify-inventory` builds each task at its parent and at its last fix and runs the check on both.
The check must exit 1 at the parent and 0 at the last fix. A check that reports it can't tell here
(exit 3) is listed as unavailable rather than broken.

**Split.** `split --dev-until DATE` gives each cluster the date of its latest task. Clusters up
to that date go to dev, later ones to validation, and a cluster is never divided. The split reads no
run, so it is fixed before any feedback exists. Failure write-ups for later phases may use only
dev-side sessions.

### Check protocol

A check runs as `python3 CHECK FIXTURE_DIR` after the turn, with the driver's interpreter.

| Exit | Meaning |
|---:|---|
| 0 | The symptom is gone and the check's regression tests pass. |
| 1 | The symptom is there, or the tree no longer builds or imports. |
| 2 | The symptom is gone, but a regression test the check names fails. |
| 3 | The check cannot tell on this platform or interpreter. It prints `discriminates: no: REASON`. |
| other | The check itself broke. |

Rules:
- **Behaviour, not the reference fix.** A check tests the symptom through code that exists at the
  parent commit, such as public functions, the CLI or the `Session` API. It must accept any
  correct fix. It never looks for names, files or messages that only the historical fix
  introduced.
- **Proven both ways without a model.** The check exits 1 at the parent and 0 at the last fix.
- **Standard library only, and no network.** Rust checks use `cargo --offline --locked`. A check
  writes only inside the fixture or a temporary folder.
- **Bounded.** Every subprocess has a timeout. A check that depends on timing uses generous bounds,
  or exits 3 rather than flake.
- **A clean environment.** Code in the fixture runs without `RINGO_CHILD`, `RINGO_HOME`,
  `CODEX_HOME` or provider API keys, and Python code runs with `sys.executable`.
- **A shared Rust build.** If `TURNRUN_CARGO_TARGET` is set, it becomes `CARGO_TARGET_DIR`, so
  checking many snapshots doesn't rebuild every dependency.

## Which harness features the tasks can test

A feature that a later phase might make editable needs a scenario that fails when that feature
breaks.

| Feature | Scenario it needs | Status |
|---|---|---|
| Turn instructions (how a shell turn's prompt is built) | One-turn bug fixes like this inventory's | The inventory exists. Live runs are not run. |
| Handoff text and thresholds (`handoff_at`) | A seeded conversation past the threshold, then a scripted next message, with a check on the answer | The runner supports `--resume-json` and `--handoff-at` (fake-agent tested). **No scenario is built yet.** One-turn tasks cannot test handoffs. |
| History after `/clear`, restart, `/compact` (the mechanisms `37d14f2` fixed) | Several turns with the history step between them | **Not built.** A one-turn task cannot detect a regression here. |
| Safe mode, permissions, tool access, provider auth | none | Never editable, so no scenario is needed. |

## Limits

- The runner and driver are tested only with fake agents. No real turn has been measured.
- Live runs are refused (see Isolation), so Phase 2 can't start until the owner either separates
  the transport from task network access, or explicitly approves a labelled unisolated mode with a
  transcript audit.
- The tasks come from one person's history in one repository, and the fixes are public. A real
  gain has to beat rerun noise and task-selection noise on about ten tasks. Phase 2 measures
  whether that's possible before anything is tuned.
- The fix commits are chained, so one task's fix snapshot can be another task's parent snapshot.
  Each fixture holds only its own snapshot, but tasks that sit close in history share a lot of
  code.
- The Rust runner tests are Unix-only because their fake agents are shell scripts, so the runner's
  Windows behaviour is untested. The driver tests run on Linux, macOS and Windows in CI.

## Proposed Phase 2: controls (**not run**, needs a declared budget)

**Nothing below has run.** Phase 2 needs the owner to approve a budget and to settle the live-run
isolation question above.

1. **Noise and cost.** Run the unchanged harness 3 times on every task that discriminates. Before
   any run, preregister the metric (pass rate per cluster), the smallest effect worth acting on, the
   grouping and the stop rules.
2. **Sensitivity.** Break something a one-turn task exercises, on purpose, choosing and
   preregistering it before any run. Run that harness on the same tasks 3 times. If it doesn't score
   worse than the unchanged harness, stop: the task set can't tell a regression from noise. The
   reviewers suggested reverting part of `37d14f2`, but its mechanisms (history after `/clear`,
   restart and `/compact`) need the multi-turn scenarios that aren't built yet.
3. **Both uncertainties.** Estimate rerun variation and task-selection uncertainty (by
   bootstrapping over clusters). An owner-specific result is not a release claim.

**Estimated cost.** These figures include cache reads and are taken from Stage C2's measured runs
of `claude-opus-5-5` on Ringo tasks (the memory experiment, 2026-11-11):
- Its two smaller tasks: 0.16–0.38M tokens per run, of which 22–34k was in+cw+out.
- Its `lib.rs` task: 1.2M tokens (a cap breach).

The table assumes 0.16–0.38M per Python run and 0.5–1.5M per Rust run.

The private inventory currently has 11 implemented checks: 7 Python and 4 Rust. All fail at the
parent and pass at the reference fix in local offline verification; one Python check requires
Python 3.11 because Python 3.14 hides the original symptom. Two additional candidates have no
check and are excluded. These counts are an inventory status, not a measurement of model quality.

For those 11 tasks (assuming every check is available on the selected interpreter):

| Item | Runs | Tokens (with cache reads) |
|---|---:|---:|
| Unchanged harness ×3 | 33 | 9–26M |
| Regression ×3 | 33 | 9–26M |
| Warm-ups (one per runtime) | 2 | about 2M |
| **Total** | **68** | **about 21–54M** (roughly 2–9M without cache reads, which were 7–13% of C2's totals) |

Suggested caps:
- Per run: 1M tokens for Python tasks and 2.5M for Rust tasks, both counting cache reads.
- Wall time: 15 minutes for Python tasks and 30 minutes for Rust tasks.
- Campaign total: 80M, with unknown usage reserved at the full cap.

Serially, at 3–10 minutes a run, that is about 3–12 hours. Subscription capacity has not been checked; check it before
approving.

## Commands

    python3 turnrun.py --tasks DIR inventory
    python3 turnrun.py --tasks DIR fixture T05 --side base --out /tmp/t05
    python3 turnrun.py --tasks DIR verify-inventory --work /tmp/verify --report /tmp/verify.json
    python3 turnrun.py --tasks DIR split --dev-until 2026-11-01
    python3 turnrun.py --tasks DIR run T05 --campaign DIR --agent claude --program FAKE \
        --budget-tokens N --budget-seconds N [--rep N] [--warmup] [--total-budget-tokens N]
    python3 turnrun.py totals CAMPAIGN_DIR

Tests: `python3 -m unittest tests.test_turnrun` (driver, fake runner) and
`cd cli && cargo test --locked --features turnrun --test gate_turnrun` (runner, fake agents).

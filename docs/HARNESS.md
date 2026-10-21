## Harness

Ringo's review and design work runs on a small harness. `harness.py` runs a pipeline of isolated, single-turn model calls; `pipelines.py` defines pipelines as data and assembles their replies in code; `evaluate.py` decides, under guardrails, whether a pipeline change helps; `improve.py` searches for changes. Standard library only.

```sh
python3 ringo.py review --input ~/ringo/change.txt --trace ~/ringo/runs/r1           # one strong call (plans/review.json)
python3 ringo.py review --input change.txt --pipeline my-plan.json   # another plan
python3 ringo.py design --input brief.md --out proposal.md
```

**Material.** One text file: the change description, the unified diff, the full text of every changed file after the change, and the README; for a design, the brief. Everything in it is sent to the backend's provider.

**Backends.** `claude` and `codex` are the built-in presets. `ollama:MODEL` runs a local model. `claude:<command>` runs any command that behaves like the Claude CLI (another model, other flags). Anything else runs as a plain command that reads stdin. The style comes from the name you give, never from guessing at the executable.

**Cost.** Every call puts the plan's role and labelled change material in the system prompt, with the pass's instruction and declared earlier replies in the message. The first pass runs alone; the first call on each per-pass model also finishes before its siblings start, to warm that model's cache. Later ready passes run in parallel, except under `--max-cost`, which runs one call at a time. Actual cache hits depend on the backend and provider; plain CLIs can add changing context ahead of stdin, so shared-prefix savings are not guaranteed. Evaluation gives each review slot its own opaque prefix to measure fresh-material cost, avoiding cache sharing between arms or prior experiments.

The trace charges malformed and failed attempts too. `--max-calls` and `--max-cost` stop new calls; calls already running finish and are charged. `--max-cost` requires `--unpriced-call-usd USD` for **every** backend, since even a cost-reporting backend can time out without reporting a price. This declared fallback is charged only when an attempt reports no cost; choose a conservative upper bound for any such attempt across all configured backends. An interrupted attempt whose completion is unknown remains unknown. The last call can exceed the remaining budget.

**File access.** The harness sends no history and starts each backend in an empty folder, but that folder is not an OS boundary. Codex retains shell tools; custom commands have unknown capabilities and inherit the caller's environment. Local files those tools read can reach the provider. Keep secrets and evaluator data inaccessible to the backend process. The Claude preset disables tools; local Ollama uses its supplied prompt. Evaluations and campaigns refuse other or unknown capabilities unless `allow_tool_backends: true` explicitly acknowledges the risk; that flag does not create isolation.

**Plans.** A plan is a JSON file of passes. Each pass has an id, a prompt file and a reply type: `findings`, `verdicts`, `issues`, `proposal` or `text`. A pass can also set `inputs` (earlier passes it reads), `batch`, `when` and `model`; `model` picks a cheaper model for one pass, for example.
- **Checker pass.** A review with several reader passes needs one checker pass. Its verdict lines (`KEEP`, `REFUTED`, `DUP`, `OPEN`, plus `CUT "sentence"`) apply one standard of evidence to every finding.
- **Assembly.** Code preserves finding text and whitespace, except for heading renumbering and explicit CUTs; a CUT must match a unique whole span separated by whitespace.
- **Review format.** Numbered findings precede the final Open questions and Not checked sections. A checked reply with findings after those sections is retried instead of dropping them. Nested subheadings stay in their findings; fenced code, including an unclosed inner fence through end of reply, cannot introduce findings.
- **Failures.** A missing verdict, or a duplicate whose direct target was not kept, sends the checker one more request. One repair covers both missing verdicts and orphan duplicates and can only judge the findings requested. If that still doesn't resolve it, the run fails.
- **Built-in plans.** They are `plans/review.json` and `plans/design.json`.

**Trace.** `--trace DIR` keeps the plan, the material, one JSON line per call (hashes, status, tokens, cost and seconds), every reply and the result. Use a new or empty folder; an earlier run's trace cannot be overwritten. Keep it outside the repository. The folder is created with mode 0700, because it holds the material.

### Evaluating a change

```sh
python3 evaluate.py import-reviewbench --store STORE --corpus REVIEWBENCH_CHECKOUT --clones CLONES
python3 evaluate.py trust  --store STORE --backend claude --family claude   # vouch for each grader CLI you checked
python3 evaluate.py split  --store STORE                     # whole repositories into dev / validation / test
python3 evaluate.py freeze --store STORE --spec SPEC.json    # preregisters the experiment; prints its digest
python3 evaluate.py run    --store STORE --digest D
python3 evaluate.py grade  --store STORE --digest D
python3 evaluate.py decide --store STORE --digest D          # exit 0 pass, 1 fail or inconclusive
python3 evaluate.py void   --store STORE --digest D --reason "inputs changed"  # close an invalidated experiment
```

The ReviewBench importer preserves Unicode and whitespace in changed filenames. Cases whose material cannot be decoded as UTF-8 text or contains NUL bytes are skipped whole, before case files are written. A gold location with a line break or backtick also skips its case, because the Markdown answer-key format cannot represent it. The import ledger record lists these cases and reasons under `skipped`. This keeps a partially omitted change out of the evaluation pool.

A loop that keeps whatever scores better on cases it has seen ends up fitting those cases. In this project's own tests, a review pipeline tuned on one hard change went from 4.5 to 7.7 defects found per review on that change, and showed no gain on six pull requests it had not seen. The guardrails are there from the start:

- **Splits.** Whole repositories are assigned to dev, validation or test.
  - Tune on dev.
  - Validation caps submissions across all campaigns sharing any current validation cases; renaming a campaign does not reset it. The improver receives only pass / fail / inconclusive, without reasons or numeric bounds.
  - The test split is sealed: one decision, then its cases are retired.
  - No case ever moves away from dev: a dev case stays in dev, and a validation case never enters test. Cases whose scores were released or retired go back to dev whenever the splits are rebuilt.
- **Ledger.** An append-only, hash-chained ledger, with a separate head file, records every step. A store-wide lock, held across processes, makes each check-then-write step atomic, and a test decision retires its cases in the same ledger record.
  - The policy fixes alpha, the validation cap, and promotion limits at the first split: quality U margin at least 0.5, cost noninferiority U margin at most 0.5, extra-wrong margin at most 0.25, quality cost ratio at most 2.0, and cost saving at least 0.2 by default. Validation/test specs cannot loosen these; dev specs remain exploratory. The Python split API can set initial policy bounds; resplitting cannot loosen them.
- **Preregistration.** The digest covers the spec, the plans and their prompts, the harness, pipeline, evaluator and backend code, the grading prompt, the split file (itself checked against the ledger), the canary map including its absence, every selected case file and its repository, and each backend's command, absolute file arguments with their content hashes, and the version of a known CLI. Each step re-checks it.
- **Blind grading.**
  - Reviews get fresh random ids, are shuffled, and carry no arm names.
  - The key is kept outside the run folder.
  - Existing reviews, blind keys and grades need their original ledger seals. Unsealed review/grade files are refused instead of being certified as model results; a score with missing seals is inconclusive. Do not seed a frozen run folder with hand-written results.
  - Grading needs a finished run, and every review must be in the key and in every grader's grades. Extra review ids or missing `found` / `invalid` fields are malformed. Optional `valid_extra` and `style` buckets are retained for audit. Each grade gets at most two attempts across invocations; failed grades stay failed. Slot starts, outcome hashes, grade outcomes and grading charges are sealed in the ledger; removing output files cannot buy a selective rerun. Changed artifacts make decisions inconclusive. Reviewer output containing an answer-key canary fails its slot. Reviews are labelled untrusted and graders are instructed to penalize grader-directed claims; this is a model instruction, not a proven injection barrier.
  - The test split needs graders from two provider families, and every grader must pass. A family is vouched for by a person: `evaluate.py trust` records the executable's real path and content hash in the ledger, and only an exact match counts, so an updated CLI needs trusting again and a renamed stand-in never qualifies. An untrusted command is custom, is never executed at freeze time, and counts toward no family.
- **Statistics.** Comparisons are paired per case and clustered by repository. A cluster-robust t bound and a cluster bootstrap bound must both clear every gate:
  - the lower bound of the gain in *U* (defects found minus wrong findings) is above the margin;
  - the lower bound of the recall gain is above zero (recall is the fraction of known defects found in each case, averaged across cases; cases with no known defects contribute zero);
  - the upper bound of extra wrong findings is below its margin;
  - the cost ratio is within its limit.
  - The *j*-th test decision spends *A* / (*j* (*j* + 1)) of the error budget *A*, so the total over all decisions stays below *A*.
  - The t quantile is computed numerically, including small tails from alpha spending. `min_groups` cannot be below 10. Validation/test freezing also requires `bootstrap * alpha >= min_bootstrap_tail` (default 1); this only rejects completely unresolved tails, and many more tail draws are desirable for stable estimates. The cluster methods are statistical approximations; alpha spending does not make them exact finite-sample guarantees.
- **Fail-closed.** Any of the following makes the decision *inconclusive*, never a pass: a failed slot, a missing grade, an unknown cost, or too few repositories.
- **Budgets.** A budget is per experiment and survives interruptions: spending is rebuilt from every attempt the trace recorded and a log of every grading charge, so a resumed run neither forgets nor double-counts. An attempt that started and never finished makes the cost unknown, and its slot is recorded as failed, never run again for free. Under a dollar cap, a pipeline makes one call at a time. Under a budget, a decision also needs every cost known, grading included, or it is inconclusive; without a budget, grader cost is reported, not gated, because it is the same for every arm and cannot bias the comparison. It is checked before every attempt, and once it is spent, the remaining slots and grades are recorded as missing, which makes the decision inconclusive. Every budget needs `unpriced_call_usd` for attempts with no reported cost, even on a cost-reporting backend. `grading_reserve_usd` can reserve part of the experiment budget for grading. `timeout_s` and `grade_timeout_s` configure review and grading deadlines (defaults 900 and 1800 seconds). Only one run and one grading of an experiment can happen at a time, so no slot is paid for twice.
- **Closing experiments.** `score` and `decide` require finished slots and grading, holding both execution locks before any exposure or retirement. An open test blocks resplitting. If frozen inputs change, `void --reason TEXT` closes it without trusting those inputs, conservatively retires the originally frozen test cases, and spends its alpha. Settled failures and interruptions still retire test cases; this prevents selective restarts.
- **Advisory, always.** Code cannot verify from the inside that the store is out of an improver's reach, because anything it checks, the same user can fake. So no decision adopts anything: a person adopts a change after a sealed-test pass, ideally from a store that the improver's user cannot read (another user, or a container).

What the guardrails cannot do:
- **They cannot create statistical power.** Detecting a gain of one defect per review takes tens of independent repositories, and a sealed test should be larger still. That is a planning figure, not a guarantee.
- **They cannot rule out contamination.** ReviewBench is public, so a model may have seen its pull requests; only fresh private cases check for that.
- **A store the improver can read is not a boundary.** Use an external OS boundary for decisions that count; reviewers and graders also need to be unable to read keys, since they run as the evaluator user. Stores are created with 0700 and existing store roots are tightened to 0700; permissions cannot constrain another process running as the same user.

### Improvement campaigns

`python3 improve.py --store STORE --campaign CAMPAIGN.json --workspace DIR` runs one bounded campaign.

**Where it comes from.** The loop follows [Meta-Harness](https://arxiv.org/abs/2603.28052): an improver receives the incumbent, dev evidence and prior proposal outcomes, then proposes the next pipeline; full artifacts remain on disk for audit. Its search discipline comes from [RRSI](https://arxiv.org/abs/2609.24972): one small edit per proposal, a log of rejected hypotheses, a screen for case-specific content, and a cost allowance that grows only with the measured gain.

**Research credit.** We credit **Peng Xia and coauthors at Google Cloud AI Research**, with collaborators at UNC-Chapel Hill, Stanford University and Washington University in St. Louis, for *RRSI: Regularized Recursive Self-Improvement of Agent Harnesses* (2026). Their work inspired this portion of Ringo's improvement loop. Ringo adapts selected ideas from the paper. See the [original paper](https://arxiv.org/abs/2609.24972) and the [authors' project page](https://regularized-rsi.com/).

**What the improver may change.** Only a review plan and the text of its prompts. It sees dev evidence only, without answer-key file paths. It cannot touch code, the grading prompt, specs, splits or thresholds.

**What happens to each proposal.**
1. A code screen checks it: one normalized change only, with prompt growth and estimated calls capped. A plan edit may introduce its new passes’ prompts, including library prompts. Repeated candidates and no-ops are rejected. Batch estimates use the maximum findings per reader observed in the baseline; they are estimates, and runtime call caps remain authoritative. Answer-key copying and canary checks are heuristics; a hidden-key match stops the campaign without giving it feedback.
2. It runs against the incumbent on a seeded dev sample. It advances only with complete slots and grades, unchanged sealed artifacts, known cost, enough repository groups, and a gain that clears the dev noise for every grader, at an acceptable cost.
3. Each advancing candidate goes to validation, which returns only its decision word to the proposer; the first pass ends the campaign. Dev evidence and the dev gate deliberately reuse the dev sample: the dev gate is a search filter, not confirmation.

**The finalist.** A finalist still needs a sealed-test pass before anyone adopts it.

**Campaign configuration.** Set `unpriced_call_usd` explicitly; there is no invented default price. The default codex grader also needs `allow_tool_backends: true` or an externally isolated alternative. Campaigns expose `timeout_s`, `grade_timeout_s` and `proposer_timeout_s`. After the baseline, measured review and grading spending estimates whether the next experiment is affordable before freezing it; a grading reserve and one observed call of headroom are kept. These are planning estimates, not guaranteed full-experiment prices. Dev and validation experiments stop scheduling calls after a failed slot; the baseline still collects evidence.

**Campaign lifetime.** A campaign name can start only once per store, with a new or empty workspace, and the workspace stays locked while it runs. An interrupted campaign cannot be resumed; its traces and charge logs preserve the spending, including unknown costs. A new campaign needs a new name and an explicitly chosen budget. The budget stops new calls after it is reached; the last call can exceed the amount remaining.

**Status.** The harness and its guardrails are built and tested offline. Which review pipeline is best is still open. The default is one strong call, because extra passes cost more and have not yet shown a gain on unseen cases.


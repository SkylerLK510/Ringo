---
name: ringo-harness
description: "Use Ringo for substantive coding and design workflows: review changes, propose designs, or revise declared artifacts using executable checks and Design Council assessment. Also supports explicit evaluation and bounded improvement campaigns. Simple questions and trivial edits do not require inference."
---

# Ringo harness

Ringo is a harness you run as a CLI. Each task is a pipeline of isolated, single-turn model calls (a plan
file plus prompts), assembled in code, with a trace of every call's tokens and cost. Run it from the repository
root. You never paste its prompts yourself: the prompts live in `prompts/` and the plans in `plans/`.

## Routine coding and design

When this skill is the user's selected workflow, use the executable harness for
substantive code/design work; merely reading this file is not a harness run.
For a code change, implement it, run the project's checks, then obtain one bounded
Ringo review before presenting it as finished. Verify each finding against the
code and fix confirmed defects. For a substantial architecture decision, use
`design`; for generating/revising a declared set of text files, read
`docs/COUNCIL-WORKFLOW.md` and use `work` with the separate council bridge. Pick
the rubric that fits the artifact (the council's `rubrics/report.json` for
documents and reports, `artifact_design.json` for visual artifacts) and keep the
default of 6 samples; 2 samples is a smoke test whose verdicts do not count.

Locate the checkout through `RINGO_HOME` when provided, or resolve this skill's
symlink: the checkout is two parents above `skills/ringo-harness/`. Invoke its
`ringo.py` by absolute path when working in another repository. Keep material,
reports and traces in a private run folder outside both project checkouts.

Use the stable checkout's `ringo.py` entrypoint so an explicitly activated local
release is selected before runtime imports. Each CLI run records its release or
dirty-checkout identity and effective plan; in-flight runs stay pinned. Management
commands are `ringo.py release ...`; see `docs/RELEASES.md`. Staging and preview do
not activate a release. Do not accept uncertain latency on the user's behalf:
activation requires their explicit decision/rationale; clear slowdowns block it.
RRSI candidate activation additionally requires an existing sealed-test pass and
human adoption. These release checks do not replace the protected evaluator.

For native ChatGPT login, use `--backend codex-chatgpt --timeout 180` for the
initial review/design run. This preset forces ChatGPT authentication,
removes API-key/endpoint overrides, disables the shell and records reported token
usage. Like every Codex backend it runs with a private CODEX_HOME and HOME, so the
owner's global Codex instructions and skills do not load in the child. It consumes subscription allowance; dollar cost
remains unknown. The default plans make one call; Ringo tries a failed or
wrong-shaped call once more, so at most two. There is no call cap to set. Use a
different backend only within the user's authentication and spending preferences. If login, budget or permissions prevent a run, report that explicitly;
do not describe offline fixtures as a real model assessment.

Keep the run bounded. Do not start improvement campaigns or additional inference
rounds merely because this skill is selected. Child writer/judge/reviewer calls
must not invoke Ringo again. The private CODEX_HOME/HOME of Codex children keeps
them from loading this skill, and a turn that uses an undeclared tool is rejected.
A rejected or timed-out turn names its reason; its raw events are kept in `TRACE/steps/*.raw.txt`.
Report the actual command, report path, call count and any known usage at completion.

## 1. Collect the material

The material is the text the harness supplies. Codex and custom commands may also read local files; their contents can reach the provider. An empty working folder is not filesystem isolation.
- **Review (preferred): let Ringo build it with `--repo PATH --base SHA --head SHA`**, plus `--pr-text FILE` with
  the change description. It reads git objects at those commits (commit the change first; uncommitted and
  untracked files are not included) and adds, after the diff, changed files and README, the unchanged
  definitions, subclasses, callers and tests of the changed symbols as `=== path (unchanged, at head) ===`
  blocks, and a `=== context manifest ===` of what it added and what the budget (`--context-budget`, default
  400000 bytes) left out. It never fetches: a partial clone missing objects is refused, so fetch them first.
- **Review with `--input`:** only when there is no repository. Give the change description, the unified diff
  (`=== change.diff ===`), the full text of every changed file after the change (`=== path (after the change)
  ===`), and the README. Give the whole changed files, not only the hunks: half the real defects sit outside the
  diff.
- **Design:** the decision brief with its hard constraints and every number it has (today's load, the load that is
  coming, limits, deadlines).
- Leave out secrets. Everything in the material is sent to the chosen backend's provider.

## 2. Run it

```bash
python3 ringo.py review --repo REPO --base BASE_SHA --head HEAD_SHA --pr-text PR.txt --backend codex-chatgpt --timeout 180 --trace NEW_DIR --out REVIEW.md
python3 ringo.py review --input MATERIAL.txt --backend codex-chatgpt --timeout 180 --trace NEW_DIR --out REVIEW.md
python3 ringo.py design --input BRIEF.txt --backend codex-chatgpt --timeout 180 --trace NEW_DIR --out PROPOSAL.md
```

- `--backend`: `claude` (CLI default: caches the material across passes and reports cost), `codex`, `codex-chatgpt`, `ollama:MODEL`,
  `claude:<command>` for another Claude-style command, `codex:<command>` for your own Codex command line (must pass
  `--json`; web search only if it passes `--search`), or any command that reads stdin.
- `--findings-json FILE` (review) also writes the findings as ReviewBench contract JSON, files checked at
  head with `--repo`; see `adapters/reviewbench/` for the benchmark image.
- `--checker-backend SPEC` (same forms as `--backend`) runs the plan's verdicts pass and its repair on another
  backend, so a different model or provider checks the findings. The plan must have a verdicts pass; the default
  plans do not, so the flag is refused with them. Plan files cannot name backends.
- `--pipeline PLAN.json` runs another plan; the default is one strong call. Multi-pass plans exist for experiments;
  use one only if an evaluation showed it pays (step 4).
- Calls are set by the plan, not by a cap: one per pass (one per batch of findings for a batched checker, whose readers may
  list at most 50 findings each), a failed or wrong-shaped call tried once more, and at most one checker repair. The default plans make one call,
  at most two. `work` makes one writer call plus one judge call per sample each round, at most
  `max_rounds x (1 + samples)`. `--max-calls` was removed.
- `--max-cost` stops new calls once spent. Under `--max-cost`, calls run one at a time and `--unpriced-call-usd USD` is required for every backend, including attempts that time out without reporting cost. Choose a conservative fallback; the final call may exceed the remainder.
- `--trace` needs a new or empty folder outside the repository. Use a plan whose kind matches the task.
- For review/design, the last stderr line is `harness: ` followed by a JSON summary; parse after that prefix. Report calls, cost, tokens and the `backend` and `checker_backend` it names. If `unpriced_calls > 0`, cost is unknown, not zero; if `untokened_calls > 0`, the token sums leave those calls out.
- Review/design: exit 0 is finished; exit 2 is failure. A late output-write failure prints the completed result and cost as recovery output.

## 3. Report the result

Give the user the review or proposal as written; it is assembled verbatim from the model's findings. Say which
backend and plan ran and what it cost. If `--trace` was used, point to the trace folder (it holds the material,
so it is created private). Open questions in a review are not findings. Before reporting, settle each one
against the repository yourself (you can read what the review call could not): promote a confirmed one to a
verified finding and say it was promoted from an open question, drop a refuted one and say why, and report the
rest as still open, separate from the findings.

## 4. Evaluating a pipeline change

Only `evaluate.py` can say a change is better, and only through its guardrails:

```bash
python3 evaluate.py split  --store STORE                 # repository groups -> dev / validation / test
python3 evaluate.py freeze --store STORE --spec SPEC.json  # preregisters; prints the digest
python3 evaluate.py run    --store STORE --digest D
python3 evaluate.py grade  --store STORE --digest D
python3 evaluate.py decide --store STORE --digest D      # exit 0 pass, 1 fail or inconclusive
```

- Tune on **dev** only. **Validation** has a capped number of submissions across campaigns sharing validation cases. The improver receives only
  pass / fail / inconclusive. The **test** split is sealed: one decision, then its cases are retired.
- Every decision is **advisory**: a person adopts a change, and only after a sealed-test pass. Every budgeted
  experiment needs `unpriced_call_usd` in its spec. Codex and unknown custom capabilities require explicit
  `allow_tool_backends: true`; this acknowledges file-access risk and does not isolate the backend.
- Evaluation uses review plans, requires `min_groups >= 10`, and measures recall per case as the fraction of
  known defects found. Every grade must cover exactly its reviews and include `found` and `invalid`; optional diagnostic buckets may be omitted.
  A failed grade stays failed; rerunning `grade` cannot grant extra attempts.
- Never prepopulate review, blind-key or grade files: model results need their original ledger seals. Changed
  canary maps invalidate preregistration. The ReviewBench importer skips entire non-text cases and unsupported
  gold locations (line breaks or backticks), recording them in the import ledger's `skipped` list.
- Never describe a dev score, a single case, or any decision short of a sealed-test pass as proof that a change works.

## 5. Improvement campaigns

`python3 improve.py --store STORE --campaign CAMPAIGN.json --workspace DIR` proposes one change at a time, screens
it, tries it on dev, and submits each candidate that clears dev to validation; the first validation pass ends the campaign. Its finalist still needs a sealed-test pass before
anyone adopts it. Campaigns cost real money: state the budget (`budget_usd`) to the user before starting one.
Use a new campaign name and a new or empty workspace. A name can start only once per store; interrupted campaigns
cannot be resumed. The traces and charge logs retain spending, and an interrupted paid attempt has unknown cost.
The budget stops new calls after it is reached; the last call can exceed the remaining amount.

An unfinished experiment cannot be scored or decided. If inputs drift, use `evaluate.py void --store STORE --digest D --reason TEXT`; this conservatively retires its original test cases and spends alpha. Store roots are 0700. Campaign price declarations are required before spending; the next experiment is estimated from measured spending and reserves grading cost.

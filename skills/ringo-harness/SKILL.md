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
`docs/COUNCIL-WORKFLOW.md` and use `work` with the separate council bridge.

Locate the checkout through `RINGO_HOME` when provided, or resolve this skill's
symlink: the checkout is two parents above `skills/ringo-harness/`. Invoke its
`ringo.py` by absolute path when working in another repository. Keep material,
reports and traces in a private run folder outside both project checkouts.

For native ChatGPT login, use `--backend codex-chatgpt --max-calls 1 --timeout 180`
for the initial review/design run. This preset forces ChatGPT authentication,
removes API-key/endpoint overrides, disables nested tools/skill discovery and
records reported token usage. It consumes subscription allowance; dollar cost
remains unknown. A one-call cap prevents automatic retry from starting a second
call. Use a different backend only within the user's authentication and spending
preferences. If login, budget or permissions prevent a run, report that explicitly;
do not describe offline fixtures as a real model assessment.

Keep the run bounded. Do not start improvement campaigns or additional inference
rounds merely because this skill is selected. Child writer/judge/reviewer calls
must not invoke Ringo again. The harness's isolated prompts and disabled discovery
on `codex-chatgpt` keep the routine review from recursively loading this skill.
Report the actual command, report path, call count and any known usage at completion.

## 1. Collect the material

The material is the text the harness supplies. Codex and custom commands may also read local files; their contents can reach the provider. An empty working folder is not filesystem isolation.
- **Review:** the change description, the unified diff (`=== change.diff ===`), the full text of every changed
  file after the change (`=== path (after the change) ===`), and the README. Give the whole changed files, not
  only the hunks: half the real defects sit outside the diff.
- **Design:** the decision brief with its hard constraints and every number it has (today's load, the load that is
  coming, limits, deadlines).
- Leave out secrets. Everything in the material is sent to the chosen backend's provider.

## 2. Run it

```bash
python3 ringo.py review --input MATERIAL.txt --backend codex-chatgpt --max-calls 1 --trace NEW_DIR --out REVIEW.md
python3 ringo.py design --input BRIEF.txt --backend codex-chatgpt --max-calls 1 --trace NEW_DIR --out PROPOSAL.md
```

- `--backend`: `claude` (CLI default: caches the material across passes and reports cost), `codex`, `codex-chatgpt`, `ollama:MODEL`,
  `claude:<command>` for another Claude-style command, or any command that reads stdin.
- `--pipeline PLAN.json` runs another plan; the default is one strong call. Multi-pass plans exist for experiments;
  use one only if an evaluation showed it pays (step 4).
- `--max-calls` and `--max-cost` cap new calls. Under `--max-cost`, calls run one at a time and `--unpriced-call-usd USD` is required for every backend, including attempts that time out without reporting cost. Choose a conservative fallback; the final call may exceed the remainder.
- `--trace` needs a new or empty folder outside the repository. Use a plan whose kind matches the task.
- For review/design, the last stderr line is `harness: ` followed by a JSON summary; parse after that prefix. Report calls, cost and tokens. If `unpriced_calls > 0`, cost is unknown, not zero.
- Review/design: exit 0 is finished; exit 2 is failure. A late output-write failure prints the completed result and cost as recovery output.

## 3. Report the result

Give the user the review or proposal as written; it is assembled verbatim from the model's findings. Say which
backend and plan ran and what it cost. If `--trace` was used, point to the trace folder (it holds the material,
so it is created private). Open questions in a review are not findings; keep them separate.

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

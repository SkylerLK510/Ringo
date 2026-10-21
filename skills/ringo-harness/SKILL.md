---
name: ringo-harness
description: Run Ringo's harness from an agent - review a code change, write a design proposal, evaluate a pipeline change with overfitting guardrails, or run a bounded improvement campaign. Use when the user asks Ringo to review a diff or PR, propose a design, test whether a pipeline change really helps.
---

# Ringo harness

Ringo is a harness you run as a CLI. Each task is a pipeline of isolated, single-turn model calls (a plan
file plus prompts), assembled in code, with a trace of every call's tokens and cost. Run it from the repository
root. You never paste its prompts yourself: the prompts live in `prompts/` and the plans in `plans/`.

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
python3 ringo.py review --input MATERIAL.txt [--backend claude] [--trace DIR] [--out REVIEW.md]
python3 ringo.py design --input BRIEF.txt    [--backend claude] [--trace DIR] [--out PROPOSAL.md]
```

- `--backend`: `claude` (default: caches the material across passes and reports cost), `codex`, `ollama:MODEL`,
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

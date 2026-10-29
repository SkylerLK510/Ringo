# Preregistration: V3 refinements on MBPP+ (paired, same model)

Registered 2026-10-29, before any model call of this study. The commit that adds this file fixes the code under
test, the task sample, the analysis and the decision rules. Any change after it is a deviation, recorded at the end
of this file with its reason.

## Question

Do four token-neutral refinements raise V3's hidden-test pass rate on unseen tasks without adding token volume?

The refinements come from reading HumanEval+ failures (HumanEval/34 and /47; `docs/AGCL.md`), so HumanEval+ is now a
development set for them and cannot test them. MBPP+ has not been used to build or tune any part of Ringo.

| # | Change | Commit | Where it acts |
|---|---|---|---|
| 1 | Writer rule: implement the named behaviour; examples illustrate it; never special-case an input to pass a check or match an example | `2b70773` | every writer call |
| 2 | Static note: Python lines that return a literal when a value equals a literal list, tuple or set of two or more items, shown to the writer with a failing check and to the review | `966dcbc`, `a0e1b0b` | only where it fires |
| 3 | Dispute stop: unchanged code that fails a declared check ends the run as `disputed` | `e26a23a` | only where it fires |
| 4 | Review wording: edge values named in the input rule; a one-sentence summary | `cd0df45` | every review call |

Prompt length does not rise: the writer prompt is 280 -> 279 tokens and the review prompt 347 -> 347 (o200k).
Development evidence, no model calls: the dispute rule's stand occurs in 3 of 335 earlier work runs and none of them
changed its code later; the static note fires on 1 of 645 distinct earlier drafts (HumanEval/47's hardcoded example).

## Design

- **Arms.** A = V3 (`e62fcf1`). B = V3 plus the four changes (the commit that adds this file). Each arm runs its own
  checkout's `workflow.py` and prompts on the same task specs.
- **Model and settings.** Both arms: `examples/v3-codex.json` (writer and judge `codex-chatgpt`, GPT-6.1 Sol through
  Codex CLI 0.164.0 with the ChatGPT login, low effort on every role, text protocol, protocol repair, no-regression
  gate, at most 3 rounds). Python 3.14.7 on macOS.
- **Tasks.** MBPP+ v0.2.0 (`MbppPlus.jsonl.gz`, sha256 `af43697e8791c4c1...`), 378 tasks, none excluded: every
  reference passes its original asserts and its own hidden grading, and a function returning None fails every task.
  A random sample of 150 (`--sample 150 --seed 20261029`), run in the sampled order; the order list has sha256
  `fd93821ab9b635ea...` (first ten: Mbpp/605, 475, 250, 749, 410, 558, 284, 167, 800, 777).
- **Sealed set.** The other 228 MBPP+ tasks are not run, read or used, so a confirmatory study can use them.
- **Public check.** The task's original MBPP asserts. **Hidden tests.** EvalPlus's base and plus inputs, graded
  after all runs finish with the study's port of EvalPlus's MBPP comparison (input deserialization, special oracles,
  float tolerance), 120 s per file, no per-input time limit.
- **Running.** `evalplus_study.py run --dataset mbpp --sample 150 --seed 20261029 --arm a=V3 --arm b=V3.1 --jobs 4
  --max-credits 70`: the two arms of a task run side by side in the same order, two tasks at a time.
- **Budget and stopping.** A launch ceiling of 70 credits at Sol rates (50 / 2.5 / 250 per million uncached input,
  cached input and output tokens). Spend is summed over every paid call in every run's trace: failed calls, runs that
  wrote no report, and set-aside runs all count. A task's first arm launches only while the spend so far, plus a
  reserve for the runs in flight and for this pair, stays under 70. The reserve is the cost of the costliest
  finished run, or 1 credit before any run finishes. The final spend is reported. It can exceed 70 only by what the
  runs in flight cost above that reserve. The ceiling is the only outcome-free stopping rule, and it never splits a
  pair, so the analysis set is a prefix of the order. Nobody looks at hidden grades before every run has finished.
- **Infrastructure failures.** Runs that end in an error, an interruption or a spent budget (a usage limit or a lost
  login) are set aside and rerun. Their spend counts toward the ceiling, but they are not task outcomes. Three such
  runs in a row pause all launches, possibly between the two arms of a pair. Once the cause is fixed, the same command
  is run again. It finishes any half-run pair first, since that pair is the earliest unfinished work in the order.

## Analysis (`evalplus_study.py grade`, then `compare --arm a --arm b`)

The analysis set is every task graded in both arms. A draft that trips the file-access tripwire counts as a failure
of its arm, both at round 1 and delivered. As a sensitivity analysis, the delivered result is also reported with
those pairs left out.

- **Primary: delivered hidden pass.** Counts per arm; gains (B passes, A fails) and harms; the two-sided exact
  McNemar p; a paired bootstrap 95% interval of the difference.
- **Secondary:**
  - round-1 hidden pass, the same way;
  - total tokens, credits and calls per task, with paired bootstrap 95% intervals of B - A;
  - run statuses;
  - the round-1 review's catches (findings on drafts that fail the hidden tests) and alarms (findings on drafts
    that pass them);
  - where the dispute rule stops in B, and where it would have stopped in A;
  - review-stage harms per arm: round 1 passes the hidden tests, the delivered files fail them;
  - where the static note was shown in B (recorded per round as `static_shown` to the writer and
    `static_reviewed` to the review), and where it would have been shown on A's drafts (a flagged candidate that went
    to the review, or that failed a check before another writer round);
  - agreement of the study's grader with the official EvalPlus evaluator (0.3.1, isolated venv, no model calls) on
    every delivered file. Every disagreement is listed, and the primary result is also given under the official
    grader. The study's grader stays primary. If the official evaluator cannot run, that is reported.

Tokens are Ringo's accounting: uncached input, cached input (`cache_read`) and output, counted once each.

## Decision rules

**The bundle.** B passes only if delivered gains are at least delivered harms, and B's mean tokens per task are at or
below A's (point estimates). If the bundle fails, nothing is adopted and all four changes are clipped. Superiority is
claimed only if the exact McNemar p is below 0.05.

**Each change, when the bundle passes.** If every change passes its own rule, B becomes the default (V3.1). A change
that fails its rule is clipped. The remaining subset is then an untested configuration, so it is not adopted from
this study: it becomes the candidate for the confirmatory study. The rules:
1. **Writer rule:** round-1 harms exceed round-1 gains.
2. **Static note:** it was shown in a B run that is a delivered harm, or it fires on more than 5% of B's tasks.
3. **Dispute stop:** an A run with a stand later delivered a hidden pass (the stop would have cost that pass), or a
   B run it stopped is a delivered harm.
4. **Review wording:** B's round-1 review alarm rate is at least 5 points above A's, or B has more review-stage harms
   (round 1 passes the hidden tests, the delivered files fail them) than A. If either arm has fewer than 20 reviewed
   round-1 drafts that pass the hidden tests, the alarm rate is not measured and this change fails its rule.

Clipped changes are recorded in the README's experiments log and removed from the code.

## Limits stated in advance

- **Power.** On HumanEval+ the arms disagreed on about 2% of tasks. On MBPP+, at perhaps 5-10%, 150 pairs give
  roughly 8-15 discordant pairs. Only a large net effect can reach p < 0.05, so this is a screening study. A
  confirmatory claim needs the sealed 228 tasks, or another unseen set.
- **Attribution.** Changes 1 and 4 act on every run and cannot be separated here. Rounds give stage-level attribution
  only, and a factorial study is the follow-up if the bundle passes. Changes 2 and 3 are attributed per task, because
  they act only where they fire.
- **Grader.** The grader is the study's port of EvalPlus, not the official evaluator, and it has no per-input time
  limit. Hence the cross-check above.
- **Scope.** The study uses one model at one effort, on function-level Python tasks. It says nothing about
  repository-level work or reviewer-judged quality.

## Reviews before registration

- Ringo review of the code (`e62fcf1..e6779ac`; 1 call): 3 findings, all fixed in `28a5d3e`.
- Ringo methods review of a draft of this plan (1 call): 6 findings. All six were taken up, in `b45f58e` and in
  this text: spend from traces with a costliest-run reserve, the resume rule after a failure stop, suspect drafts
  counted as failures, notes recorded where shown, an untested clipped subset not adopted, and the minimum count
  for the alarm rate.

## Deviations

- The official EvalPlus evaluator ran with `EVALPLUS_MAX_MEMORY_BYTES=-1` (its own switch for no memory limit),
  because macOS refuses its address-space limit and every task otherwise read as a timeout. Both graders then
  agreed on all 150 reference solutions and all 150 null functions of the sample before the arms were graded.
- Not a deviation: two arm-A runs (Mbpp/555, Mbpp/77) ended with the provider error "Selected model is at capacity"
  at no token cost, and were rerun as the plan says.

## Outcome (added 2026-10-29, after grading)

The bundle fails both conditions: 6 delivered harms and no gains (V3 132/150, B 126/150; exact McNemar p = 0.031,
in favour of V3), and +35 tokens per task (95% interval -43 to +118). Nothing is adopted and all four changes are
removed. The official evaluator agrees with the study's grader on all 300 graded files. Spend: 51.88 credits. The
per-change readings and the mechanism are in the README's experiments log, section 8.

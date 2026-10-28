# Audit-Gated Continual Learning (AGCL)

AGCL is how Ringo learns when it can stop spending tokens without changing its behavior on its own. The thing it learns is a stopping policy, starting with one rule:
- stop a `work` run after round 1 when every check passes.

Learning ships only as a release that carries a measured harm bound and a human approval. Every part adds zero model-call tokens.

## Why a method, not a model

Our private benchmark (93 graded runs; aggregates only, see the README's Experiments section) showed:
- **Most spending comes after round 1:** 71% of all tokens (31% of credits) went into rounds after round 1, and those rounds changed the hidden-test score in 8 runs: 5 better, 3 worse.
- **On code, a rule captures the saving:** "stop when round-1 checks pass" would have cut 73% of tokens with 0 of 58 runs made worse (95% upper bound 5.0%), and it would have avoided 2 regressions. A learned model has nothing left to win there.
- **Memos are the one real learning problem,** with 5 helpful later rounds in 35 runs. That is too few to train anything on: a small neural network would memorize them.
- **A free label exists:** in 46 of 93 runs, the delivered files were byte-identical to round 1's. That label is free in real use, with no hidden tests.

The limit is labels, not model capacity. AGCL is built to collect labels without adding tokens and to promote only what the evidence supports.

**Literature basis:**
- Wald's sequential test (1945), Thompson sampling (1933) and Horvitz–Thompson weighting (1952).
- Apple tasting (Helmbold, Littlestone & Long): skipping a step destroys its own label.
- Error seeding (Mills, 1972), with Dawid–Skene (1979) and Hui–Walter (1980) for error rates without ground truth.
- Complementary learning systems (McClelland et al., 1995).
- Seldonian algorithms (Thomas et al., 2019).
- On conformal certificates under shift: Kotte, *When Can Conformal Risk Control Certify LLM Outputs?* (arXiv 2606.29054, 2026).

## The six parts

1. **Shadow labels (zero added tokens).** Every ordinary run already saves the round-1 draft and finishes the full baseline. `shadow_labels.py` turns finished run folders into labeled rows:
   - eligibility;
   - byte identity;
   - tokens before and after round 1;
   - harm, when a `grade.json` from an evaluation sits beside the report.
2. **Retention cohort (zero added tokens).** With `stop_on_checks`, a run that is eligible still runs the full suffix when a random draw falls below `retain` (from 0.05 to 1). The draw is logged in `report.json`, so audits can weight it.
   - These are calls the baseline already makes, so exploration costs nothing relative to today.
   - The runs that ran the full suffix give an unbiased **change rate**. Stopping can harm only when the files would have changed, so the change rate bounds real-use harm.
   - Harm itself needs a graded outcome, such as hidden tests in an evaluation.
3. **Seeded audits (evaluation only).** In a sealed evaluation, plant a defect that the public checks miss into a correct draft, then run `work`'s own review on it.
   - This measures whether the review catches what checks miss, which decides whether the review is worth its tokens on check-passing work.
   - It is a diagnostic, not a certificate. It assumes the review and the checks err independently and that planted defects are as visible as natural ones, so report the result per defect kind.
4. **Declared drift.** Evidence is keyed by regime: the exact implementation, writer prompt, artifact kind, backends, roles and loop settings, never the task.
   - A model, prompt or release change starts a new regime with no evidence.
   - Old evidence carries over only through an approved transfer study.
5. **Consolidation as a release.** A candidate policy is refitted from the whole log, not updated online, so nothing is forgotten. It ships through `ringo.py release` only with a dossier:
   - an exact upper bound on harm among the **eligible** sealed tasks, the ones it would skip;
   - the retention change rate;
   - negative flips, meaning runs the release got right and the candidate gets wrong;
   - the share of logged runs where the old and new decisions differ;
   - positive token and billed-credit savings;
   - latency;
   - a human approval.

   If the evidence fails, the answer is "no solution found" and the current release stays.
6. **Model ladder.** Rule, then counts, then a linear model, then a small net. Each rung ships only if it beats the rung below on sealed data. The first net, if one is earned, belongs in the IDE's event monitor, where data arrives by the thousand.

## What is implemented

- **`stop_on_checks`** (`work` spec field, opt-in, off by default):
  - Form: `"stop_on_checks": {"retain": 0.1}`.
  - A round-1 candidate that passes every check, with no failing spec tests, is ready for review without the review call unless the draw keeps it.
  - `retain: 1` is shadow mode: it records eligibility and changes nothing.
  - It only removes calls, and it does not combine with the Council.
- **`shadow_labels.py RUNS... [--jsonl OUT]`:** rows and per-regime bounds from finished runs. It makes no model calls.
- **`evalplus_study.py`:** the sealed paired-prefix study and the seeded-review study on HumanEval+ (EvalPlus, Apache-2.0).
  - It downloads data at a pinned version and checksum.
  - The original HumanEval tests are the public check, and EvalPlus's extra inputs (about 80 times as many) are hidden tests graded locally.
  - Runs use this checkout's `workflow.py` in shadow mode, and writer and reviewer must be tool-free backends, so neither can read the hidden files.
  - HumanEval/32 is excluded: its reference fails EvalPlus's own root check on 6 of 888 inputs.

## Running the studies

Keep the study folder outside the checkout. Each run makes at most `max_rounds + 1` model calls: with V3 settings, 2 when the review finds nothing.

```sh
python3 evalplus_study.py run    --out ~/ringo-runs/agcl-evalplus --jobs 4   # 163 tasks
python3 evalplus_study.py grade  --out ~/ringo-runs/agcl-evalplus             # no model calls
python3 evalplus_study.py seed   --out ~/ringo-runs/agcl-evalplus --limit 40 --jobs 4   # 40 review calls
python3 evalplus_study.py report --out ~/ringo-runs/agcl-evalplus             # bounds; no model calls
```

**Interruptions:**
- `run` resumes: finished tasks are skipped, and a run interrupted before its report is set aside and started again.
- It stops launching new runs after three runs in a row fail, for example on a usage limit.
- `--limit N` runs the first N tasks; start with `--limit 5` to check the setup.

**Cost guide** at our V3 averages, before you start:
- the full `run` is about 163 tasks × 2–3 calls;
- the seeded reviews are one call each.

**What to look for:**

| Output | Meaning |
|---|---|
| `graded harm: 0/149` or better among eligible tasks | Under 2% harm at 95% confidence, the bar for a first release of the code stop rule. 59 eligible tasks with zero harms give under 5% |
| `seeded reviews: k/n raised a finding on the planted defect` | Whether the review catches what the checks miss. Read it against the false-alarm line, which gives the same drafts before seeding |

If fewer than 149 tasks are eligible, because round 1 failed the base tests, add tasks: MBPP+ is the next source. Then make the release decision with the dossier above.

## Parallel runs

**The binding limit is your provider's usage limit, not the CPU.** A run spends nearly all its time waiting on model calls, and grading or checking is seconds of local work. With Codex on a ChatGPT plan, about six concurrent runs is where we hit usage limits, so `--jobs 4` is a safe default; raise it only while runs keep succeeding.

**A strong desktop still helps with local work:**
- grading many drafts;
- running local models through Ollama on a GPU;
- the IDE later.

## Running on Windows through WSL 2

1. **Install WSL 2:** follow the README's Windows steps. In PowerShell as Administrator, run `wsl --install`, restart, then open **Ubuntu**.
2. **Work in the Linux home folder** (`~/code/...`), not under `/mnt/c`. Files there are faster, and by default Ringo's owner-only folder permissions (mode 0700) take effect only on the Linux filesystem.
3. **Clone the repository and run the offline checks** inside Ubuntu: see the README's Linux steps. `python3 -m unittest discover -s tests -q` makes no model calls.
4. **Install the model CLIs inside Ubuntu,** not in Windows, so Ringo finds them.
5. **Sign in to Codex** with `codex login`, which opens your Windows browser. If the sign-in page cannot return to the CLI (a blocked localhost callback), use `codex login --device-auth`. This is a beta, and you enable device-code sign-in in your ChatGPT security settings first ([Codex authentication](https://developers.openai.com/codex/auth)).
6. **No release is needed on a fresh machine.** It has no active release, so `ringo.py` and `workflow.py` run the checkout you cloned.
7. **For GPU work later,** the current NVIDIA Windows driver exposes the GPU to WSL 2. Ollama's Linux install inside Ubuntu then uses it.

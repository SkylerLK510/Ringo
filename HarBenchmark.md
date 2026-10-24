# HarBenchmark

> Publication scope (2026-10-24): the independent HarBenchmark scorer and its
> fixture tests are included. Referenced Ringo runtime and adapter prototypes
> are pending separate publication; this upload makes no runtime adoption claim.

HarBenchmark v0.1 is Ringo's runnable **public development benchmark**. It checks
bounded answers across six areas using Python 3.11+ and the standard library.
The implementation and public cases live in [harbenchmark.py](harbenchmark.py).
It makes no model calls and adds no work to Ringo's review/design/work execution.

**Status:** twelve cases implemented; scorer verified with reference fixtures.
No Ringo or competitor model has been evaluated on HarBenchmark yet. Passing
the scorer's tests is implementation verification, not a harness score.

## Cases and scope

All cases are original synthetic fixtures authored for this repository. They
are deliberately small, published alongside their answers, and suitable for
development and adapter checks. They are not a sealed or contamination-resistant
test set. Each track currently has only two cases; a single failure changes its
rate by 50 percentage points.

| Track | Case IDs | What v0.1 checks | What it does not establish |
|---|---|---|---|
| Review | `review-fallback`, `review-clean` | Demonstrated zero-value bug with a counterexample; restraint on a clean implementation | General PR recall, precision, cross-file review or severity calibration |
| Coding | `repair-zero`, `repair-none` | Required Python repair expressions, compared by AST | Arbitrary equivalent repairs, executable repository patches, tests or CI success |
| Design | `design-capacity`, `design-quorum` | Failure-capacity sizing and quorum arithmetic under explicit constraints | Architecture quality, distributed consistency or real performance |
| Artifact work | `artifact-release`, `artifact-table` | Source fidelity, issue/category consistency and correct totals | Document prose, layout, accessibility or editorial quality |
| Research | `research-uncertainty`, `research-drawdown` | Sample uncertainty and synthetic portfolio arithmetic | Original research, live financial advice, causal validity or reproducibility of experiments |
| Evaluation/accounting | `evaluation-macro`, `evaluation-accounting` | Macro/micro distinction, exposed-data adoption limits, bounded calls and unknown-cost handling | Enforcement of Ringo's actual evaluation gates or a measured improvement campaign |

Numeric comparisons use relative tolerance `1e-6` and absolute tolerance `1e-9`.
Booleans cannot stand in for numbers. Object keys and ordered arrays must match
the case contract. Repair expressions permit quotation, parentheses and formatting
differences that produce the same Python AST; they do not execute candidate code.
This intentionally narrow accepted language is stated in the repair prompts.
There is no keyword-based grading of Markdown or architecture prose.

## Run

Keep exports, submissions, model artifacts and reports in a private folder outside
the checkout. Use a new output filename each time; the CLI refuses overwrite.

```sh
python3 harbenchmark.py list
mkdir -p ../ringo-runs/harbenchmark-example
python3 harbenchmark.py export --out ../ringo-runs/harbenchmark-example/manifest.json
# Generate one answer per exported case with the harness being evaluated.
# Populate submission_template from the manifest, preserving its case_ids/hash.
python3 harbenchmark.py score \
  --manifest ../ringo-runs/harbenchmark-example/manifest.json \
  --submission ../ringo-runs/harbenchmark-example/submission.json \
  --out ../ringo-runs/harbenchmark-example/report.json
```

`export --case review-fallback --case review-clean` selects a subset **before**
generation. A subset report states its selected/available coverage. Do not compare
it with a full-suite report. The export is the schedule: a submission cannot
silently shorten it to the answers that completed. Save its hash and generation
settings before running a model; local files do not themselves enforce timing.

The submission has exactly these top-level fields:

```json
{
  "version": "0.1",
  "suite_sha256": "COPY_FROM_EXPORT",
  "metadata": {
    "harness": "Ringo",
    "model": "EXACT_MODEL_AND_SETTINGS",
    "revision": "COMMIT_AND_WORKTREE_PATCH_HASH",
    "run_kind": "model_run",
    "evidence": "PRIVATE_TRACE_PATH_AND_GENERATION_SETTINGS"
  },
  "case_ids": ["review-clean"],
  "answers": {"review-clean": {"status": "ok", "answer": {"defects": 0}}}
}
```

That example is a contract illustration, not a recorded Ringo run. Use
`run_kind: fixture` for hand-written/reference answers. Metadata is self-reported;
the scorer cannot certify that a named harness or model produced an answer.

Records may report `ok`, `failed` or `unsupported`; absent records are `missing`.
Malformed records, wrong values and unsupported/missing answers receive no credit
and remain in the scheduled denominator. An unusable envelope, duplicate JSON
keys, nonfinite numbers, unknown IDs, changed schedule or suite-hash mismatch
rejects the whole submission with exit 2: **no score**, never a successful result.
Do not drop a rejected attempt from a multi-run study. Submission size is limited
to 1 MiB. Scores use one scheduled observation per selected case; repetitions
require separately declared manifests/runs and reporting every attempt.

Reports contain suite identity, metadata, coverage, per-case status/error and
per-track passed/failed/missing/malformed/unsupported counts. Each track's rate is
passed / selected; there is no combined score. Exit 0 means every selected case
passed, 1 means a scored run has failures, and 2 means invalid input or I/O failure.
The SHA-256 covers the complete scorer source, including prompts and references;
even a source-formatting edit creates a new identity and requires a fresh export.

### Using Ringo

For ordinary engineering, keep using Ringo's own prompts and documented
`review`, `design` and `work` commands. HarBenchmark is an offline evaluation
contract, not a replacement agent framework or a new default prompt.

Ringo review/design produce native Markdown, whereas these cases require JSON
companions. A scored attempt must record the exact conversion method and preserve
the native artifact. A human may transcribe explicit values but must not solve a
missing answer; record such a submission as human-assisted and compare it only
with identically assisted arms. No automatic Markdown adapter is supplied in v0.1.

For machine-only generation, `work` can declare `answer.json` as its editable
artifact and use a fixed case checker plus the **separate Design Council** bridge
and a suitable council rubric. This requires a separately configured task and
declared generation budget; the scorer does not start it. Use six council samples
for substantive assessments. Council scores remain separate from these constraint
checks. Ringo's protected `evaluate.py` gates remain authoritative for adopting
review-pipeline improvements; HarBenchmark cannot grant an adoption decision.

## Published comparison statistics

Sources checked 2026-10-23. These are **selected published configurations**, not
a claim to list today's highest scores. Results depend on model, settings,
compute budget, task version and scoring protocol. Blank coverage is explicit;
unmeasured is not zero. No cross-benchmark ranking is computed.

| Harness | Model/configuration named by source | Benchmark / version | Published statistic | Evidence/status |
|---|---|---|---|---|
| Ringo | No matched external run | SWE-bench, Terminal-Bench, PaperBench, GDPval, official ReviewBench | Not measured | ReviewBench adapter exists; no official score claimed |
| Ringo | No HarBenchmark model run | HarBenchmark 0.1, 12 public-dev cases | Not measured | Reference-fixture checks only |
| Codex CLI | GPT-5.3-Codex | Terminal-Bench 2.1 | 79.1% accuracy | Benchmark authors' [2.1 comparison experiment](https://www.tbench.ai/news/terminal-bench-2-1), not a fresh local run |
| Codex CLI | GPT-5.4 | Terminal-Bench 2.1 | 77.3% accuracy | Same [comparison experiment](https://www.tbench.ai/news/terminal-bench-2-1) |
| Claude Code | Opus 4.6 | Terminal-Bench 2.1 | 70.1% accuracy | Same [comparison experiment](https://www.tbench.ai/news/terminal-bench-2-1) |
| Terminus 2 | Gemini 3.1 Pro | Terminal-Bench 2.1 | 70.7% accuracy | Same [comparison experiment](https://www.tbench.ai/news/terminal-bench-2-1) |
| Gemini CLI | Gemini 3.1 Pro | Terminal-Bench 2.1 | 67.1% accuracy | Same [comparison experiment](https://www.tbench.ai/news/terminal-bench-2-1) |
| OpenHands | Claude 4 Sonnet, 2025-05-24 submission | SWE-bench Verified, 500 instances | 352/500 = 70.4% resolved | Historical [official submission artifact](https://github.com/SWE-bench/experiments/tree/main/evaluation/verified/20250524_openhands_claude_4_sonnet) |
| mini-SWE-agent | Launch headline; configuration not specified in headline | SWE-bench Verified | 65% resolved | Historical July 2025 [benchmark-owner announcement](https://www.swebench.com/); not a current best-config claim |
| SWE-agent | No selected fully inspected configuration | Matched benchmark comparison pending | Not entered | Do not substitute a base-model score |
| Goose | No selected fully inspected configuration | Matched benchmark comparison pending | Not entered | No inferred score |
| Open-source PaperBench scaffolding | Claude 3.5 Sonnet (New) | PaperBench, April 2025 release | 21.0% mean replication score | Historical [author evaluation](https://openai.com/index/paperbench/); this is not Claude Code |

Terminal-Bench 2.1 has 89 tasks; the authors revised 28 from 2.0. Its results must
not be mixed with 2.0 or 4.0. The linked comparison does not expose every per-row
budget/uncertainty detail, so those figures do not establish a cost/latency winner.
The live website now presents newer versions; keep historical version labels.

### Existing local Ringo optimization evidence

These are **previously recorded development results**, not new runs in this
change and not official ReviewBench leaderboard results. The local protocol used
an adapted same-file matcher and a subscription-backed judge. Grounded precision
excludes unmatched findings; the unmatched rate is reported to expose that limit.
The recorded writer alias was `opus`; an exact resolved model version was not
pinned in this summary, limiting reproducibility. Full traces remain private.

| Ringo plan | Dev PRs / distinct repos | Macro recall | Micro recall | Grounded precision | Unmatched candidates | Recorded calls/PR |
|---|---|---|---|---|---|---|
| Baseline, one reader | 30 / 30 | 22.1% | 55/249 = 22.1% | 86.8% | 31% | 1.0 |
| Broad review candidate | 30 / 30 | 66.6% | 156/249 = 62.7% | 84.1% | 54% | 1.0 |
| Three-reader candidate plus merge | 30 / 30 | 73.7% | 176/249 = 70.7% | 83.2% | 59% | 4.0 |

Source: the owner's private 2026-10-22 recall-research report, inspected during
this task. Candidate prompts/plans and case answers have not been copied here.
The existing 40-PR validation report is incomplete. No unseen-case gain or new
default adoption follows from this table. The smaller four-PR harness pilot has
different gold selection and grading and is deliberately not pooled with it.

## Benchmark coverage across Ringo's areas

| Area | External benchmark/reference | Relevant measurement | Ringo status / remaining gap |
|---|---|---|---|
| Repository coding | [SWE-bench Verified](https://www.swebench.com/), [FeatureBench](https://arxiv.org/abs/2602.10975) | Issue resolution; feature-oriented development | Not measured; HarBenchmark repair expressions are much narrower |
| Terminal/tool execution | [Terminal-Bench](https://www.tbench.ai/news/terminal-bench-2-1) | End-to-end terminal tasks | No adapter/run; not equivalent to tool-free reviews |
| Code review | [ReviewBench](https://github.com/review-bench/ReviewBench) | Human-reviewed golden findings on 219 PRs | Adapter exists; local development evidence above, official result absent |
| Architecture/maintainability | [SlopCodeBench v2](https://arxiv.org/abs/2603.24755v2) | Repeated extensions across 36 problems / 196 checkpoints, erosion and redundancy | Not measured; numerical constraints plus Council assessment cover only part of this |
| Professional artifacts | [GDPval public gold set](https://huggingface.co/datasets/openai/gdpval) | 220 tasks across 44 occupations | Not measured; formatting and human review need separate checks |
| ML/scientific research | [PaperBench](https://openai.com/index/paperbench/), [ScienceAgentBench](https://arxiv.org/abs/2410.05080) | Paper replication; scientific data-analysis tasks | Not measured; synthetic arithmetic is an adapter/development check |
| Finance research | [GDPval](https://huggingface.co/datasets/openai/gdpval) includes finance-related work | Professional deliverables, not a trading-validity guarantee | No matched harness statistic; requires temporal leakage, fees and independent numerical checks |
| Evaluation/improvement | Existing Ringo guarded evaluator; [NIST evaluation-cheating analysis](https://www.nist.gov/caissi/cheating-ai-agent-evaluations) | Provenance, grading integrity and measured generalization | Offline regressions exist; no benchmark establishes automatic self-improvement |
| Future security/CI bot | [PrimeVul](https://arxiv.org/abs/2403.18624), [CI-Repair-Bench](https://arxiv.org/abs/2604.27148) | Vulnerability detection; full-workflow repair validation | Separate proposed product; not implemented in Ringo |

For benchmark blind spots, proposed harder cases, unresolved research questions
and the no-added-interactive-wait requirement, see
[BENCHMARK-GAPS.md](docs/BENCHMARK-GAPS.md) and
[SECURITY-CI-BOT-RESEARCH.md](docs/SECURITY-CI-BOT-RESEARCH.md).

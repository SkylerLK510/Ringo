# Overfitting guardrails after runtime optimization

Research and code audit: 2026-10-25. This audit covers the published release,
latency, evaluation and improvement implementations. It makes no new model
quality claim and does not run a real improvement campaign or spend held-out
case submissions. All added adversarial cases use independent synthetic fixtures.

## Assessment

The traversal optimization changes how local files are enumerated and hashed.
It does not select cases, read answer keys, change review prompts, loosen scoring
thresholds or add provider calls. The integrity regressions support those
specific properties; they cannot prove that the broader harness generalizes.
Publication also includes previously implemented review/workflow changes. Their
offline correctness tests are not evidence of improved review quality.

The native Ringo review found a genuine evidence-transfer gap: a plan with an
existing sealed-test pass could be staged alongside different review runtime
code. This publication fixes it by matching every evaluator-frozen code file in
the staged bundle to the checked experiment implementation, recording that hash
and rechecking at activation. Activation also regenerates the expected checked-hash
bytecode for those evaluated Python sources and compares the executable files,
so a substituted bytecode body cannot borrow matching source hashes. This
compilation is confined to RRSI activation and adds no ordinary review pass.
The separate stable bootstrap remains identified
by its own hash; the claim is scoped to the evaluated review configuration, not
the whole IDE or every runtime feature.

The audit also identified that noncomparative experiments can legitimately
complete without establishing improvement. Release adoption now requires
comparative sealed-test evidence for an RRSI claim and records its objective and
comparators. Baseline experiments remain available; evaluator thresholds and
protected decision rules are unchanged. A cost objective must not be reported as
proof of quality superiority. The owner's existing local deployment has no RRSI
quality claim, so no measured quality gain is being revoked or invented here.

The review's second defect was a possible hang when a JSON metadata file was
replaced by a FIFO. Pointer, manifest and approval reads now require regular
files before opening them. This is a failure-handling fix, not an overfitting
defense or a same-user concurrent-mutation security boundary.

## Research basis

Adaptive selection can fit repeated feedback from a holdout, even when each
individual evaluation seems reasonable. Dwork and colleagues study this problem
and provide particular reusable-holdout methods; Ringo does **not** implement
their differential-privacy guarantee. A submission cap is a practical exposure
limit, not that mathematical guarantee. [Dwork et al., 2015](https://arxiv.org/abs/1506.02629v2).

RRSI regularizes both proposals and selection: attributable edits and recorded
negative evidence limit adaptive search, while leakage screening, noise-aware
selection and resource constraints resist benchmark-specific gains. The paper
evaluates transfer using fixed held-out tasks and other benchmarks, and lists
finite feedback sets and search hyperparameters among its limitations. These
ideas motivate checks; the paper's results do not establish Ringo's quality.
[RRSI, version 3, sections 3 and Appendix A](https://arxiv.org/html/2609.24972v3).

NIST documents agents exploiting evaluation access, including obtaining solutions,
altering assertions and adding task-specific logic. Its guidance includes
transcript inspection and stronger evaluation-environment controls. Ringo's
canaries and tool declarations address some observable paths; they do not make
answer keys inaccessible to a process running as their owner. Automated review
also needs human verification. [NIST examples](https://www.nist.gov/caissi/cheating-ai-agent-evaluations),
[NIST prevention guidance](https://www.nist.gov/caissi/4-practices-detecting-and-preventing-evaluation-cheating).

## Implemented controls and their limits

| Risk | Code and existing control | Practical limit / required interpretation |
|---|---|---|
| Tuning on held-out cases | `improve.evidence` accepts dev only; `evaluate.split` keeps repository groups together and only moves previously exposed cases toward dev | Group labels must be correct; forks, near duplicates and public benchmark memorization are not automatically detected |
| Repeated validation shopping | `evaluate.freeze` caps submissions across campaigns sharing validation cases; policy is fixed in the ledger | A new store is a new trust boundary; there is no global registry preventing duplication or operator resets |
| Test reuse | One open test experiment; one decision; test cases retired on decision or void; exposed/retired cases cannot reenter test | Protection assumes the store history remains authoritative; operator access can bypass local controls |
| Optional stopping / noise winners | Test decisions spend decreasing lifetime alpha; paired cluster-t and cluster-bootstrap bounds must pass every required gate | Bounds are approximate; alpha accounting does not make poorly calibrated bounds exact |
| Biased or permissive judges | Test requires at least two trusted provider families, blind review keys and passing results from every grader | Distinct families need not make independent or correct judgments; aliases do not pin remote model weights |
| More findings mistaken for quality | Utility, recall, extra-wrong-finding bounds and cost gates are separate requirements | Results depend on gold completeness and grader validity; unmatched findings need explicit treatment |
| Missing/failing runs silently dropped | Ledger seals, exact grade coverage, interrupted/failed outcomes and unknown-cost checks fail closed | Preserve all scheduled attempts and publish failure counts; do not present selective subsets as a complete study |
| Answer-key leakage | Proposal scope and growth limits, contamination matching, canaries, protected prompts and dev-only feedback | Phrase matching can miss paraphrases and can reject coincidental matches; no filesystem isolation is provided |
| Stale pass transferred to new behavior | Checked input digest, normalized plan binding, staged evaluated-code match, activation recheck, scoped backend/checker/default-plan claim | Only the frozen evaluation scope is covered; design, work, overrides, remote provider drift and external resources are not certified |
| Fast failures or latency tuning | Complete paired schedule, failure-rate checks, per-workload mean/p95 absolute/relative limits, exact launcher identity | Synthetic workloads and a single host do not establish live-provider, cold-cache, concurrency or IDE performance |

The implementation links are [evaluation](../evaluate.py),
[improvement](../improve.py), [release management](../releases.py),
[latency assessment](../latency.py) and [experiment runner](../bench_latency.py).

## New adversarial cases

`tests/test_overfitting_guardrails.py` checks that changing campaign names and
resplitting the same validation pool do not reset the submission allowance. Its
in-memory decision fixtures require rejection for excess wrong findings,
nonpositive recall, a failing second judge, altered artifacts, missing grades,
too few groups, unknown budgeted cost and failed slots. It also rejects attaching
an RRSI improvement claim to a noncomparative test pass.

`tests/test_releases.py` changes each evaluator-frozen code file in turn and
requires candidate staging to reject the mismatched implementation. It also
constructs a self-consistent fixture bundle whose manifest hashes are valid but
whose sealed-test claim belongs to different code; activation must reject it.
A further case preserves a legitimate source-hash header while substituting the
executable bytecode body and updating its manifest hash; activation rejects that
self-consistent bundle too. A positive control stages and activates matching
comparative proof in an isolated fixture store.
The existing suite covers exposed/retired cases, canaries, unsealed artifacts,
frozen-input changes and failed attempts that cannot be selectively retried.

`tests/test_optimization_cases.py` checks fresh-process revalidation, metadata
FIFOs and complete failure/timeout denominators. Mock timings and mock evaluator
decisions are explicitly unit-test setup, never adoption evidence or benchmark
scores. These checks add no model passes to ordinary reviews.

## Claims that remain unsupported

HarBenchmark and the Optimize cases are public development material. Passing
them cannot establish unseen-case quality. The earlier recall validation remains
incomplete. No test pass, unit-test count, Council recommendation, dev gain or
latency improvement substitutes for a valid comparative sealed-test experiment
and explicit human adoption of an RRSI candidate.

The evaluator freezes its declared code, plan/prompt contents, case artifacts and
backend command identities. It cannot freeze remote model internals, every
external file a tool reads or the model's pretraining data. Repository clustering
does not detect semantic duplicates. With few groups, or bootstrap tails near
the minimum resolution, a nominal confidence bound can be unstable. These are
open validity limits rather than guarantees established by an increasing test
count.

## Conditions for a future quality claim

Before spending on a campaign, declare its scope, hypotheses, search/submission
budget, metrics, group construction, resolved provider settings, grader protocol
and practical acceptance margins. Keep a fixed untouched test pool under an
independent access boundary when tool-enabled runs could otherwise read keys.
Preserve unsuccessful proposals and every attempted run. Review transcripts for
answer lookup, task-specific shortcuts, grader manipulation and altered tests.

After selection, freeze the candidate, use fresh comparative sealed-test evidence
and report uncertainty, failure counts, cost and the actual evaluation scope.
For broad claims, independently evaluate transfer outside the development
benchmark and check negative controls such as clean code and plausible false
positives. These are recommended future protocols; no new campaign or external
isolation service is installed by this audit.

For latency, use the preserved before-version bootstrap and the same fixed
workload protocol; retain uncertain outcomes and do not repeat collections until
they pass. The earlier 5 ms / 5% defaults remain provisional diagnostic budgets,
not a user-approved production SLO. The previous verifier study remains the
evidence for its measured speedup; guardrail fixes require their own assessment.

## Final verification and latency assessment

The final full suite ran **329 tests, OK, 2 skipped**, including twelve new test
methods and their parameterized scenarios. GitHub Actions also ran all 329 tests
successfully on Python 3.11 and 3.14. Native Ringo review used two calls: the
initial publication review identified the implementation-binding and metadata
issues; follow-up review reported no defects and raised the executable-bytecode
question, which was confirmed and fixed with a negative case and positive
control. Its other questions were resolved by inspecting the evaluator and
running the corresponding tests. Model review is an assessment of supplied
evidence, not a statistical quality evaluation.

The final code was measured after tests and inference ended, using 100 randomized
pairs plus five warmup pairs for each of nine workloads: 900 measured pairs and
1,890 fresh CLI processes. Baseline and candidate used separate before/after
launchers and matched private synthetic approvals of 58,186 bytes each. The
baseline application was the already approved release; the final candidate
source and bytecode were verified against the measurement bundle. Normal user
approval state was not changed by collection. No provider calls were made.

| Workload | Before p50 ms | After p50 ms | Median delta ms | Diagnostic assessment |
|---|---:|---:|---:|---|
| startup | 34.461 | 34.567 | +0.106 | negligible |
| review | 66.281 | 66.403 | +0.122 | negligible |
| review-trace | 68.030 | 68.270 | +0.241 | negligible |
| review-large | 68.492 | 68.561 | +0.069 | uncertain |
| design | 67.425 | 67.555 | +0.130 | uncertain |
| work | 279.312 | 279.243 | -0.069 | uncertain |
| review-delay | 127.827 | 127.960 | +0.133 | uncertain |
| review-override | 67.288 | 67.356 | +0.068 | uncertain |
| review-failure | 87.550 | 87.724 | +0.174 | uncertain |

Startup, ordinary review and traced review fit both the provisional 5 ms and 5%
mean/p95 budgets using simultaneous paired-bootstrap bounds. Six workloads
remained uncertain; none demonstrated a beyond-budget slowdown. The ordinary
review median rose by about 0.12 ms. This is not a claim of uniformly negligible
latency, improved quality, or live-provider/IDE performance.

All non-diagnostic runs succeeded. The 210 intentionally failing executions,
including warmups, remained failures. All 200 measured work runs made exactly
one writer and six simulated judge calls. Raw pairs, complete schedules, bounds,
source identities and the preregistered protocol remain private.

An earlier complete collection used the real baseline approval (108,384 bytes)
against a smaller synthetic candidate approval (58,186 bytes). That size
mismatch confounds attribution to the code change, so it is retained as
exploratory evidence, not the final comparison. Final code also gained the
activation-only bytecode check after the follow-up review. The revised protocol
specified matched approvals before the final collection; sample counts, budgets
and failure treatment were unchanged. Collection was not repeated to seek a
passing verdict, and all uncertain results are retained.

The existing application bundle remains pinned. The reviewed metadata and
adoption-evidence fixes belong to the separately identified stable bootstrap and
management code. Their publication or local application does not attach an RRSI
quality claim or automatically activate a newly staged application candidate.

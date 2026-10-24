# Where frontier agent benchmarks leave gaps

Research checked 2026-10-23. This is a coverage analysis, not a new benchmark
result or a declaration that one benchmark is universally best. New suites
already address several of these gaps. The useful question is which claim a
particular version, metric and execution protocol can support.

**Evidence** below means a limitation or observation documented by the cited
authors. **Ringo implication** and proposed cases are our engineering inferences.
None of the proposed harder cases is part of HarBenchmark v0.1 yet.

## Evidence and proposed cases

| Area | What a strong published score can leave untested | Primary evidence | Proposed HarBenchmark extension and metric |
|---|---|---|---|
| Patch correctness | Passing a task's tests may disagree with its written requirements; public fixes may be memorized | OpenAI's [SWE-bench Verified audit](https://openai.com/index/why-we-no-longer-evaluate-swe-bench-verified/) found faulty tests in **59.4% of 138 deliberately selected hard problems**, not 59.4% of all 500; it also found contamination evidence | Fresh private tasks with independently written property tests; count test/spec disagreements and novel-task pass rate separately |
| Feature delivery | Resolving an isolated issue does not measure a feature spanning multiple changes and compatibility requirements | [FeatureBench](https://arxiv.org/abs/2602.10975) builds executable feature tasks spanning commits/PRs | A feature plus two downstream consumers and a backwards-compatibility check; report new-feature and preserved-behavior rates |
| Long-term design | Current tests can pass while repeated extensions accumulate duplication and concentrated complexity | [SlopCodeBench v2](https://arxiv.org/abs/2603.24755v2) studies 36 problems and 196 checkpoints; it measures erosion/verbosity in addition to correctness | Five evolving requirements on the same artifact; report final behavior, regressions, touched modules, complexity change and independently assessed design decisions |
| Environment reliability | Task defects, external services and moving dependencies can change scores without an agent improvement | [Terminal-Bench 2.1](https://www.tbench.ai/news/terminal-bench-2-1) fixes issues in 28 of 89 tasks, including changed external dependencies | Inject package/network/cache failures in pinned environments; separate model failure, infrastructure failure, recovery success and elapsed time |
| Review completeness | Agreement with a finite golden set is not the same as finding every real problem or controlling every false alarm | [ReviewBench methodology](https://github.com/review-bench/ReviewBench/blob/main/docs/METHODOLOGY.md) distinguishes golden-set metrics from augmented metrics for unmatched findings | Clean negative PRs, seeded cross-file defects, duplicate gold items and valid unmatched findings; report critical recall, false positives/clean PR, duplicate findings and unmatched rate |
| Reviewer/judge dependence | A semantic judge can miss paraphrases, favor particular phrasing or validate extra findings incorrectly | ReviewBench [local judging](https://github.com/review-bench/ReviewBench/blob/main/docs/JUDGING.md) uses the selected model both for matching and unmatched classification | Blind paraphrase pairs, independent human adjudication and a different-provider judge; report agreement and confidence intervals, not just one judge's score |
| Artifact quality | A polished one-shot deliverable need not preserve source facts through revision or survive real use | [GDPval](https://openai.com/index/gdpval/) explicitly notes its one-shot setup omits context-building and multiple drafts | Revise a report twice as source data change; check cited facts, table arithmetic, preserved requirements, rendered layout and reviewer correction time |
| Scientific validity | Replication progress is not original discovery; convincing code or prose is not reproduced evidence | [PaperBench's limitations](https://arxiv.org/html/2504.01848v1#S7) include dataset size, contamination, judge performance and cost; [ScienceAgentBench](https://arxiv.org/abs/2410.05080) targets specific scientific analysis tasks | Reproduce a numerical claim from fresh inputs; independently rerun experiments, audit data lineage, vary seeds, include negative results and assess uncertainty |
| Finance research | Correct arithmetic or a professional-looking report does not establish tradable out-of-sample results | GDPval's professional deliverables have a different target from temporal strategy validation; **this finance gap is our scope inference**, not a claimed finding of its authors | Time-ordered data with revised timestamps, fees, survivorship and a regime shift; detect future-data use and report net results with uncertainty |
| Security detection | Vulnerability labels, duplication and unrealistic prevalence can inflate accuracy; detecting a vulnerability is different from detecting leaked credentials | [PrimeVul](https://arxiv.org/abs/2403.18624) documents dataset-quality and evaluation problems and introduces deduplication/chronological splits | Vulnerable/patched pairs, realistic clean-code prevalence, synthetic secret fixtures and contextual exfiltration flows; report recall at a fixed false-positive budget |
| CI/CD correctness | Unit-test success does not cover workflow syntax, matrix environments, packaging, permissions or deployment behavior | [CI-Repair-Bench](https://arxiv.org/abs/2604.27148v2) uses 567 failures from 103 repositories and validates with original full workflows | Pinned workflow reproductions with code/config/dependency faults; report full required-job success and distinguish fixes from skipped/weakened checks |
| Evaluation integrity | Agents can exploit grader access or implementation loopholes instead of solving the intended task | [NIST CAISI](https://www.nist.gov/caissi/cheating-ai-agent-evaluations) documents solution contamination and grader gaming | Separate trusted checks and answer keys from agent access; inject canaries and tampering attempts; report rejected attempts and failure provenance |
| Latency and real operating cost | Accuracy does not identify the best harness at the same time, token or dollar budget; unknown subscription cost is not zero | ReviewBench reports review duration, so latency is **not universally absent**. Published protocols still need matched resource budgets for our comparison | Run paired fixed-budget arms; record total and per-role calls, retries, p50/p95/p99 elapsed time, resource use, failure rate and unknown-cost count |

For SlopCodeBench, use v2's numbers above: earlier summaries with 20 problems / 93
checkpoints refer to an earlier version. For Terminal-Bench, 2.0, 2.1 and 4.0
are different evaluations. These version changes are themselves a reason to pin
datasets, scorers, environments and result dates in every table.

Grounded review precision can omit unmatched findings from its denominator.
Reporting a high grounded score alone can therefore hide a substantial volume
of novel findings, good or bad. ReviewBench's augmented metrics attempt to
address this; our earlier local recall study did not run its full official
pipeline. Its development numbers must retain that limitation.

## Knowledge gaps in the current Ringo evidence

The local recall study establishes development behavior, not external frontier
standing. Its validation report is incomplete. The first useful continuation is
an integrity/status audit of scheduled cases and grading, followed by completion
under its existing preregistration and an explicitly supplied inference budget.
Do not inspect hidden per-case validation evidence to tune a candidate, redraw
the splits, or silently substitute a newer judge. Do not use a public-dev pass
as authorization to adopt a prompt.

The most important unresolved questions are:

1. Does broader review improve high-impact, independently adjudicated findings
   on fresh repositories, or mainly match a public benchmark's broader taxonomy?
2. How much does a changed context builder affect recall at the same token budget,
   particularly docs-only changes and unchanged callers? Missing evidence and
   insufficient reasoning need separate labels.
3. Do any multi-pass gains survive equal time/token budgets, retries included?
4. How often do the matcher and human reviewers disagree, especially on duplicate
   findings, partial claims and new valid defects outside the golden set?
5. Does Council recommendation predict executable correctness or reduced human
   revision effort? Agreement among judges alone cannot answer this.
6. Can a separate security/CI bot meet the requested latency constraint under real
   contention? There is no performance measurement supporting that claim yet.

## Order of expansion

Start with clean negative reviews, cross-file and docs/config changes, and
independent reference calculations. Next add repeated artifact revisions and
failure recovery. Then build externally isolated repository/CI tasks and security
fixtures for the separate bot. Each stage needs independent checks and observed
failures before becoming a headline metric.

Keep public development fixtures separate from fresh, private evaluation tasks.
Use repository/time-disjoint groups, report every scheduled attempt, and publish
uncertainty rather than collapsing these areas into a single average. Run
statistical comparisons through Ringo's existing guarded evaluator where its
review-task contract applies. Other domains need separately scoped evaluation
protocols; this document does not extend or weaken those gates.

HarBenchmark v0.1 currently checks simple structured constraints only. It does
**not** solve the long-horizon, semantic, security, CI execution, contamination,
judge-calibration or latency gaps described here. Its first purpose is to provide
a reproducible contract for developing those evaluations without invented scores.

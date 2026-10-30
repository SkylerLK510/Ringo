# Research documents

This index collects the current Ringo research and experiment summaries. Sources
and benchmark versions are recorded in the individual documents. Updated
2026-10-25. Published results distinguish external reports, local observations,
proposals and unmeasured areas.

| Document | Scope | Status |
|---|---|---|
| [HarBenchmark](../HarBenchmark.md) | Twelve public development cases, reproducible scorer contract and sourced comparison tables | Standalone scorer and fixture tests included; no model HarBenchmark score |
| [Benchmark gaps](BENCHMARK-GAPS.md) | What coding, review, research, artifact and security benchmarks leave untested | Research and proposed harder cases |
| [Security and CI bot](SECURITY-CI-BOT-RESEARCH.md) | Separate future bot, threat/detection coverage and latency acceptance questions | Proposal; no scanner or IDE integration installed |
| [Release architecture](RELEASES.md) | Versioned bundles, explicit activation/rollback, pinned runs and a separate latency gate | Implementation and tests published; explicitly active in the owners local installation |
| [Local latency results](LATENCY-RESULTS.md) | 900 paired measurements across nine workloads using simulated backends | Local observations; eight workloads remain uncertain under provisional budgets |
| [Verifier optimization results](VERIFIER-OPTIMIZATION.md) | Implemented local traversal change, reviewed source patch, 2,000 component pairs and 900 CLI pairs | Verifier component improved; whole-command tails remain uncertain; explicitly adopted locally on 2026-10-25 |
| [Overfitting guardrails](OVERFITTING-GUARDRAILS.md) | Primary-source research, evaluation audit, evidence binding and adversarial regressions | Strengthened RRSI release claims; remaining generalization and isolation limits explicit |
| [Optimization regression cases](OPTIMIZE-CASES.md) | Release rejection before inference, fresh-invocation rechecks and complete timing schedules | Implementation and offline regression tests included |
| [Latency optimization options](LATENCY-OPTIMIZATION.md) | Component profiling, possible optimizations and validation plan | Measured component costs; proposed improvements are not claimed speedups |
| [Research requirements](RESEARCH-REQUIREMENTS.md) | Evidence, reproducibility and ML/finance validation goals | Requirements and future-work scope |
| [Harness guide](HARNESS.md) | Runtime/evaluation architecture, provenance and guardrails | Implementation and tests included |
| [ReviewBench adapter](../adapters/reviewbench/README.md) | Container/contract integration and scoring procedure | Adapter source and offline tests included; no official result claimed |
| [Council workflow](COUNCIL-WORKFLOW.md) | Executable checks and separate Design Council assessment | Implementation and tests included |
| [Engineering objectives](ENGINEERING-OBJECTIVES.md) | Coding ability, design judgment, architectural insight and quality-conditioned token efficiency | Requirements and proposed pilot; no campaign or measured gain |
| [Research role](RESEARCH-ROLE.md) | `ringo.py research`: one search call, a deterministic quote verifier with a guarded fetcher, one tool-free write call | Implementation and offline tests; unmeasured, no quality or token claim |
| [Audit-Gated Continual Learning](AGCL.md) | Learning when to stop spending tokens, with labels collected at no added token cost and changes only through releases | `stop_on_checks`, shadow labels and the HumanEval+ studies implemented; study results pending |

## Publication boundary

The repository includes the runtime release/verifier implementation, latency
assessment and experiment tools, matching regression tests, workflow integration
and ReviewBench adapter sources. The research summaries remain separate from
those executable checks. The owner explicitly adopted the measured release
locally; a clone does not deploy a release or adopt an RRSI candidate. Existing
sealed-test requirements and human adoption remain in force.

Raw traces, credentials, private case material, sealed answer keys, native-login
artifacts and experiment stores stay outside the repository. Local experiment
summaries cannot be presented as official leaderboard results. Uncertain latency
acceptance remains a human decision.

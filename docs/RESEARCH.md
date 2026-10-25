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
| [Release architecture](RELEASES.md) | Versioned bundles, explicit activation/rollback, pinned runs and a separate latency gate | Active in the owners local installation; repository runtime publication separate |
| [Local latency results](LATENCY-RESULTS.md) | 900 paired measurements across nine workloads using simulated backends | Local observations; eight workloads remain uncertain under provisional budgets |
| [Verifier optimization results](VERIFIER-OPTIMIZATION.md) | Implemented local traversal change, reviewed source patch, 2,000 component pairs and 900 CLI pairs | Verifier component improved; whole-command tails remain uncertain; explicitly adopted locally on 2026-10-25 |
| [Latency optimization options](LATENCY-OPTIMIZATION.md) | Component profiling, possible optimizations and validation plan | Measured component costs; proposed improvements are not claimed speedups |
| [Research requirements](RESEARCH-REQUIREMENTS.md) | Evidence, reproducibility and ML/finance validation goals | Requirements and future-work scope |
| [Harness guide](HARNESS.md) | Runtime/evaluation architecture, provenance and guardrails | Includes local prototype capabilities pending runtime publication |
| [ReviewBench adapter](../adapters/reviewbench/README.md) | Container/contract integration and scoring procedure | Local prototype; no official result claimed |
| [Council workflow](COUNCIL-WORKFLOW.md) | Executable checks and separate Design Council assessment | Includes local prototype capabilities pending runtime publication |

## Publication boundary

This upload publishes research documents and the independent HarBenchmark scorer
with its tests. It does not publish the separate unfinished runtime changes,
modify protected evaluation policy, deploy a release, or adopt an RRSI candidate.
Some implementation guides therefore describe features available in the tested
local prototype before their runtime changes reach the default branch.

Raw traces, credentials, private case material, sealed answer keys, native-login
artifacts and experiment stores stay outside the repository. Local experiment
summaries cannot be presented as official leaderboard results. Uncertain latency
acceptance remains a human decision.

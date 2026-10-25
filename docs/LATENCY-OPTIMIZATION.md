# Ringo latency optimization options

**Follow-up (2026-10-25):** the targeted inventory-traversal optimization is now
implemented and tested in the local runtime prototype. See the
[measured results and source patch](VERIFIER-OPTIMIZATION.md). The original
profile and options assessment below are retained as historical context;
approval receipts, cross-task caching and persistent workers were not implemented.

Research checked 2026-10-24. This note assesses measured local component costs and unimplemented options. It makes no additional runtime change. Raw profiles stay outside the repository.

## Decision and scope

Prioritize a narrowly scoped inventory-traversal experiment while preserving the current verification contract. Retain lazy imports, checked bytecode and direct dispatch as the baseline. Investigate request-scoped reuse only where repeated work within one task is demonstrated. Defer compact approval receipts until explicit human review of their changed evidence-verification semantics. Treat a persistent IDE worker as a separate architecture proposal.

This assessment proposes research only. It makes no runtime change, adoption decision or performance-gain claim. Normal activation remains off; the user decides whether uncertain latency is acceptable.

Scope is Python 3.11+ and its standard library, within a same-user threat boundary. The proposals do not establish protection against a malicious actor with the same account’s ability to modify application files and state.

## Known measurements

The tested prototype selects one approved bundle before runtime imports, verifies all source and checked-bytecode files plus approval, hashes bootstrap helpers, and imports harness/workflow directly. RRSI quality evaluation and adoption gates remain protected. Staged checked bytecode and direct dispatch already superseded the initial uncompiled 65→83 ms path.

The final end-to-end experiment used 100 randomized paired measurements per workload, five warmups and nine workloads: 900 measured pairs, simulated backends and zero real provider calls.

| Workload | Before p50 | After p50 |
|---|---:|---:|
| Small review | 67.28 ms | 70.67 ms |
| Design | 67.89 ms | 71.26 ms |
| Work round | 314.01 ms | 317.85 ms |
| Startup/help | 40.55 ms | 35.00 ms |

The 5 ms and 5% diagnostic budgets were not approved production SLOs. One workload fit those budgets; eight remained uncertain. These results establish neither a clear final beyond-budget slowdown nor that the entire change is negligible.

The newer profile is warm-cache, in-process component timing, not a fresh-process end-to-end comparison. It used 500 observations per component, 20 warmup rounds and randomized order with seed 24. Its explicit synthetic approval fixture came from the prior experiment; normal state was unchanged.

| Component | Median ms | Mean ms | p95 ms |
|---|---:|---:|---:|
| Pointer read | 0.027791 | 0.028549976 | 0.033 |
| Bundle inventory and hashes | 1.2506665 | 1.273942 | 1.37325 |
| Approval parse and hash | 0.6214375 | 0.628159522 | 0.687042 |
| Bootstrap hashes | 0.0489795 | 0.055343542 | 0.059959 |

The fixture contains 30 bundle files totaling 688,588 bytes and a 58,186-byte approval. The release identifier is fe736b7789cb1e9b1ba357f3a234116bf8ea8e96387fb8994fc2499a56b268bc.

## Load and ceiling

No command arrival rates, concurrency, future loads, hardware capacities, quotas or deadlines are supplied, so resource saturation and a load-dependent breaking point cannot be calculated.

Inventoried bundle-file contents plus the approval total 746,774 bytes, excluding the release manifest, bootstrap helpers and filesystem metadata. The four component medians sum to approximately 1.949 ms, calculated from the supplied values; that sum is not an observed launch latency or achievable saving.

Inventory and hashing is the largest measured component, but it is not an established system bottleneck. Approval parse-and-hash’s approximately 0.62 ms median is a rough ceiling on eliminating that component at this fixture size, not a guaranteed receipt saving. Disk bandwidth, metadata costs, CPU contention and worker queue capacity remain unmeasured.

## Options and tradeoffs

At the time of this options assessment, the proposed improvements below were unmeasured; the traversal follow-up above now has its own measurements.

| Option | Latency opportunity | Integrity and approval requirements | Complexity and applicability |
|---|---|---|---|
| Inventory traversal | Remove demonstrably redundant traversal/stat work within the 1.25 ms measured component; full hashing remains | Preserve complete inventory, unexpected-file detection, symlink rejection and full content hashes | Smallest experiment; pathlib may already use scandir internally, so changing API names alone establishes no benefit |
| Compact approval receipt | Reduce work in the approximately 0.62 ms approval component, with receipt-verification overhead still payable | Receipt references immutable, content-addressed full evidence; verify full evidence at activation/audit and receipt at launch | Changes when altered evidence is detected; requires explicit human review before implementation |
| Request-scoped reuse | Avoid demonstrated duplicate verification within one task; frequency and savings unknown | Preserve pinned identity and per-task approval; no stat-only trust across independent commands | Requires precise task lifetime and invalidation rules; useful only if duplicate work exists |
| Persistent IDE worker | Potentially amortize Python startup; no measured benefit supplied | Preserve per-task approval, identity pinning and verification boundaries | Separate future architecture requiring queue policy, isolation, lifecycle management and cache invalidation |
| Retain lazy imports, bytecode and direct dispatch | Preserve the tested baseline; additional savings unknown | Keep all verification and RRSI gates | Lowest-risk baseline; avoid eager imports and extra model passes |

No stronger option is established by the evidence. Do not disable checks or add critical-path background rescanning. Threads or processes for these tiny hash workloads require demonstrated net benefit before consideration.

## Failure behavior and checks

For an inventory candidate, pass requires unchanged rejection of unexpected files, symlinks, missing files and content corruption, including checked bytecode. Bootstrap helpers currently have a separate recorded hash; preserve that identity and do not claim that the current baseline rejects every changed bootstrap helper. A concrete failure test adds an unexpected file to an otherwise approved bundle: launch must reject it before runtime imports. Repeat with a symlink and modified approved content. Candidate verification errors must fail closed, without falling back to unchecked dispatch.

For receipts, malformed receipts, missing referenced artifacts and activation/audit digest mismatches must block the relevant operation. Evidence altered after activation could remain undetected until the next full verification; content addressing alone does not prevent local mutation. Review must explicitly accept or reject that detection delay and define audit timing.

For reuse, task completion ends reuse eligibility. A new task must obtain its own approval decision; identity or scope mismatches must trigger fresh verification or rejection. Reuse assumes verified material remains stable during its permitted lifetime; that assumption needs validation.

A worker proposal must define bounded queueing, overload rejection, crash recovery and invalidation before adoption testing. Available information cannot establish its capacity ceiling.

## Minimal A/B protocol

Test one candidate at a time against the current baseline using identical material, model settings, simulated-backend behavior and environment. Use randomized paired fresh-process runs for launch claims; keep component profiles separate. Preserve the prior experiment’s five warmups, 100 pairs per workload and nine workloads for comparability, while recording cache conditions and measurement uncertainty.

Keep raw data private. Report every attempted run in the denominator, including errors, timeouts, verification rejections and exclusions with reasons. A security-behavior regression fails regardless of timing. Define performance acceptance criteria with the user before evaluation; the earlier diagnostic budgets do not automatically become gates. Do not tune workloads, exclusions or repetitions to pass thresholds. No future-load test can be specified until a future load is supplied.

## Priority and unresolved questions

Proceed in this order: preserve the current baseline; measure redundant inventory work; assess demonstrated within-task duplication; review receipt semantics; consider a persistent worker separately.

The decision would change with evidence of substantial duplicate verification, materially larger bundles or approvals, or a sustained IDE workload. Unresolved questions are acceptable latency, representative filesystem/cache conditions, task lifetime and mutation guarantees, evidence-audit timing, and expected command concurrency.

## Primary guidance

Python's [os.scandir documentation](https://docs.python.org/3/library/os.html#os.scandir) describes directory-entry metadata reuse; it does not establish an improvement over this Ringo implementation. [pyperf's system guidance](https://pyperf.readthedocs.io/en/stable/system.html) explains benchmark stability and system noise. These sources support methodology/API choices, not measured Ringo speedups.

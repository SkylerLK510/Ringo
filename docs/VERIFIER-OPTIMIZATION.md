# Verifier traversal optimization

Research updated 2026-10-25. This change is implemented, tested and **active in the owners local Ringo installation** following explicit user adoption. The repository default runtime is still awaiting separate publication. A reviewable [implementation patch](patches/verifier-traversal.patch) is included for the prototype baseline; it is not an update applied to the repository default runtime.

## Local adoption on 2026-10-25

The normal local release store now selects release `e777d95353ba6de294a3deec5f2e83b1c2c44533ec02f045a322c809f43ebe42`. It is the exact immutable candidate measured below, staged again with matching source and bytecode identity. The full suite was rerun before activation: 317 tests, OK, 2 skipped. The user explicitly requested future use after disclosure of the uncertain whole-command tails; that decision and rationale are recorded in the private approval. Activation recomputed the complete measured latency suite and retained all existing release gates. No RRSI quality claim or protected-policy change was made.

New invocations through the stable `ringo.py` launcher use that local release. The optimized verifier remains part of the stable bootstrap and its separate hash is recorded in run provenance. Local activation does not deploy the runtime to other machines or apply the pending runtime sources to GitHub main.

## Change and integrity contract

The verifier now validates manifest paths/hashes and checks inventory/content in one iterative `os.scandir` walk. Directory-entry metadata replaces redundant path type queries. Every expected source and bytecode file still receives a complete SHA-256 read on every verification. Missing/extra files and file/directory/dangling symlinks remain rejected. There is no cross-command verification cache and no approval-receipt change. Empty unregistered directories remain compatible. Unsupported special files, previously omitted from the regular-file inventory, are now explicitly rejected without opening them.

During the measurement experiment, protected evaluation/improvement rules, backend behavior, prompts and normal activation state were unchanged. This is a code-efficiency experiment, not an RRSI pipeline-quality gain or an adoption decision. Readonly bundles discourage accidents; the implementation does not claim an OS security boundary against malicious same-user concurrent mutation.

## Same-fixture component comparison

Each fixture used 1,000 randomized measured before/after pairs plus 100 warmup pairs, seed 24. Both implementations verified the **same immutable bundle path and contents**. The primary fixture is the earlier measured 30-file bundle. A second synthetic fixture adds 120 registered files. These are warm-cache, in-process component timings, separate from the fresh-process experiment below.

| Fixture | Files | Before p50 ms | After p50 ms | Median reduction | Before p95 ms | After p95 ms | Mean delta ms (simultaneous interval) |
|---|---:|---:|---:|---:|---:|---:|---|
| original-30-files | 30 | 1.259 | 0.820 | 34.9% | 1.615 | 1.052 | -0.460 [-0.478, -0.445] |
| scaled-150-files | 150 | 5.649 | 3.196 | 43.4% | 6.606 | 4.002 | -2.424 [-2.487, -2.348] |

The primary verifier median fell by about **0.44 ms (35%)**. The larger synthetic fixture fell by about **2.45 ms (43%)**. These percentages describe the verifier component, not the entire harness or provider inference. Both paired mean-delta intervals are below zero on these measured fixtures; other filesystems and cold-cache behavior are unmeasured.

## Fresh-process command comparison

Nine workloads used 100 randomized measured pairs and five warmup pairs each: 900 pairs and 1,890 top-level commands. Baseline and candidate had separate before/after stable launchers and isolated active-selection fixtures. This is essential because the verifier lives in the bootstrap; selecting the old bundle through the new launcher would measure the new verifier twice. The benchmark now supports an explicit `--baseline-launcher` for this purpose.

Both arms used identical materials and simulated backends. No tests or native provider inference ran concurrently with collection. An explicitly synthetic approval fixture was used only to exercise real approval loading/dispatch in the private test stores; its dummy setup timings are not measured results or human adoption evidence.

| Workload | Before p50 ms | After p50 ms | Median delta ms | Mean delta ms (simultaneous interval) | Diagnostic assessment |
|---|---:|---:|---:|---|---|
| startup | 35.17 | 34.72 | -0.45 | -0.63 [-1.36, +0.07] | uncertain |
| review | 67.34 | 66.92 | -0.42 | +0.75 [-0.85, +2.57] | uncertain |
| review-trace | 68.90 | 68.41 | -0.49 | -2.19 [-4.41, -0.09] | negligible |
| review-large | 68.25 | 67.88 | -0.37 | -0.36 [-0.98, +0.42] | negligible |
| design | 67.56 | 66.87 | -0.69 | -0.15 [-1.84, +1.61] | uncertain |
| work | 281.98 | 280.61 | -1.36 | -2.86 [-13.79, +8.05] | uncertain |
| review-delay | 127.82 | 127.33 | -0.48 | -0.97 [-7.38, +5.49] | uncertain |
| review-override | 67.88 | 67.24 | -0.64 | -0.52 [-2.17, +1.17] | uncertain |
| review-failure | 88.63 | 88.16 | -0.47 | -0.90 [-4.48, +2.54] | uncertain |

Short successful command medians improved by roughly **0.4–0.7 ms** in this collection. Fresh-process means and tail intervals remain noisy, so a uniform whole-harness speedup is not established. Two workloads met the provisional non-regression budgets and seven were uncertain; none demonstrated a beyond-budget slowdown. The whole-command effects are not a reason to repeat collections until a desired verdict appears.

The diagnostic budgets remained 5 ms absolute and 5% relative, with 100 pairs minimum and 2,000 paired bootstrap resamples, seed 7. Simultaneous bounds cover mean/p95 absolute/relative comparisons. These are not user-approved production SLOs. p99 values remain in the private reports but are descriptive. OS caches, thermal conditions and unrelated host activity were not controlled; these results do not establish live-provider or IDE latency.

The deliberately failing backend workload remains failed in both arms; fast failures do not earn a success. All other executions succeeded. All 200 measured work runs completed exactly one writer and six simulated judge calls. Measurements made zero real provider calls and did not alter normal release activation; the later user-authorized adoption is recorded above.

## Correctness and review

The local prototype suite ran **317 tests: OK, 2 skipped**, including 10 new tests with parameterized path/hash cases. Coverage includes registered nested Unicode/whitespace names; missing and unexpected files; bytecode changes after a prior success; directory and dangling symlinks; FIFO refusal without blocking; canonical/reserved/traversal paths; malformed hashes; enumeration/content-read errors; empty-directory compatibility; and correct before-version launcher selection. Existing isolation, approval, rollback, privacy and call-count tests remain in force.

The actual Ringo review reported no defects. Its performance open question is answered for these local measured fixtures; representative behavior on other storage/cache conditions remains open. That experiment used one native review and no RRSI campaign; subsequent adoption verification is separate from the timing collection.

## Evidence and publication scope

Raw paired samples, warmups, commands, stderr, source snapshots and private fixture states remain outside the repository. The documentation summary and source patch are published; the separate unfinished runtime feature set is not bundled into this documentation update. The patch is against the local pre-optimization prototype, not the current default runtime. It can be inspected now and integrated when that runtime is published.

Do not compare unpaired absolute numbers from the [earlier feature-overhead study](LATENCY-RESULTS.md) to infer this change's effect. That study compared release features with the prior checkout; this study compares the same feature set immediately before and after the verifier optimization. The [optimization-options note](LATENCY-OPTIMIZATION.md) remains the historical component profile and proposal context.

# Local release implementation latency results

100 randomized paired observations per workload, plus five warmup pairs each. Fresh CLI processes on this host; caches/thermal conditions were not controlled at the OS level. Tests and provider inference were not running concurrently with this final collection. This is simulated-backend local overhead, not a live provider or IDE measurement.

The baseline is the exact pre-task dirty checkout captured in a private baseline snapshot. The candidate uses the active dispatch path in an isolated state with an explicitly synthetic approval fixture, not a normal deployment. The synthetic approval observations are only setup for realistic approval-loading cost; every table value below comes from the actual paired wall-clock observations.

Provisional diagnostic budgets, declared before the final collection: 5 milliseconds absolute and 5% relative; 100 pairs minimum, 2,000 paired bootstrap resamples with seed 7, simultaneous bounds across mean/p95 absolute/relative comparisons. These are not user-approved production SLOs.

| Workload | Baseline p50 ms | Candidate p50 ms | p50 delta ms | Baseline p95 ms | Candidate p95 ms | Mean delta ms (simultaneous interval) | Assessment |
|---|---:|---:|---:|---:|---:|---|---|
| startup | 40.55 | 35.00 | -5.55 | 44.85 | 37.74 | -5.68 [-6.16, -5.36] | negligible |
| review | 67.28 | 70.67 | +3.39 | 72.27 | 75.17 | +3.73 [+2.83, +4.68] | uncertain |
| review-trace | 68.98 | 72.38 | +3.40 | 70.83 | 73.89 | +3.31 [+2.58, +3.94] | uncertain |
| review-large | 66.03 | 69.37 | +3.34 | 69.65 | 73.80 | +3.50 [+3.07, +3.91] | uncertain |
| design | 67.89 | 71.26 | +3.37 | 69.91 | 73.17 | +3.15 [+1.92, +4.47] | uncertain |
| work | 314.01 | 317.85 | +3.85 | 345.40 | 351.51 | +4.89 [-0.14, +9.45] | uncertain |
| review-delay | 140.60 | 144.67 | +4.08 | 188.98 | 191.21 | +2.16 [-2.91, +7.36] | uncertain |
| review-override | 67.99 | 71.49 | +3.50 | 69.97 | 77.57 | +3.94 [+2.88, +5.22] | uncertain |
| review-failure | 92.17 | 96.02 | +3.84 | 113.85 | 113.58 | +4.33 [+0.91, +7.34] | uncertain |

Interpretation: startup/help improved; ordinary review/design median overhead is about 3–4 ms, and the work round about 4 ms. The remaining confidence bounds do not consistently establish both provisional budgets, especially p95 tails. The final run shows no workload classified as a demonstrated slowdown beyond those budgets; it also does not justify declaring the whole change negligible. Leave acceptance to the user and do not activate a normal release.

The review-failure workload deliberately exits unsuccessfully in both arms. It remains a failed diagnostic observation, never a successful fast result. All other final workloads succeeded. All 200 measured work runs completed one writer plus six simulated judge calls. There were zero real provider calls in this latency experiment.

The first uncompiled-bundle pilot showed roughly 65 -> 83 ms on a zero-delay review. Staging precompiled, hash-checked bytecode and using direct in-process imports replaced that slower approach. The earlier exploratory matrix ran on an earlier source and its work bridge was rejected before inference; it is preserved privately and not used for this final claim. Final work fixtures were corrected and separately tested.

Raw paired data, exact commands, stderr and complete bootstrap reports remain in the owner's private experiment archive and are not part of this upload. The synthetic approval setup is explicitly labeled and must never be reused as real adoption evidence. This is a summary of an observed local prototype experiment, not an independently reproduced public benchmark.

Validation summary:
```json
{
  "paired_measurements": 900,
  "top_level_executions_including_warmups": 1890,
  "workloads": 9,
  "verdicts": {
    "negligible": 1,
    "uncertain": 8
  },
  "release": "fe736b7789cb1e9b1ba357f3a234116bf8ea8e96387fb8994fc2499a56b268bc",
  "bundle_matches_final_runtime_source": true,
  "work_measured_runs_verified": 200,
  "simulated_calls_per_work_run": 7,
  "unexpected_failures": 0,
  "expected_failure_executions": 210,
  "real_provider_calls_in_latency_experiment": 0,
  "normal_release_state_exists": false
}
```

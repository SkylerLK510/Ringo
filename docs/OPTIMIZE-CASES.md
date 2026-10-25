# Optimization regression cases

Updated 2026-10-25. The optimized release implementation, latency runner and
matching tests are now included in the repository source. The owner has already
adopted the exact measured runtime locally; cloning this repository does not
activate a release on another machine. See [release management](RELEASES.md).

This continuation adds offline regression cases for the latency/performance work.
They exercise observable correctness and accounting. They are not new latency
measurements, model quality scores, or an RRSI improvement campaign.

| Case | Execution | Required result |
|---|---|---|
| OPT-01: content integrity | Fresh CLI against an approved fixture with altered source or checked bytecode | Reject before any backend call or trace creation |
| OPT-02: inventory integrity | Fresh CLI with a missing prompt, unexpected nested Unicode file, dangling symlink, manifest symlink, or FIFO where supported | Reject promptly; retain the active pointer; no unchecked fallback |
| OPT-03: approval integrity | Fresh CLI after approved evidence is altered | Reject before inference |
| OPT-04: no stale success | Successful CLI review followed by source, bytecode or approval mutation, then another fresh CLI | The second invocation rechecks and rejects; backend call count remains one |
| OPT-05: bounded normal path | Ordinary approved local review | Exactly one fixture backend call; exact release, approval, bootstrap and plan identities |
| OPT-06: complete timing schedule | Mocked orchestration of all nine workloads, 30 pairs, two warmup pairs and injected failure/timeout | Retain all 576 scheduled invocations; warmups excluded from measured pairs; failures/timeouts retained and assessed |
| OPT-07: metadata special files | Fresh CLI with a FIFO in place of the pointer, approval or release manifest | Reject within the subprocess deadline before inference or trace creation |

The cases live in `tests/test_optimization_cases.py` as five test methods with
parameterized scenarios. Release cases execute real fresh Python CLI processes
with a local fixture backend. OPT-06 uses a deterministic clock and mocked
process results to inspect the runner's complete schedule; its durations are
synthetic test setup and cannot support a performance claim. Thirty pairs remain
insufficient for the runner's 100-pair diagnostic minimum. A new candidate
failure is still classified as slower rather than hidden by that uncertainty.

```sh
python3 -m unittest tests.test_optimization_cases -q
python3 -m unittest discover -s tests -q
```

The GitHub Actions workflow runs the full offline suite on Python 3.11 and 3.14,
with a separate temporary release store and no provider credentials. It uses the
official [checkout](https://github.com/actions/checkout) and
[Python setup](https://github.com/actions/setup-python) actions. Tests and CI add
no work to normal Ringo invocations. The runtime still hashes complete bundle
contents on every launch; approval receipts and persistent workers remain future
options requiring separate assessment.

The [verifier study](VERIFIER-OPTIMIZATION.md) remains the measured evidence:
2,000 component pairs and 900 CLI pairs. Its whole-command tail uncertainty is
unchanged. The [overfitting audit](OVERFITTING-GUARDRAILS.md) adds evidence-binding and
statistical-gate regressions. The earlier code-review recall validation is still incomplete; no
new validation/test submissions, model comparison or spending campaign is
started by this case continuation.

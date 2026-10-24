# Separate security and CI bot: research and latency requirements

Research checked 2026-10-23. This is a future product brief for a separate bot
that could integrate with Ringo IDE. No scanner, CI gate, IDE hook or additional
runtime model call is installed by the HarBenchmark change.

The user's constraint is **no added interactive latency**. The proposed design
has no synchronous scan or model call on typing, saving, Ringo generation or
review completion. Exact zero total overhead cannot be guaranteed by choosing an
asynchronous API: background work can still compete for CPU, disk, network and
model quota. Treat that distinction as an integration acceptance requirement.

## Separate detection jobs

| Job | Candidate existing capability | What still needs evidence |
|---|---|---|
| Credential leaks | [Gitleaks](https://github.com/gitleaks/gitleaks), [GitHub secret scanning](https://docs.github.com/en/code-security/reference/secret-security/secret-scanning-scope) | Known-pattern coverage, synthetic/invalid tokens, encoded and split strings, safe redaction, history scope and false positives |
| Source vulnerabilities / exfiltration | Language-aware static analysis plus targeted model review | Interprocedural data flows, access control, runtime configuration and whether the issue is reachable; secret matching alone cannot prove these |
| Workflow configuration failures | [actionlint](https://github.com/rhysd/actionlint) checks GitHub Actions workflows | Reusable-workflow contracts, generated files, runner/matrix dependencies and actual provider behavior |
| Workflow security | [zizmor](https://docs.zizmor.sh/audits/) audits GitHub Actions security patterns | Untrusted event/prompt inputs, credential permissions, cache/artifact trust, external action changes and residual blind spots |
| Actual CI breakage | Reproduce selected jobs in disposable runners; inspect existing CI logs asynchronously | Static checking cannot establish runtime success; classify dependency/network outages, flaky tests, packaging and real code regressions |

GitHub's supported secret patterns and validation differ by token type. Measure
coverage against the actual enabled pattern set rather than promising universal
detection. Do not test a suspected real credential against its provider as part
of a benchmark: use purpose-built synthetic credentials and isolated test services.
[GitHub scope documentation](https://docs.github.com/en/code-security/reference/secret-security/secret-scanning-scope).

Zizmor documents that it analyzes workflow/action definitions, with limitations
for code outside those definitions. Its online modes may make GitHub API calls;
configure offline/local collection for a predictable fast lane and reserve
network-dependent analysis for separate work. This is tool capability evidence,
not a claim about scan latency. [Usage and limitations](https://docs.zizmor.sh/usage/).

## Proposed execution boundary

Use an independent service observing repository events and immutable commit
snapshots. The IDE consumes completed, versioned findings. If local observation
is necessary, enqueue outside the interactive request path; never synchronously
wait for acceptance, a scan, provider quota or a result. An unavailable service
means the status remains pending/unknown; do not silently display clean.

Coalesce superseded snapshots, bound queues, cancel stale work and cache by
content **plus** rules/configuration/tool version. A cache keyed only by commit
or source text can reuse an obsolete verdict after a rules update. Diff checks
need dependency/configuration invalidation and periodic full scans; changes in
an unchanged caller can matter. These are proposed requirements, not measured
properties of an implemented service.

Prefer dedicated remote workers and separate concurrency/quota for deep scans
and CI reproductions so they do not contend with Ringo inference. Locally running
a scanner at lower priority may still cause disk or memory pressure; that would
not establish the no-latency requirement. Keep result rendering bounded and
outside critical IDE handlers. Do not silently add pre-commit, pre-push or merge
gates: making users wait for a mandatory result conflicts with the present
requirement. A later request for blocking prevention needs a separate policy
decision; asynchronous detection necessarily reports some findings later.

For future deployment, scanner rules and check policy must come from trusted
configuration outside the change being inspected. Do not let a proposed PR
disable checks or expand ignore rules and then report itself clean. Trivy's
[configuration guidance](https://trivy.dev/docs/v0.76/guide/configuration/) makes
this concern concrete for automatically loaded ignore/secret configuration.

## Latency acceptance experiment

No absolute-zero performance claim is currently supported. Before integration,
preregister paired baseline/bot runs on identical workload snapshots, hardware
and caches. Use randomized alternating order and repeat both warm and cold
conditions. Include large diffs, rapid saves, a disconnected service, provider
throttling, queue overload and concurrent builds.

| Measurement | Required reporting | Acceptance requirement |
|---|---|---|
| Synchronous waits | Instrument typing/save and Ringo request paths | Zero waits on scanner, model or scan-result delivery |
| IDE responsiveness | p50/p95/p99 input-to-render and save duration; baseline deltas and uncertainty | No detected regression at the agreed measurement precision; do not call an underpowered test proof of zero |
| Ringo completion | End-to-end time, inference queue wait and token/call counts | No scanner calls or shared scanner quota on the critical path |
| Resource contention | CPU, memory, disk/network and ordinary CI duration | No measured regression; otherwise isolate further or leave integration disabled |
| Feedback age | Event-to-first-result and event-to-complete-result, queue backlog | Report separately; moving work off-path does not make detection instantaneous |
| Stale results | Commit/content identity on each displayed result | Never attach a verdict to different content; pending is explicit |

The measurement precision, representative workload and practical regression
threshold remain open product decisions. If literal zero wall-clock change is
required, software measurements cannot establish it; keeping the bot separate
and integration disabled is the only currently supported state.

## Benchmarks and missing coverage

[PrimeVul](https://arxiv.org/abs/2403.18624) addresses vulnerability-dataset quality,
duplication and chronological splits. It is not a leaked-credential or CI benchmark.
[CyberSecEval](https://github.com/meta-llama/PurpleLlama/tree/main/CybersecurityBenchmarks)
contains multiple security evaluations, including automated patching; individual
subtasks do not establish universal repository protection.
[CI-Repair-Bench](https://arxiv.org/abs/2604.27148v2) validates repairs by rerunning
original workflows. None of these establishes our future IDE latency behavior.

Build separate labeled sets for credentials, source vulnerabilities, workflow
security and CI breakage. Include clean negatives at realistic prevalence,
patched/vulnerable pairs, fork-PR and reusable-workflow configurations, and
infrastructure failures that should be diagnosed rather than patched away.
Measure severity-weighted recall, false alerts per clean change, localization,
redaction fidelity and full required-job success. A disabled test, skipped matrix
entry or broadened suppression is not a successful fix. Confirm this using a
protected required-job inventory and checks outside candidate control.

Open questions before implementation: repository languages and CI providers;
whether leaks includes proprietary data as well as credentials; local versus
remote trust boundary; permitted source egress; alert destinations; retention;
scanner versions/rules; and acceptable background feedback age. Connected
repository inventories can answer some of these once a concrete project is
selected. No external messages, webhooks or deployment changes are authorized
or made by this research document.

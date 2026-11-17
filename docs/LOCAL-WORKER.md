# Local worker (standalone, bounded)

This is the first deliverable of `OPENCODE-LOCAL-WORKHORSE-HANDOFF.md`: an
independent local inference worker with versioned input/output, a real deadline,
bounded checks and a local-only routing policy. It has no dependency on the
shell, the task/evidence gateway, `harness.py`, `backends.py` or the provider
registry. No candidate is written into the owner's working tree.

Status: **implementation only**. No model was installed, no Ollama runtime was
started and no live benchmark or model comparison was performed. The reported
cancellation proof comes from a deliberately hung loopback HTTP request that
observes the server-side disconnect; it is not a measurement of a live Ollama
build. Exact tokenizer accounting is not available to this standalone module,
so input size is checked with a one-token-per-byte upper bound (true for any
tokenizer) and an oversized assignment is rejected rather than truncated. The
configured model digest is verified against Ollama's local `/api/tags` before
inference; a missing or mismatched digest fails closed.

## Layout

| File | Purpose |
| --- | --- |
| `local_worker/schemas.py` | Versioned assignment/result contracts, canonical digests, strict JSON, path safety |
| `local_worker/config.py` | Loopback-only execution config and model pinning |
| `local_worker/candidate.py` | Strict model-output parsing; rejects malformed, stale, binary and out-of-scope edits |
| `local_worker/control.py` | Monotonic deadline, cancellation flag, owned-socket shutdown guard |
| `local_worker/adapters.py` | Ollama `/api/generate` NDJSON adapter; explicit refusal of cloud/auto |
| `local_worker/prompts.py` | Bounded prompt assembly and context-budget rejection |
| `local_worker/checks.py` | Host-configured argv checks, capped private output, process-group cleanup |
| `local_worker/workspace.py` | Detached git worktree (or slice-only snapshot for fixtures) and candidate application |
| `local_worker/ledger.py` | Durable operation records outside the checkout; conflict and restart handling |
| `local_worker/runner.py` | Bounded two-attempt orchestration and result assembly |
| `local_worker/cli.py` | `validate`, `run`, `reconcile` |

## Live execution blocker

`run` refuses live Ollama generation until server-side cancellation has been
verified against the actual supported runtime. The mock HTTP tests prove client
socket cleanup only. There is no configuration switch that certifies a runtime;
adding a real certification workflow is a separate deliverable. `validate`,
offline adapters and deterministic checks remain available. Process-tree cleanup
for checks currently requires POSIX; other platforms fail closed.

## Boundaries

- At most **two** local inference calls: one candidate, one repair with bounded
  check/validation feedback. A malformed candidate is **not** silently repaired
  by a hidden parsing call. A timeout consumes its attempt.
- All configured checks run as **one suite per parsed candidate**, at most
  **two** suites per job (one per attempt). A candidate is ready only when every
  check in its suite passes. Check argv comes from the assignment, never from
  the model; no shell is used. Raw output goes to a private diagnostics
  directory, never into the result.
- One 180-second wall deadline covers inference and checks, with a short
  separately bounded cleanup allowance. The HTTP adapter shuts down only the
  socket it owns when the deadline fires; it never kills a shared daemon.
- `local` is the only implemented policy. `cloud` and `auto` are recognized and
  refused explicitly, so a local job cannot silently send material remotely.
- The operation ledger and all run artifacts must live outside the repository
  checkout. The CLI rejects ledger/result paths inside the checkout.

## Assignment and result

Both are strict version-1 JSON objects. Unknown fields, duplicate JSON keys and
NaN/Infinity are rejected. The assignment carries: IDs, objective, owner
constraints, base revision, allowed paths, identity-bound text slices and
host-configured checks. It never carries a raw conversation transcript.

The result carries: terminal status, phase, worker identity with digest
verification, wall deadline and elapsed time, per-attempt operation
IDs/fingerprints/usage,
per-check status and output digest, changed paths, candidate digest and a
candidate artifact reference, uncertainty flags, errors and cleanup summary.
A `candidate_ready` result still needs a person or the later gateway to accept
it; it is an attributed local claim, not verified evidence or owner instruction.

## Configuration example

No command below installs anything. They are the exact commands for a verified
setup; setup itself requires separate owner authorization.

```json
{
  "schema_version": 1,
  "policy": "local",
  "endpoint": "http://127.0.0.1:11434",
  "model": "qwen3.5:4b-q4_k_m",
  "model_digest": "sha256:<exact digest from ollama list / GET /api/tags>",
  "runtime_version": "<verified ollama version>",
  "quantization": "<verified quantization>",
  "context_tokens": 4096,
  "max_output_tokens": 1024,
  "job_deadline_s": 180,
  "cleanup_allowance_s": 10,
  "attempt_deadline_s": 120
}
```

## Commands

```bash
python3 -m local_worker validate --assignment ASSIGNMENT.json --config CONFIG.json
python3 -m local_worker run \
  --assignment ASSIGNMENT.json --config CONFIG.json \
  --ledger-root PRIVATE_DIR --result-dir PRIVATE_DIR \
  --repo /path/to/repository --request-id JOB_ID
python3 -m local_worker reconcile --ledger-root PRIVATE_DIR
```

`validate` performs no inference. `run` prints one compact JSON line with the
terminal status, result path, call/check counts and usage; exit 0 means
`candidate_ready`, 4 means another terminal status, 2 means bad input/config.
An interrupted `running` operation is never retried automatically: run
`reconcile`, which marks it `unknown` and requires a person to resolve it.

## Tests and remaining work

```bash
python3 -m unittest discover -s tests -q
```

The fake-runtime tests pin the exact two-call/two-suite ceiling, deadline
behavior, no-cloud-call rule, cancellation cleanup, malformed output bounds,
request-ID conflict handling and the hung-request server-side stop. Live
smoke, the 12-task development comparison and opt-in gateway integration are
later deliverables and are not authorized by this implementation.

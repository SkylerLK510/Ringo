# Trusted live turn-policy pilot

`turnrun_live.py` connects the campaign API to actual `Session` transitions and the
isolated native subscription broker. Preparing a pilot, certifying its graders and
running preflight make no inference calls. Only `run` launches live work. It refuses
`RINGO_CHILD`; there is no flag to remove that restriction.

The existing `turnrun_campaign.py` CLI remains an offline demonstration and sizing
interface. Its static blocker list is not the live adapter's preflight result.

## Execution boundary

The trusted host runs the pinned `scenariorun` binary with `--broker-socket`, mutually
exclusive with the offline `--program`. This is an explicit evaluation transport,
not a provider executable renamed to evade the offline runner's checks. Claude and
the pinned [Codex subscription profile](CODEX-RRSI-BROKER.md) are supported.
Normal shell startup never enables this transport.

Session owns instruction policy, handoff thresholds, restart/clear, continuation
IDs and event accounting. A client subprocess sends only the resulting text and
an observed continuation ID over a private Unix socket. The controller fixes the
model, tool, images, worker, token ceiling and absolute deadline. The provider has
no access to this socket, the host project, sealed scenario files or protected
checks. Its sole tool executes inside the separate task worker.

A scenario keeps one native broker container across serial turns. Native resume
uses a transcript in that container's quota-backed private temporary filesystem;
it is destroyed with the broker. Single-attempt process certification still uses
no session persistence. Unknown continuation IDs are refused before inference.
Each native turn reconnects MCP without resetting the worker's call limit, failure
state or lease. Clear and handoff start fresh native conversations in the same
trial; restart keeps the actual Session continuation identity.

The scenario has one token reservation shared across all native turns. The next
turn receives only the remaining allowance. Every native attempt is journaled
before launch; failed or incomplete attempts never retry. The complete native
aggregate (input, cache creation, cache reads, output) is the accounting authority.
Session's independently parsed total must agree before grading. An overshoot is
retained at its reported value and halts the campaign; an unknown count keeps at
least the evaluation reservation. The cap is admission accounting, not an exact
provider-side stop. Provider HTTP request counts remain unknown.

Workers stop before export. The controller replaces `.ringo-evidence/session.json`
in the exported submission with its trusted Session record. A separate networkless
checker receives that submission and the protected check. No candidate code or
check executes on the host. Check exit 0 means pass, 1 treatment failure, and any
other exit/timeout/truncated output fails the protected gate and stops advancement.

Proposals use a separate empty worker/broker. Their context contains the incumbent,
covered policy fields and a projection of development scores, costs, hypotheses and
policy decisions. It excludes raw provider replies, paths, check/scenario contents
and validation/holdout outcomes. Invalid JSON proposals are charged and screened;
there is no repair call. Search retains the campaign's two-sibling/two-round limit.

## Private configuration

Build the evaluation binary with:

```sh
cargo build --manifest-path cli/Cargo.toml --locked --features turnrun --bin scenariorun
```

A private version-1 config contains exactly:

- `version: 1`, `plan`: a validated campaign plan (see RRSI-CAMPAIGN.md).
- `image`, `worker_image`: immutable native and grading image IDs/digests.
- `model`: the full pinned Claude model name.
- `binary`: an absolute path to the built scenario binary.
- `tasks`: the same task IDs as the frozen plan. Each maps to absolute private
  `fixture`, `scenario`, `check`, `positive` and `negative` paths. The fixture and
  grader examples contain only ordinary files/directories. Both grader examples
  must include the evidence files that the check expects.

For Codex, use version 2 with the additional `profile: codex-luna-low`,
`model: gpt-6-luna`, a matching Codex image and a fresh Codex process proof.
Codex scenarios set `agent: codex` and support turns, restart, clear, fresh starts
and matching-agent seeds. Compact remains Claude-only.

Claude scenarios use the existing actions. For seeded context-size
experiments, `seed.threads` may contain `[["claude", "$current"]]`: the runner resolves
it to the current observed native session. Literal invented IDs are refused.
Context sizes and records remain evaluator-supplied and must be described as
seeded observations, rather than measured native context sizes. `/compact` is
sent exactly, without policy text; it must satisfy the same pinned-model and
complete-usage gates. Offline control-command tests do not establish live compact
compatibility. The initial private pilot uses seeded handoff/restart, not compact.

For an explicit fresh conversation after restart, use `turn`, `restart`, `start_over`, `turn`.
This invokes the normal Session record builder and sends no native resume ID on the last turn;
it does not alter ordinary restart behavior. Inspect actual broker requests with deterministic
stand-ins before authorizing controls: verify the earlier user contract, exact latest message,
fresh conversation and selected handoff instruction at the intended turn. Raw prompt hashes can
differ just because private record paths differ and cannot establish treatment delivery.

Such a panel can cover `handoff_instruction`; forcing a fresh start cannot establish sensitivity
to `handoff_at`. Keep the newest message from repeating the earlier contract, and keep decisive
requirements in the inline record: an isolated worker cannot read a host record-file pointer.
Route checks establish delivery only. Baseline competence and treatment sensitivity still require
the live controls gate. An all-pass baseline and degraded arm does not authorize search.

```sh
python3 turnrun_live.py prepare --config PRIVATE_CONFIG.json --work NEW_PRIVATE_DIR
python3 turnrun_live.py certify-panel --work PRIVATE_DIR
```

Preparation freezes all execution sources, the runner binary, scenarios, fixtures,
checks and both discrimination examples. It reports the full token schedule and
separately bounds native process attempts/model turns. Every subsequent phase
rechecks these pins. An existing prepared directory or grading evidence is never
overwritten. Source/input changes require fresh preparation, not editing the pins.

Grader certification must fail at each negative fixture and pass at its positive
fixture inside the exact grading image. This establishes grader discrimination,
not baseline competence, live policy sensitivity or generalization.

Before running, the trusted controller needs three private prerequisite records:

1. `grading.json`, produced by `certify-panel` for this manifest.
2. `panel-review.json`, with `manifest_sha256`, a named `reviewer`, and true
   `fresh_sealed`, `family_disjoint`, `policy_coverage` assessments. A hash alone
   cannot establish those properties. Preserve the review's evidence and limits.
3. `process-proof.json`, the actual successful native certification report for
   this exact image, worker image, model and current native/bridge/broker/isolation
   source hashes. The earlier process proof is historical evidence; changing the
   process profile does not transfer its certification to a new image/source.

The separate authorization file must contain `manifest_sha256`, `owner_approved:
true`, the actual `owner_statement`, and `phase_budgets` mapping each authorized
phase to its exact ceiling in the frozen plan. Preparation never writes approval.
Approving controls does not authorize search or confirmation. Confirmation also
requires the separate `manual` allocation because it evaluates that arm first.

```sh
python3 turnrun_live.py preflight --work PRIVATE_DIR --authorization PRIVATE_APPROVAL.json --phase controls
# Standalone only, after the numeric authorization above:
python3 turnrun_live.py run --work PRIVATE_DIR --authorization PRIVATE_APPROVAL.json \
  --phase controls --token-file PRIVATE_OAUTH_FILE
```

A passing sensitivity control is required before `--phase search`. Confirmation
remains closed until search completes; failed validation blocks holdout. No phase
automatically adopts a policy, merges code or changes release certification.

## Offline acceptance

CI exercises actual native CLI resume/restart/clear through the real Session,
worker and protected checker against a local synthetic HTTP provider. The replay
uses the same controller loop as live execution, reads no real credential and
unconditionally rejects upstream tunnel opens. Its usage is synthetic. Separate
Session tests exercise threshold changes, handoff instruction, exact new-message
bytes and compact command preservation, as well as budget and failure behavior.

Set `RINGO_TEST_IMAGE`, `RINGO_NATIVE_IMAGE`, and `RINGO_SCENARIO_BINARY` to pinned
local artifacts to include these tests:

```sh
python3 -m unittest tests.test_turnrun_live tests.test_turnrun_native \
  tests.test_turnrun_campaign tests.test_turnrun_isolation \
  tests.test_turnrun_broker tests.test_turnrun_certify -v
```

This acceptance establishes transport/integration behavior. It supplies no live
baseline, improvement measurement, paper replication or campaign authorization.

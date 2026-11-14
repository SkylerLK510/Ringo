# Native subscription process adapter

`turnrun_native.py` supplies the native process boundary missing from the offline
campaign implementation. It is separate from the task worker and protected checker.
The native CLI runs as UID 65532 in a disposable, pinned Docker image with a read-only
root, no network interface beyond loopback, bounded memory/storage/processes and an
independent parent-death/lifetime watchdog. It has no owner home, task fixture,
inventory, checker, other trial, host credential directory or Docker socket.

The immutable container bridge relays only MCP JSON-RPC and bounded TLS tunnel chunks
over `docker exec` stdio. The trusted controller fixes the only upstream authority to
`api.anthropic.com:443` and refuses non-public DNS destinations. The provider verifies
TLS normally; the controller does not terminate TLS. The fixed MCP endpoint still
owns task authority and rejects every tool except the assigned worker. Neither
provider traffic nor credentials are relayed into the worker or checker.

The CLI gets a fresh home/configuration directory, no built-in tools, no slash
commands, no Chrome integration, strict MCP selection, disabled hooks/plugins,
explicit model selection and disabled retries. No owner settings are inherited.
Safe mode is not used because it also removes the required MCP tool. The adapter
does not use bare mode, API keys, model fallbacks or a billing-provider switch.

## Build and probe without inference

Build contexts and reports must be outside the checkout. The base must already have
Python 3 and the ordinary worker dependencies. Pin its image ID or repository digest.
The builder fetches a numeric Claude CLI release from Anthropic's download service,
verifies its publisher manifest checksum and size, and records the exact image,
binary and bridge hashes. Only the binary and bridge enter the image build context.

```sh
python3 turnrun_native_image.py --version 2.1.153 --platform linux-arm64 \
  --base sha256:PINNED_PYTHON_IMAGE --work PRIVATE_NEW_BUILD_DIR
python3 turnrun_native.py probe --image sha256:BUILT_IMAGE \
  --report PRIVATE_NEW_REPORT.json
```

Use `linux-x64` on an x86-64 Docker host. The probe checks native startup, UID,
blocked direct networking, absent owner/Docker/managed-settings paths, empty
workspace and equality of the image bridge hash with the selected source. It makes
zero provider inference calls and always reports `live_certified: false`.

```sh
RINGO_TEST_IMAGE=sha256:PINNED_GRADER_IMAGE RINGO_NATIVE_IMAGE=sha256:BUILT_IMAGE \
  python3 -m unittest tests.test_turnrun_native tests.test_turnrun_isolation \
  tests.test_turnrun_certify tests.test_turnrun_broker tests.test_turnrun_campaign -v
```

The native tests include both a deterministic bridge exercise and the **actual native
CLI** against a local Anthropic-protocol replay inside its network-disabled container.
The replay verifies tool discovery, native MCP execution, a worker artifact and
aggregate usage parsing. Its credentials and observations are synthetic. It makes
zero real inference calls and does not establish subscription authentication,
model availability, native usage accuracy or provider connectivity.

## One standalone subscription proof

Live execution refuses `RINGO_CHILD` before reading a credential or starting a
provider. Run the following only from a standalone controller session after numeric
authorization. Do not clear the child flag inside a child session.

Provision a **dedicated native subscription OAuth token** using the native CLI's
interactive `claude setup-token` workflow. Save the resulting token in a private
ordinary file outside repositories with mode 0600. Do not put it in an argument,
report, repository, worker, image or chat. The adapter does not export the owner's
existing keychain login. The token is passed to the provider over its private stdin
channel and environment; Docker argv and inspection metadata contain no token.

```sh
python3 turnrun_native.py certify --image sha256:BUILT_IMAGE \
  --worker-image sha256:PINNED_GRADER_IMAGE --model FULL_APPROVED_CLAUDE_MODEL \
  --token-file PRIVATE_OAUTH_FILE --budget-tokens APPROVED_CAP --seconds 120 \
  --work PRIVATE_NEW_CERTIFICATION_DIR
```

This performs exactly one provider-process attempt, with at most four agentic turns.
One process attempt is not an exact count of upstream requests. The CLI is asked not
to retry; `provider_requests` remains unknown. The command reserves its full admission
cap before launch, retains it on unknown usage, records all four aggregate usage
categories including cache reads, and halts on an overshoot or failed delivery.
The token ceiling is an admission reservation; the provider may overshoot it.
The wall timer stops the entire provider container, including detached processes.
An existing evidence directory is refused; there is no automatic retry.

The proof requires the pinned model and exact single-tool initialization, connected
MCP status, complete usage, successful completion, and a fresh nonce written by the
assigned worker. The result concerns that process profile only. It is not evidence
of recursive improvement or approval for a campaign.

## Campaign work still required

The campaign remains fail-closed. A native process proof does not by itself supply
the trusted evaluator/proposer implementations, live Session scenario execution,
fresh sealed scenarios, fix-family-disjoint splits or image-specific sensitivity.
In particular, do not treat this direct native CLI proof as a test of Session
handoff thresholds or restart behavior. Those remain on the real Session path.

Once a real frozen plan exists, size every phase without inference:

```sh
python3 turnrun_campaign.py budget --plan PRIVATE_PLAN.json
```

This includes both sibling proposals, worst-case paired incumbent/candidate runs,
the manual arm, separate validation/holdout evaluations and each phase's stop margin.
Search and manual receive equal admission ceilings. The output is a sizing report,
not authorization. Historical public graders remain development evidence; no synthetic
or historical panel should be relabeled as fresh sealed evidence.

Primary CLI references: [CLI flags](https://code.claude.com/docs/en/cli-reference),
[subscription OAuth environment](https://code.claude.com/docs/en/env-vars),
[settings](https://code.claude.com/docs/en/settings-reference).

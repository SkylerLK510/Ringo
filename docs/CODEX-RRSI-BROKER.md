# Codex subscription controls

The `codex-luna-low` live profile runs `gpt-6-luna` with low reasoning effort
through native Codex ChatGPT authentication. Changing the model requires a fresh
process proof and new frozen campaign inputs; a Claude proof cannot certify Codex.

The provider container has no task files, owner home, Docker socket, API key,
refresh token or host configuration. The controller reads a private native
ChatGPT login and pipes only its access token and account identifier into the
disposable process. Only TLS to `chatgpt.com:443` passes through the controller.
Expired credentials stop the attempt; there is no refresh or API billing fallback.

Codex app-server exposes one dynamic `worker` tool. Native shell, file editing,
web search, delegation, plugins and skills are disabled. A pinned publisher
model catalog retains model instructions while restricting built-in tools.
The actual tool schema is checked by offline native protocol replay. The worker
runs in a separate isolated container; grading checks remain controller-only.

Production Session routes Codex turns, restart handoffs and resumed threads
through the broker. Native cumulative thread usage is differenced per turn,
including cached input exactly once. Missing, interrupted or inconsistent usage
retains the reservation and stops the campaign. A usage update must follow the
final assistant item and precede completion; otherwise observed counts remain
a lower bound. Protocol variants without that ordering are refused. Native HTTP request counts remain
unknown; reported usage updates and wall limits are admission checks, not a hard
provider-side token or request cap. A single turn can overshoot its reservation. The built-in live provider may
retry internally; offline replay disables retries, but does not certify live
HTTP request counts.

Build with `turnrun_codex_image.py` using publisher archive and catalog paths,
their SHA-256 checksums, an immutable base image and a private build directory.
CI pins Codex 0.169.0 and exercises its real binary against a local Responses
fixture, with no credentials or upstream inference. Native replay covers worker
execution, resumed usage, restart handoff delivery and Session accounting.
These tests establish transport behavior, not model competence or live access.

Version 2 live configurations pin `profile: codex-luna-low`, the model, immutable
images, controller sources, runner binary and private task inputs. Live process
certification uses `turnrun_native.certify(..., profile='codex-luna-low')`.
Only a matching successful proof unlocks controls. Approval remains bound to the
manifest and numeric phase budget. Live execution requires a standalone session
outside Ringo; never remove `RINGO_CHILD` to defeat that guard.

Run controls before search. An insensitive baseline/degraded comparison does not
authorize search or demonstrate recursive improvement. A small author-known
programming panel is exploratory evidence, not a replication of a paper.

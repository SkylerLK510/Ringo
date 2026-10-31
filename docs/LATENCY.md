# Where a shell answer's time goes

Measured 2026-10-30 on macOS (arm64), Codex CLI 0.165, `codex-chatgpt` backend (GPT-6.1-Sol at its default effort,
low). A plain line in the shell is one tool-free call through the design pipeline with `plans/ask.json`. The timings
come from a pass-through `codex` on PATH that stamps each `--json` event. Calls ran one at a time, 12 per variant
over 6 questions from a one-line fact to a long design answer. The samples are small and the network decides the
tails, so read the numbers as sizes, not guarantees.

## One ask, phase by phase (medians)

| phase | median | slowest seen | what happens |
|---|---|---|---|
| Ringo setup | 0.1 s | | private Codex home, login copy, lean catalog |
| catalog step (`codex debug models`) | 0.47 s | 5.0 s | Codex fetches its model catalog |
| Codex start to turn start | 0.42 s | 5.5 s | account and model lookups, websocket connect |
| model | 4.5 s | | about 2.2-2.9 s to the first token for a short answer, then about 25 tokens/s |
| Codex exit | 0.35 s | 0.65 s | analytics flush; 0.015 s with `analytics.enabled=false` |
| shell notices the end | up to 0.3 s | | trace poll; now 50 ms |

About 1.4 s of a 5.9 s ask is not the model. Ringo's own code accounts for about 0.1 s of it; the rest is Codex
starting and stopping once per call.

**The catalog step.** The private home starts with a copy of the owner's `models_cache.json`. When another Codex
version wrote that copy (here a 0.166 build, against the 0.165 CLI on PATH), the CLI rejects it and fetches the
whole catalog again (about 465 KB), on every call. With a cache written by the same version the step takes 15 ms,
but Codex refetched a 12-minute-old one (a same-minute one was used), so keeping a cache only helps calls made close
together.

## What was tried

| change | measured | outcome |
|---|---|---|
| `-c analytics.enabled=false` on lean calls | exit 0.35 s to 0.015 s in 6 of 6 paired asks; usage still reported | adopted |
| shell trace poll 300 ms to 50 ms | up to 0.25 s sooner, 0.125 s on average | adopted |
| one long-lived `codex app-server` per session, streaming | a new thread in ~10 ms and its turn in ~2 ms on a warm process; first text at 2.2-2.9 s (short and medium answers) and 4.2-5.7 s (a long design answer) instead of when the answer is complete (3-25 s) | not built: the next step, with the reply shown as it streams |
| Fast service tier (`service_tier="fast"`) | model time 28% lower (median), 30-44% lower on the long answer | not adopted: Codex bills it at 2.5x the usage of a standard call; a candidate opt-in |
| GPT-6-Luna (smaller model), effort low | model time about half | rejected: a blind judge preferred the default model's answer in 12 of 12 pairs, and Luna's answers had factual errors in 3 |
| effort high | short asks about the same (the model mostly skips reasoning); the long design answer 20-23 s against 15-20 s, with about 100 reasoning tokens | the default stays (low) |
| `--disable shell_snapshot` | no difference on lean calls | dropped |

Agent turns (`@codex`, `@claude`) use the owner's own CLI settings, so a high reasoning effort or MCP servers
configured there slow every turn the shell starts. They were not measured here: that would run against the owner's
real configuration.

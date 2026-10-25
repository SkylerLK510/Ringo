# Ringo

A local harness for code review, design proposals, bounded council-guided
artifact work, and guarded improvement of its own prompts and pipelines. Ringo
runs model CLIs and assembles their results; it uses frozen provider models and
does not train model weights.

This repository owns the harness, its model calls and their budget. The original
[design council](https://github.com/SkylerLK510/DukChulBOT) owns proposal judging,
rubric scoring, decision gates and journals. A future GUI will be a separate
client; no GUI is implemented here.

## Run

Python 3.11+ and the standard library. From this checkout:

```sh
python3 ringo.py --help
python3 ringo.py review --repo ../project --base BASE_SHA --head HEAD_SHA --pr-text pr.txt --backend codex-chatgpt --timeout 180
python3 ringo.py review --input change.txt --backend codex-chatgpt --timeout 180
python3 ringo.py design --input brief.md --out proposal.md
python3 ringo.py work --spec task.json --out /ABSOLUTE/PATH/TO/new-run
python3 -m unittest discover -s tests -q
```

For reviews, prefer `--repo PATH --base SHA --head SHA [--pr-text FILE]`: Ringo
builds the material in code from git objects (read-only, offline; an incomplete
partial clone is refused): the change description, the three-dot diff, the complete
changed files and README at head, then the unchanged definitions, subclasses,
callers and tests of the changed symbols, within a byte budget
(`--context-budget`, default 400000) that never drops the change itself, and a
manifest of what was added and left out. The material is built in code before the
call; the call is tool-free only on `claude`, `codex-chatgpt` and `ollama:` (the `codex` preset keeps a read-only
shell, and a custom command's tools are unknown). `--input FILE` instead takes the material as one file. For designs, supply the decision brief and hard
constraints. Review/design default to one strong model call; a call that fails or
replies in the wrong shape is tried once more, so at most two. Custom plans are JSON
files under `plans/`, with instructions under `prompts/`; a plan makes one call per
pass (one per batch of findings for a batched checker, at most 50 findings per
reader reply), each retried at most once,
plus at most one checker repair. `work` makes one writer call and one judge call per
council sample each round: at most `max_rounds x (1 + samples)`. There is no call
cap: the plan or the round count decides the calls, and `--max-cost` and timeouts
bound money and time.

`work` revises a declared set of text files: a writer model returns complete
files or exact search/replace edits, which Ringo turns into complete files; fixed
executable checks run on them, and a separate council assessment decides whether
to revise or stop. A result that passes every check and is recommended is written
to the run's `final/` folder for human review; nothing is applied, adopted or
published. The task names the council bridge and rubric explicitly (the council's
`rubrics/report.json` for documents and reports, `artifact_design.json` for visual
artifacts). Each round is judged with 6 samples by default; 2 is for smoke tests
only. See [the council workflow guide](docs/COUNCIL-WORKFLOW.md) for the task
format, outputs, budgets and trust limits.

`review --findings-json FILE` also writes the findings as
[ReviewBench](https://github.com/review-bench/ReviewBench) agent-contract JSON,
one entry per finding and file. With `--repo`, files are checked against head
and lines clamped. [adapters/reviewbench](adapters/reviewbench/README.md) runs
Ringo as a ReviewBench reviewer image, so runs can be reproduced and scored with
the benchmark's own judge. No ReviewBench result is claimed here.

Backends include Claude Code, Codex, local Ollama and commands that read stdin.
Every Codex backend (`codex`, `codex-chatgpt` and `codex:<command>`) runs with a
private, per-call `CODEX_HOME` holding only a copy of your `auth.json` and an empty
`HOME`, so your global Codex instructions (`~/.codex/AGENTS.md`), skills
(`~/.codex/skills`, `~/.agents/skills`) and memories do not load in the child. A
login the child refreshes is written back to your `auth.json` only if that file
did not change during the call. Ringo parses the CLI's JSON events and records
reported tokens; a turn that uses a tool its backend does not declare is rejected.
`codex-chatgpt` also forces native ChatGPT authentication, strips API-key and
endpoint overrides and disables the shell. The `codex` preset keeps its shell and
the caller's configured authentication. `codex:<command>` runs your own Codex
command line, which must pass `--json`; it may use web search only if that command
line passes `--search`. Codex dollar cost remains unknown.
`--checker-backend SPEC` runs a plan's verdicts pass and its repair on a second
backend, so another model or provider checks the findings; the plan needs a
verdicts pass, and the trace records which backend made each call.
Authenticate with the provider's supported CLI. Usage consumes the selected account's
allowance or API billing according to authentication and plan settings. Unknown
cost is not zero. `--trace` uses a new folder outside this checkout; it contains
private material. Opening the CLI help or running tests makes no provider calls.

Routine usage reports use [token counts](docs/TOKEN-USAGE.md): uncached input,
cached input, cache writes, output and their reported total, with missing usage
identified explicitly. Dollar estimates are omitted unless requested or needed
for an explicit spending limit; existing budget and evaluation gates remain.

## Evaluation and improvement

See [the harness guide](docs/HARNESS.md) and [the agent skill](skills/ringo-harness/SKILL.md).

[Approved releases and latency checks](docs/RELEASES.md) describes local versioned
bundles, explicit activation/rollback, pinned runs and a separate offline latency
gate. `python3 ringo.py release --help` manages releases; staging does not activate
them. RRSI candidate adoption still requires existing sealed-test evidence and a
human decision. Timing experiments use simulated backends and make no provider calls.

[Optimization regression cases](docs/OPTIMIZE-CASES.md) check release integrity before inference,
fresh-invocation rechecks and complete timing schedules. The full offline suite
also runs in GitHub Actions on Python 3.11 and 3.14. The
[overfitting audit](docs/OVERFITTING-GUARDRAILS.md) maps evidence and held-out-data
guardrails to their tests and remaining limits.

[HarBenchmark](HarBenchmark.md) provides twelve public development cases and an
offline scorer across review, constrained repairs, design calculations, artifact
consistency, research arithmetic and evaluation/accounting. Its comparison tables
separate sourced external results from local development evidence and unmeasured
coverage. See the [benchmark gap research](docs/BENCHMARK-GAPS.md) and the
[separate security/CI bot brief](docs/SECURITY-CI-BOT-RESEARCH.md). These additions
make no provider calls and add no work to the normal harness execution path.

```sh
python3 evaluate.py --help
python3 improve.py --help
```

Evaluation freezes inputs, separates repository groups, grades blind, records an
integrity ledger, and controls validation/test reuse. Improvement campaigns search
review prompts/plans against dev evidence. A finalist requires a sealed-test pass
and human adoption. These gates stay advisory; no automatic adoption occurs.
`work` repairs one task's files and does not change Ringo's prompts or plans.
A dollar-budgeted run requires an explicitly declared fallback for unpriced attempts.
The final admitted call can exceed a remaining budget.

Backend calls start in empty directories, which are not filesystem boundaries.
Codex and custom commands may retain file-access tools; use an external boundary
when answer keys must be inaccessible. Evaluation locking currently uses POSIX
`fcntl`.

## Research credit

**Research credit.** We credit **Peng Xia and coauthors at Google Cloud AI Research**, with collaborators at UNC-Chapel Hill, Stanford University and Washington University in St. Louis, for *RRSI: Regularized Recursive Self-Improvement of Agent Harnesses* (2026). Their work inspired this portion of Ringo's improvement loop. Ringo adapts selected ideas from the paper. See the [original paper](https://arxiv.org/abs/2609.24972) and the [authors' project page](https://regularized-rsi.com/).

The proposal loop also draws on [Meta-Harness](https://arxiv.org/abs/2603.28052).
Ringo implements selected ideas; it is not a full reproduction of either paper.
See the guide for the implemented limits and required confirmation.

## Repository separation

The runtime has no import or hardcoded path dependency on the council checkout.
`backends.py` contains the minimal extracted subprocess, command-preset and JSON
parsing helpers. Council judging remains in its own repository and is not bundled
into this CLI. `work` requires an explicitly supplied external council bridge
command and rubric: the council scores the replies it is given and makes no model
calls, while Ringo makes the writer and judge calls and owns their budget.
Existing evaluation stores must be re-frozen for this source layout; do not reuse
old digests or copy sealed data into this repository.

A publication license has not yet been selected. No performance or quality gain
is claimed from this repository extraction.

## Use from an agent

The [Ringo workflow skill](skills/ringo-harness/SKILL.md) connects ordinary coding
and design work to actual bounded harness calls. Link that folder into the agent's
user skills directory and set its local instructions to use it for substantive
engineering work. For Codex, the user skill location is `~/.agents/skills/`; for
Claude Code it is `~/.claude/skills/`. Use `RINGO_HOME` to identify this checkout
when needed. The CLI remains explicitly invoked; installing a skill is guidance,
not a background daemon. Start a new agent session after changing global instructions.

## Intended research use

The target includes intensive ML and finance research. See the
[research requirements](docs/RESEARCH-REQUIREMENTS.md) for evidence, reproducibility,
numerical testing and domain validation goals. The selected direction uses Design
Council for structured assessment, with Ringo providing execution and validation.
Basic integration exists: `work` combines generated files, fixed checks and council
assessment. The research workflow those requirements describe is future work, and
the current harness has not been validated for that workload.

# Pipeline reference

[Documentation](README.md) · [Getting started](GETTING-STARTED.md) · [Project home](../README.md)

Run the examples below from the Ringo checkout. They describe commands, work specs,
backends, accounting and evaluation tools. For interactive use, start with the
[shell user guide](USER-GUIDE.md).

## Commands

| Command | What it does |
| --- | --- |
| `review` | Reviews a code change and lists its findings |
| `design` | Writes a design proposal from a brief |
| `work` | Writes or revises a set of files until fixed checks pass, then gets one review and makes one revision |
| `research` | Answers a brief from web sources that Ringo checks itself |
| `wait DIR` | Waits for a background run to finish and prints its state |
| `release` | Manages approved, versioned releases of Ringo itself |
| `tools` | Serves one read-only task/evidence JSON request on stdin (see [shared tasks and evidence](SHARED-TASK-TOOLS.md)) |
| `ringo` (the shell) | A full-screen session for working with several agents and running Ringo's pipelines |

Run the commands with `python3 ringo.py COMMAND`, or with the `ringo` binary (`cargo install --locked --path cli`), which
takes the same commands. `review`, `design` and `work` can run natively in the binary; `research` runs through the
Python runtime from either.

```sh
python3 ringo.py --help
python3 ringo.py review --repo ../project --base BASE_SHA --head HEAD_SHA --pr-text pr.txt --backend codex-chatgpt --timeout 180
python3 ringo.py review --input change.txt --backend codex-chatgpt --timeout 180
python3 ringo.py design --input brief.md --out proposal.md
python3 ringo.py work --spec task.json --out /ABSOLUTE/PATH/TO/new-run
python3 ringo.py research --input brief.md --trace /ABSOLUTE/PATH/TO/new-trace --out /ABSOLUTE/PATH/TO/report.md
python3 -m unittest discover -s tests -q
```

## Using Ringo

### Review a change

Prefer `--repo PATH --base SHA --head SHA [--pr-text FILE]`. Ringo then builds the review material itself, in code,
from git objects. This is read-only and offline, and an incomplete partial clone is refused. The material contains:

1. the change description and the three-dot diff
2. the complete changed files, and the README at head
3. the unchanged definitions, subclasses, callers and tests of the changed symbols
4. a manifest of what was added and what was left out

Other options:

- **Size:** the material fits a byte budget (`--context-budget`, default 400000). The budget only drops context,
  never the change itself.
- **Changed files:** `--context-mode auto|full|windowed` (default `auto`). `full` sends every changed file whole;
  `windowed` sends a changed file over 16000 bytes as line windows around its changes plus an outline, and lock or
  generated files as a stat line; `auto` is `full` unless the change alone is over the budget, then `windowed`,
  re-widened to fill the budget. Recall of windowed material is untested; see `docs/CONTEXT-COMPACTION.md`.
- **Your own material:** `--input FILE` takes the material as one file instead.
- **Tools:** the call has no tools on `claude`, `codex-chatgpt` and `ollama:`. The `codex` preset keeps a read-only
  shell, and a custom command's tools are unknown.
- **A second checker:** `--checker-backend SPEC` runs a plan's verdicts pass and its repair on another backend, so a
  different model or provider checks the findings. The plan needs a verdicts pass, and the trace records which
  backend made each call.
- **ReviewBench output:** `--findings-json FILE` also writes the findings as
  [ReviewBench](https://github.com/review-bench/ReviewBench) agent-contract JSON, one entry per finding and file.
  With `--repo`, files are checked against head and lines are clamped.
  [adapters/reviewbench](../adapters/reviewbench/README.md) runs Ringo as a ReviewBench reviewer image, so runs can be
  reproduced and scored with the benchmark's own judge. No ReviewBench result is claimed here.
- **Custom plans:** JSON files under `plans/`, with their instructions under `prompts/`.

The [harness guide](HARNESS.md) has the full details.

### Write a design

```sh
python3 ringo.py design --input brief.md --out proposal.md
```

Supply the decision brief and its hard constraints.

### Do bounded work

`work` revises a declared set of text files:

1. A writer model returns complete files, or exact search-and-replace edits that Ringo turns into complete files.
2. Fixed executable checks run on them. Failed checks go straight back to the writer for the next round.
3. The first candidate that passes every check gets one review, and the review's findings drive one revision.
4. A result that still passes every check goes to the run's `final/` folder for your review, together with the review
   it answered. Nothing is applied, adopted or published.

**The Design Council is opt-in.** A task that names a council bridge and rubric gets a council decision instead of
the review. Use the council's `rubrics/report.json` for documents and reports, or `artifact_design.json` for visual
artifacts. It is judged with 6 samples by default. In a controlled code test, the Council cost 7x the tokens of checks
alone with no significant quality gain. One review plus one revision delivered every candidate passing all
acceptance tests, at a third of the Council's tokens.

**Optional spec fields** tune `work` for cost and safety. The defaults are unchanged.

| Field | What it does |
| --- | --- |
| `roles` | Sets the model and reasoning effort for each call role (writer, revision, escalate, tests, review) |
| `protocol: "text"` | Uses plain `@@@` blocks instead of JSON-escaped files, with exact `REPLACE` edits for revisions |
| `protocol_repair` | Makes one immediate call asking for whole files, instead of losing the round to a reply that can't be applied |
| `no_regression` | Sets aside a revision that breaks a check, or a spec test the adopted candidate passed. For documents with `scripts`, a revision may not introduce a number that no successful script printed |
| `scripts` | Has a document's writer compute its numbers with a script, which Ringo runs and shows to the writer and the review |
| `spec_tests` | Adds blind tests written from the brief |
| `candidates` | Writes k first drafts and keeps the best one, chosen by running them |

`candidates` adds k - 1 writer calls in round 1, `protocol_repair` adds at most one call per round, and `spec_tests`
adds one. In a check, script or spec-test command, `"{python}"` means the interpreter running Ringo, so one spec runs
on every OS.

The [work guide](COUNCIL-WORKFLOW.md) covers the Council measurement, the task format, outputs, budgets and
trust limits.

#### Recommended settings: V3

V3 is the configuration our experiments favour (see
[experiments log](EXPERIMENTS.md)):

- one strong model at low effort on every role
- the text protocol, protocol repair and the no-regression gate
- review material without rewrite diffs
- one review, and at most three rounds

It adds nothing that costs extra tokens. To use it on your own task, add these fields to its spec:

```json
"protocol": "text", "protocol_repair": true, "no_regression": true, "max_rounds": 3,
"roles": {"writer": {"effort": "low"}, "revision": {"effort": "low"}, "review": {"effort": "low"}}
```

With a Claude backend, leave out `roles`, because Claude takes a model, not an effort. `scripts` is not part of V3:
it adds 1–2k tokens per document, so use it only for a document whose numbers must be computed.

### Research

`research` answers a brief from web sources that Ringo checks itself. It is new and unmeasured. Nothing here claims
that it finds better sources, or costs fewer tokens, than asking a model directly.

It makes at most two model calls, with no retry, repair or probe call. `--timeout` is a hard limit per call, 600
seconds by default.

1. **Gather.** One call on the `codex-search` preset: ChatGPT login, API keys removed, read-only, every tool off
   except live web search. The reply lists sources, each with narrow claims and an exact quote from the page.
2. **Verify.** Ringo fetches every URL itself, in code, at no token cost. It accepts a quote only if it finds it on
   that page, ignoring differences in whitespace, curly quotes and dashes (a match that differs only in case is
   accepted and flagged). DOIs and arXiv ids are looked up, and their titles are compared with the claimed work.
3. **Write.** One call with no tools (`--backend`, default `codex-chatgpt`; `claude` and `ollama:MODEL` also work)
   sees only the verified evidence. Ringo numbers the citations, writes the source list, and checks that every
   citation points to verified evidence. If the reply is in the wrong form, Ringo writes the verified evidence list
   instead.

Things to know:

- **"Verified" means the quote was on the page when Ringo fetched it.** It doesn't mean the page is true or that
  the report's sentences follow from the quotes. For anything that matters, check the evidence yourself.
- **Primary sources rank first** (standards, confirmed papers, official documentation), by simple rules on the
  address and page metadata. Aggregators and user-generated sites rank last.
- **The brief is sent to a web search.** A brief with a file path, code, a credential, an email address or a marker
  such as CONFIDENTIAL is refused unless you pass `--allow-private-search`.
- **No fallbacks.** Research never falls back to an API key, a model's own knowledge or an agent turn.

The [research guide](RESEARCH-ROLE.md) has the formats, the fetcher's safety limits, the ranking rules, and
the number of calls for each outcome.

### The `ringo` shell

Install it with `cargo install --locked --path cli`, then type `ringo` to open Ringo's own full-screen session.

- Say what you want in plain words. The agent you're talking to (Claude Code, Codex, Google's Antigravity CLI (agy),
  Grok or an agent you add) does it in that folder. The default `ask` mode allows project reads and edits;
  command execution and approval prompts depend on the agent. See the
  [permission guide](USER-GUIDE.md#choose-how-much-agents-may-do) for each provider's behavior.
  `/undo` takes a turn's change back.
- Give a task to several agents at once, each in its own copy, run Ringo's pipelines with them, and watch every job
  and subagent as it runs.
- On first start, the shell finds the agents installed on your computer and signs you in.
- One switch (`/mode ask|auto|read`, or Shift-Tab) sets how far every agent may go before it asks you.

Start with the step-by-step [user guide](USER-GUIDE.md) (`/help` in the shell). The full reference is
[docs/SHELL.md](SHELL.md).

### The native runtime

`review`, `design` and `work` also run natively inside the `ringo` binary. The Python runtime stays, unchanged, as
the reference.

- **Why Rust:** the shell, the planned IDE and its local model all need one engine to start, follow and stop
  pipelines. The binary also uses less memory per job (8.1 MB peak against 24.5 MB), and it is one program to
  install. Speed was not the reason: Python's own cost is 0.1–0.4% of a run.
- **Same requests:** the port sends the model the same bytes as the Python runtime, checked module by module and end
  to end. That's why the published results, measured on the Python runtime, still apply.
- **Releases:** a release can carry the binary only with a passing check for that exact binary. Windows keeps the
  Python runtime.

The [native runtime notes](NATIVE-RUNTIME.md) cover the measurements, the release gate, what stayed in Python,
and the bugs the port found.

## Model calls, time limits and cost

### How many calls

Ringo has no call cap. The plan or the round count decides the calls, and `--max-cost` and time limits bound the
money and time.

| Task | Model calls |
| --- | --- |
| `review` or `design`, default plan | One strong call. A call that fails or replies in the wrong shape is tried once more, so at most two |
| A custom plan | One per pass, each retried at most once, plus at most one checker repair. A batched checker makes one call per batch of findings (at most 50 findings per reader reply) |
| `work` | One writer call per round, plus one review after the first candidate that passes every check: at most `max_rounds + 2`, counting the review's one retry |
| `work` with the Council | Judge calls instead of the review: the first `first_judges` (3 of the default 6), and the rest only if those don't recommend. At most `max_rounds x (1 + samples)` |
| `research` | At most two, with no retries |

Outside research, a call you stop can add one retry, and a call that passes its time limit can add one probe call.

### Time limits

`--timeout` (review and design, default 900 seconds) and `timeout_s` (work, default 120 seconds) are soft limits. A
call past its limit keeps running. Ringo sends one tiny probe call to the backend (none under a cost cap), records an
incident in the trace, and prints one line. It recommends stopping only when the evidence suggests a stall. To stop
the call, write `stop` to the decision file Ringo prints. The call is then retried once. Every call is stopped at
twice its soft limit (never under 900 seconds, and in `work` also at the run deadline), with no retry. The
[harness guide](HARNESS.md) has the details.

### Waiting for a run

A run with `--trace`, and every `work` run, keeps a small `state.json`. `python3 ringo.py wait DIR` blocks until the
run finishes and prints it, so an agent can wait with one command instead of polling.

### Usage and cost

- Calls use your selected account's allowance, or its API billing, depending on how it signs in and its plan.
  Unknown cost is not zero, and Codex's dollar cost is unknown.
- Usage reports use [token counts](TOKEN-USAGE.md): uncached input, cached input, cache writes, output and their
  reported total. Missing usage is marked as missing.
- The goal is fewer whole-task tokens than the same agent working alone, at equal or better quality, counting
  harness and Council overhead and retries. No monetary spending cap has been set for this work, dollar estimates
  are left out, and protected evaluation gates stay unchanged.
- Opening the CLI help or running the tests makes no provider calls.

## Backends

| Backend | What it runs |
| --- | --- |
| `claude` | Claude Code. API keys and endpoint overrides are removed, so calls stay on your subscription login |
| `claude:<command>` | Your own Claude Code command line, with your environment kept |
| `codex-chatgpt` | Codex CLI with native ChatGPT sign-in forced, API-key and endpoint overrides removed, and the shell off |
| `codex` | Codex CLI with its shell, and with your configured authentication |
| `codex-search` | `codex-chatgpt` with live web search, used by `research` |
| `codex:<command>` | Your own Codex command line, which must pass `--json`. It may use web search only if it passes `--search` |
| `ollama:MODEL` | A local Ollama model |

Commands that read stdin also work as backends.

How Ringo keeps calls isolated:

- **Every Codex backend** runs with a private `CODEX_HOME` for each call, holding only a copy of your `auth.json`, and
  an empty `HOME`. So your global Codex instructions (`~/.codex/AGENTS.md`), skills (`~/.codex/skills`,
  `~/.agents/skills`) and memories don't load. A login the call refreshes is written back to your `auth.json` only
  if that file didn't change during the call.
- **`codex-chatgpt` calls are single turns with no tools.** So Ringo replaces the Codex agent prompt with one line
  and drops the code-mode, multi-agent, skills and environment blocks that Codex adds to every request (see the
  [improvement log](IMPROVEMENT-LOG.md)).
- **Tool use is checked.** Ringo parses the CLI's JSON events and records the tokens it reports. A turn that uses a
  tool its backend doesn't declare is rejected.
- **Traces are private.** `--trace` takes a new folder outside this checkout. It contains private material.

Sign in with each provider's supported CLI.

## Evaluation, improvement and releases

Ringo is optimized for working code, sound design judgment and architectural insight, with concrete task outcomes
and separate evidence for each. Whole-task token efficiency remains an objective while correctness and quality hold.
The [engineering objectives](ENGINEERING-OBJECTIVES.md) cover task families, measurement, the Council's
responsibilities and the gaps in today's review-only evaluator. That direction doesn't start an evaluation or
improvement campaign by itself.

```sh
python3 evaluate.py --help
python3 improve.py --help
python3 ringo.py release --help
```

- **Evaluation** (`evaluate.py`) freezes its inputs, separates repository groups, grades blind, records an integrity
  ledger, and controls how validation and test data are reused.
- **Improvement campaigns** (`improve.py`) search review prompts and plans against development evidence. A finalist
  needs a sealed-test pass and a person's adoption. These gates stay advisory, and nothing is adopted automatically.
  (`work` repairs one task's files; it never changes Ringo's own prompts or plans.)
- **Releases** are local, versioned bundles with explicit activation and rollback, pinned runs and a separate offline
  latency gate. Staging a release doesn't activate it. Adopting an RRSI candidate still needs sealed-test evidence and
  a human decision. Timing experiments use simulated backends and make no provider calls.
- **Budgets:** a dollar-budgeted run needs an explicitly declared fallback for attempts without a price. The last
  admitted call can go past the remaining budget.
- **Isolation limits:** backend calls start in empty folders, which are not filesystem boundaries. Codex and custom
  commands may keep file-access tools, so use an external boundary when answer keys must be out of reach. Evaluation
  locking currently uses POSIX `fcntl`.

More detail:

| Document | What it covers |
| --- | --- |
| [Harness guide](HARNESS.md) and [agent skill](../skills/ringo-harness/SKILL.md) | How the harness, evaluation and improvement work |
| [Improvement log](IMPROVEMENT-LOG.md) | Each measured improvement, when it was measured and the change that made it. `improvement_log.py` recomputes every number from raw evidence under `docs/improvements/`, and the tests refuse an entry whose evidence doesn't show the improvement |
| [Releases and latency checks](RELEASES.md) | Release bundles, activation, rollback and the latency gate |
| [Optimization regression cases](OPTIMIZE-CASES.md) | Release integrity before inference, fresh-invocation rechecks and complete timing schedules |
| [Overfitting audit](OVERFITTING-GUARDRAILS.md) | The evidence and held-out-data guardrails, their tests and their remaining limits |
| [HarBenchmark](../HarBenchmark.md) | Twelve public development cases and an offline scorer, covering review, constrained repairs, design calculations, artifact consistency, research arithmetic and evaluation/accounting. Its tables keep sourced external results apart from local development evidence and unmeasured coverage |
| [Benchmark gaps](BENCHMARK-GAPS.md) and [security/CI bot brief](SECURITY-CI-BOT-RESEARCH.md) | Research on benchmark gaps, and a brief for a separate security and CI bot |

These tools make no provider calls and add no work to a normal harness run. The full offline suite also runs in
GitHub Actions on Python 3.11 and 3.14.

## Use from an agent

The [Ringo workflow skill](../skills/ringo-harness/SKILL.md) connects ordinary coding and design work to real, bounded
harness calls.

1. Link the skill's folder into the agent's user skills folder: `~/.agents/skills/` for Codex, `~/.claude/skills/`
   for Claude Code.
2. Set the agent's local instructions to use it for substantive engineering work.
3. Set `RINGO_HOME` to this checkout if the agent needs to find it.
4. Start a new agent session after changing global instructions.

The CLI is still run explicitly. Installing the skill is guidance, not a background daemon.

## Ringo and the Design Council

- The runtime has no import or hardcoded path dependency on the council checkout. `backends.py` holds the minimal
  extracted helpers for subprocesses, command presets and JSON parsing.
- Council judging stays in its own repository and isn't bundled into this CLI.
- A `work` task that opts into the Council supplies a council bridge command and rubric explicitly. The council
  scores the replies it's given and makes no model calls. Ringo makes the writer and judge calls and owns their
  budget.
- Existing evaluation stores must be re-frozen for this source layout. Don't reuse old digests or copy sealed data
  into this repository.
- No performance or quality gain is claimed from separating the repositories.

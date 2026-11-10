# Ringo

Ringo is a local harness that runs model CLIs (Claude Code, Codex, Ollama) for four kinds of job:

- reviewing code changes
- writing design proposals
- doing small pieces of work, with checks and a review
- answering research questions from sources it verifies

It can also improve its own prompts and pipelines, behind guards. It uses frozen provider models and does not train
model weights. A task's plan sets how many model calls it makes. Nothing is applied, adopted or published until a
person reviews it.

Related repositories:

- The [Design Council](https://github.com/SkylerLK510/DukChulBOT) judges proposals: rubric scoring, decision gates
  and journals. Ringo makes the model calls and owns their budget.
- The [Ringo IDE](https://github.com/SkylerLK510/Ringo-IDE) will be a separate client of this harness (a Rust engine;
  see [Roadmap](#roadmap)). None of it is implemented here.

**About the name:** the harness and the IDE are both named after Ringo, one of my three beloved cats.

## Contents

- [Quick start](#quick-start)
- [What Ringo does](#what-ringo-does)
- [Using Ringo](#using-ringo): [review](#review-a-change), [design](#write-a-design), [work](#do-bounded-work),
  [research](#research), [the shell](#the-ringo-shell)
- [Model calls, time limits and cost](#model-calls-time-limits-and-cost)
- [Backends](#backends)
- [Experiments](#experiments-what-we-tried-what-broke-what-we-measured)
- [Evaluation, improvement and releases](#evaluation-improvement-and-releases)
- [Roadmap](#roadmap)
- [Use from an agent](#use-from-an-agent)
- [Ringo and the Design Council](#ringo-and-the-design-council)
- [Credits](#credits) and [License](#license)

## Quick start

Ringo is one Python program that uses only the standard library. You need:

- Python 3.11 or newer
- git, or a ZIP download
- at least one model CLI, to make model calls (the offline tests need none)

### macOS

```sh
brew install python git                    # Homebrew; or install Python from python.org
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
python3 --version                          # must print 3.11 or newer
python3 -m unittest discover -s tests -q   # offline check: makes no model calls
python3 ringo.py --help
```

The `python3` that comes with Apple's command-line tools may be older than 3.11. If it is, use the Homebrew or
python.org Python.

### Linux

```sh
sudo apt update && sudo apt install -y python3 git   # Debian, Ubuntu
# sudo dnf install -y python3 git                    # Fedora, RHEL
# sudo pacman -S python git                          # Arch
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
python3 --version                          # must print 3.11 or newer
python3 -m unittest discover -s tests -q
python3 ringo.py --help
```

Ubuntu 24.04 and later ship Python 3.12. On an older release, install Python 3.11 or newer first.

### Windows

Ringo runs natively with Python 3.11 or newer, and the offline test suite passes on Windows in CI. Real model calls
have so far been run only on macOS.

- Native Windows refuses `.cmd` and `.bat` backend and check programs. Call the underlying executable directly, for
  example `node.exe` with the CLI's JavaScript entry point.
- Private folders are locked to your Windows account. [Portability](docs/EXPERIMENTS.md#9-portability) explains
  what that covers and what it doesn't.

To run Ringo as on Linux instead, use WSL 2 (the Windows Subsystem for Linux):

1. Open PowerShell as Administrator, run `wsl --install`, then restart Windows. This installs WSL 2 with Ubuntu.
2. Open **Ubuntu** from the Start menu and follow the Linux steps above inside it.
3. Install the model CLIs inside Ubuntu too, using the Linux commands below, so Ringo can find them.

### Without git

On the repository page, choose **Code**, then **Download ZIP**, or download an archive from
[Releases](https://github.com/SkylerLK510/Ringo/releases). Unzip it, open a terminal in the folder, and run the
checks above.

### Model CLIs

Install at least one. Ringo calls them with your existing sign-in and doesn't need an API key.

- **Codex CLI** ([instructions](https://github.com/openai/codex); Ringo backend `codex-chatgpt`):
  - macOS, Linux, WSL: `curl -fsSL https://chatgpt.com/codex/install.sh | sh`, `brew install --cask codex` or
    `npm install -g @openai/codex`.
  - Windows PowerShell: `powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"`.
  - Sign in: run `codex` and choose **Sign in with ChatGPT**. A Plus, Pro, Business, Edu or Enterprise plan covers
    the calls.
- **Claude Code** ([instructions](https://code.claude.com/docs/en/setup); Ringo backend `claude`):
  - macOS, Linux, WSL: `curl -fsSL https://claude.ai/install.sh | bash` or `brew install --cask claude-code`.
  - Windows: `irm https://claude.ai/install.ps1 | iex` in PowerShell, or `winget install Anthropic.ClaudeCode`.
  - Sign in: run `claude` and follow the browser prompts. It needs a Pro, Max, Team, Enterprise or Console account.
- **Ollama** for local models (Ringo backend `ollama:MODEL`): see [ollama.com/download](https://ollama.com/download).

### First run

```sh
python3 ringo.py work --spec examples/v3-codex.json --out ~/ringo-runs/first-run
# or, with Claude Code:
python3 ringo.py work --spec examples/v3-claude.json --out ~/ringo-runs/first-run-claude
```

The example asks for a small `slugify` function with explicit rules. Ringo:

1. writes `slug.py`
2. runs the example's check against it
3. reviews the result
4. puts the version that's ready for your review in `~/ringo-runs/first-run/final/`

`report.json` lists the rounds, checks and token usage. Our first run of the Codex example took one round, two model
calls, about 2,800 tokens and 14 seconds.

Run folders must be outside the Ringo checkout, which keeps private material out of the repository. Ringo creates
them with owner-only permissions.

## What Ringo does

| Command | What it does |
| --- | --- |
| `review` | Reviews a code change and lists its findings |
| `design` | Writes a design proposal from a brief |
| `work` | Writes or revises a set of files until fixed checks pass, then gets one review and makes one revision |
| `research` | Answers a brief from web sources that Ringo checks itself |
| `wait DIR` | Waits for a background run to finish and prints its state |
| `release` | Manages approved, versioned releases of Ringo itself |
| `ringo` (the shell) | A full-screen session for working with several agents and running Ringo's pipelines |

Run the commands with `python3 ringo.py COMMAND`, or with the `ringo` binary (`cargo install --path cli`), which
takes the same commands. The binary doesn't run `research` yet.

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
- **Your own material:** `--input FILE` takes the material as one file instead.
- **Tools:** the call has no tools on `claude`, `codex-chatgpt` and `ollama:`. The `codex` preset keeps a read-only
  shell, and a custom command's tools are unknown.
- **A second checker:** `--checker-backend SPEC` runs a plan's verdicts pass and its repair on another backend, so a
  different model or provider checks the findings. The plan needs a verdicts pass, and the trace records which
  backend made each call.
- **ReviewBench output:** `--findings-json FILE` also writes the findings as
  [ReviewBench](https://github.com/review-bench/ReviewBench) agent-contract JSON, one entry per finding and file.
  With `--repo`, files are checked against head and lines are clamped.
  [adapters/reviewbench](adapters/reviewbench/README.md) runs Ringo as a ReviewBench reviewer image, so runs can be
  reproduced and scored with the benchmark's own judge. No ReviewBench result is claimed here.
- **Custom plans:** JSON files under `plans/`, with their instructions under `prompts/`.

The [harness guide](docs/HARNESS.md) has the full details.

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

The [work guide](docs/COUNCIL-WORKFLOW.md) covers the Council measurement, the task format, outputs, budgets and
trust limits.

#### Recommended settings: V3

V3 is the configuration our experiments favour (see
[Experiments](#experiments-what-we-tried-what-broke-what-we-measured)):

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

The [research guide](docs/RESEARCH-ROLE.md) has the formats, the fetcher's safety limits, the ranking rules, and
the number of calls for each outcome.

### The `ringo` shell

Install it with `cargo install --path cli`, then type `ringo` to open Ringo's own full-screen session.

- Say what you want in plain words. The agent you're talking to (Claude Code, Codex, Google's Antigravity CLI (agy),
  Grok or an agent you add) does it in that folder, and asks before it runs a command. `/undo` takes a turn's change
  back.
- Give a task to several agents at once, each in its own copy, run Ringo's pipelines with them, and watch every job
  and subagent as it runs.
- On first start, the shell finds the agents installed on your computer and signs you in.
- One switch (`/mode ask|auto|read`, or Shift-Tab) sets how far every agent may go before it asks you.

Start with the step-by-step [user guide](docs/USER-GUIDE.md) (`/help` in the shell). The full reference is
[docs/SHELL.md](docs/SHELL.md).

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

The [native runtime notes](docs/NATIVE-RUNTIME.md) cover the measurements, the release gate, what stayed in Python,
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
[harness guide](docs/HARNESS.md) has the details.

### Waiting for a run

A run with `--trace`, and every `work` run, keeps a small `state.json`. `python3 ringo.py wait DIR` blocks until the
run finishes and prints it, so an agent can wait with one command instead of polling.

### Usage and cost

- Calls use your selected account's allowance, or its API billing, depending on how it signs in and its plan.
  Unknown cost is not zero, and Codex's dollar cost is unknown.
- Usage reports use [token counts](docs/TOKEN-USAGE.md): uncached input, cached input, cache writes, output and their
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
  [improvement log](docs/IMPROVEMENT-LOG.md)).
- **Tool use is checked.** Ringo parses the CLI's JSON events and records the tokens it reports. A turn that uses a
  tool its backend doesn't declare is rejected.
- **Traces are private.** `--trace` takes a new folder outside this checkout. It contains private material.

Sign in with each provider's supported CLI.

## Experiments: what we tried, what broke, what we measured

Every number comes from our own runs. Task contents, hidden tests and raw traces stay private so the tasks can be
reused. The [experiments log](docs/EXPERIMENTS.md) has the method, every study, the mistakes we found and the ideas
we rejected.

**The current adoption rule:** a change is adopted only if it uses no more tokens than what it replaces. Cheaper
credits do not justify more tokens. An evaluation or improvement campaign enforces it only when its spec sets
`max_token_ratio` (see the [harness guide](docs/HARNESS.md)): the evaluator's own cost gates are in dollars.

### Results at a glance

| Question | What we found | Section |
| --- | --- | --- |
| Is the multi-judge Design Council worth it by default? | About 7x the tokens of checks alone, with no significant quality gain. Now opt-in | [1](docs/EXPERIMENTS.md#1-the-design-council-2026-10-27) |
| Where do the tokens go? | Output tokens are the cost. Most of an agent's tokens are cached re-reads, and high effort on every call bought nothing | [2](docs/EXPERIMENTS.md#2-where-the-tokens-go-one-code-task-with-26-hidden-tests-one-run-per-arm-all-2626) |
| Ringo against the Codex agent, held-out tasks | Same quality (100%). Low-effort Ringo, and later V3, matched the low-effort agent's credits with about 80% fewer tokens | [3](docs/EXPERIMENTS.md#3-current-ringo-against-the-agents-held-out-tasks-phase-1), [6](docs/EXPERIMENTS.md#6-v3-removing-the-kinks) |
| HumanEval+, 154 tasks | A quality tie (147 against 148). Ringo used 14.4x fewer tokens and 2.2x fewer credits | [7](docs/EXPERIMENTS.md#7-humaneval-the-review-earns-its-place-on-code-2026-10-28) |
| Four refinements built from HumanEval+ failures | Made Ringo worse on MBPP+ (126 against 132 of 150, p = 0.031). All removed | [8](docs/EXPERIMENTS.md#8-mbpp-four-refinements-from-humaneval-failures-rejected-2026-10-29) |

The log also covers building and testing v2 (sections [4](docs/EXPERIMENTS.md#4-building-v2-trials-and-errors) and
[5](docs/EXPERIMENTS.md#5-v2-on-the-held-out-tasks-phase-2)), [portability](docs/EXPERIMENTS.md#9-portability), the
[ideas we rejected](docs/EXPERIMENTS.md#10-ideas-we-considered-and-did-not-adopt) and
[what would move Ringo next](docs/EXPERIMENTS.md#11-what-would-move-it-next).

## Evaluation, improvement and releases

Ringo is optimized for working code, sound design judgment and architectural insight, with concrete task outcomes
and separate evidence for each. Whole-task token efficiency remains an objective while correctness and quality hold.
The [engineering objectives](docs/ENGINEERING-OBJECTIVES.md) cover task families, measurement, the Council's
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
| [Harness guide](docs/HARNESS.md) and [agent skill](skills/ringo-harness/SKILL.md) | How the harness, evaluation and improvement work |
| [Improvement log](docs/IMPROVEMENT-LOG.md) | Each measured improvement, when it was measured and the change that made it. `improvement_log.py` recomputes every number from raw evidence under `docs/improvements/`, and the tests refuse an entry whose evidence doesn't show the improvement |
| [Releases and latency checks](docs/RELEASES.md) | Release bundles, activation, rollback and the latency gate |
| [Optimization regression cases](docs/OPTIMIZE-CASES.md) | Release integrity before inference, fresh-invocation rechecks and complete timing schedules |
| [Overfitting audit](docs/OVERFITTING-GUARDRAILS.md) | The evidence and held-out-data guardrails, their tests and their remaining limits |
| [HarBenchmark](HarBenchmark.md) | Twelve public development cases and an offline scorer, covering review, constrained repairs, design calculations, artifact consistency, research arithmetic and evaluation/accounting. Its tables keep sourced external results apart from local development evidence and unmeasured coverage |
| [Benchmark gaps](docs/BENCHMARK-GAPS.md) and [security/CI bot brief](docs/SECURITY-CI-BOT-RESEARCH.md) | Research on benchmark gaps, and a brief for a separate security and CI bot |

These tools make no provider calls and add no work to a normal harness run. The full offline suite also runs in
GitHub Actions on Python 3.11 and 3.14.

## Roadmap

- **IDE:** a Rust engine that runs on macOS, with support for Python, Jupyter notebooks, Rust and C/C++. It will
  carry live context, one-click decisions where a spec is ambiguous, and per-repository preference memory, as a
  separate client of this harness. Its local model is meant to do more than complete code. It would keep Claude,
  Codex and other agents supplied with current facts about the code, so they spend fewer tokens finding them. It
  would also audit a codebase in the background, proposing small fixes that pass the project's checks for the user
  to approve. None of it is built or measured yet.
- **Native Windows model calls:** the offline suite passes natively, but real Claude and Codex calls still need a run.
- **Audit-Gated Continual Learning ([AGCL](docs/AGCL.md)):** Ringo learns when it can stop spending tokens, and it
  changes behavior only through a release with a measured harm bound and human approval. Built so far:
  `shadow_labels.py` and the HumanEval+ studies in `evalplus_study.py`. The first candidate, stopping when the checks
  pass, was rejected by its sealed study. A new candidate needs a new study on untouched tasks.
- **Benchmarks with headroom,** so quality gains can be measured.
- **Research workloads:** the target includes intensive ML and finance research. The
  [research requirements](docs/RESEARCH-REQUIREMENTS.md) set goals for evidence, reproducibility, numerical testing
  and domain validation, with the Design Council for structured assessment and Ringo for execution and validation.
  Basic integration exists: `work` combines generated files, fixed checks and an opt-in council assessment. The
  research workflow those requirements describe is future work, and the harness hasn't been validated for that
  workload.

## Use from an agent

The [Ringo workflow skill](skills/ringo-harness/SKILL.md) connects ordinary coding and design work to real, bounded
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

## Credits

We credit **Peng Xia and coauthors at Google Cloud AI Research**, with collaborators at UNC-Chapel Hill, Stanford
University and Washington University in St. Louis, for *RRSI: Regularized Recursive Self-Improvement of Agent
Harnesses* (2026). Their work inspired this part of Ringo's improvement loop, and Ringo adapts selected ideas from the
paper. See the [original paper](https://arxiv.org/abs/2609.24972) and the
[authors' project page](https://regularized-rsi.com/).

The proposal loop also draws on [Meta-Harness](https://arxiv.org/abs/2603.28052). Ringo implements selected ideas;
it is not a full reproduction of either paper. The [harness guide](docs/HARNESS.md) describes the implemented limits
and the confirmation they require.

## License

Ringo is licensed under the [Apache License, Version 2.0](LICENSE).

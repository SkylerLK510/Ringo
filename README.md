# Ringo

A local harness for code review, design proposals, bounded checked and reviewed
artifact work, and guarded improvement of its own prompts and pipelines. Ringo
runs model CLIs and assembles their results; it uses frozen provider models and
does not train model weights.

This repository owns the harness, its model calls and their budget. The original
[design council](https://github.com/SkylerLK510/DukChulBOT) owns proposal judging,
rubric scoring, decision gates and journals. A future IDE (a Rust engine; see
[Roadmap](#roadmap)) will be a separate client; none is implemented here.

**About the name:** the harness and the [Ringo IDE](https://github.com/SkylerLK510/Ringo-IDE) are both named after
Ringo, one of my three beloved cats.

## Install

Ringo is one Python program that uses only the standard library. You need:
- Python 3.11 or newer;
- git, or a ZIP download;
- at least one model CLI for model calls. The offline test suite needs none.

### macOS

```sh
brew install python git                    # Homebrew; or install Python from python.org
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
python3 --version                          # must print 3.11 or newer
python3 -m unittest discover -s tests -q   # offline check: makes no model calls
python3 ringo.py --help
```

The `python3` that comes with Apple's command-line tools may be older than 3.11. If `python3 --version` prints an
older version, use the Homebrew or python.org Python.

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

Ubuntu 24.04 and later ship Python 3.12. On an older release, install Python 3.11 or newer before running Ringo.

### Windows

The offline test suite passes natively on Windows (CI runs it on `windows-latest` with Python 3.11 and 3.14), so
Ringo runs with a native Python 3.11 or newer. Real model calls have so far been run only on macOS, so they are
untested natively. To run Ringo as on Linux instead, use WSL 2, the Windows Subsystem for Linux:

1. Open PowerShell as Administrator, run `wsl --install`, then restart Windows. This installs WSL 2 with Ubuntu.
2. Open **Ubuntu** from the Start menu and follow the Linux steps above inside it.
3. Install the model CLIs inside Ubuntu too, using the Linux commands below, so Ringo can find them.

### Without git

On the repository page, choose **Code**, then **Download ZIP**, or download an archive from
[Releases](https://github.com/SkylerLK510/Ringo/releases). Unzip it, open a terminal in the folder, and run the
checks above.

### Model CLIs

Install at least one, from its official instructions ([Codex CLI](https://github.com/openai/codex),
[Claude Code](https://code.claude.com/docs/en/setup)):

- **Codex CLI** (Ringo backend `codex-chatgpt`):
  - macOS, Linux, WSL: `curl -fsSL https://chatgpt.com/codex/install.sh | sh`, `brew install --cask codex` or
    `npm install -g @openai/codex`.
  - Windows PowerShell: `powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex"`.
  - Sign in: run `codex` and choose **Sign in with ChatGPT**. A Plus, Pro, Business, Edu or Enterprise plan covers
    the calls.
- **Claude Code** (Ringo backend `claude`):
  - macOS, Linux, WSL: `curl -fsSL https://claude.ai/install.sh | bash` or `brew install --cask claude-code`.
  - Windows: `irm https://claude.ai/install.ps1 | iex` in PowerShell, or `winget install Anthropic.ClaudeCode`.
  - Sign in: run `claude` and follow the browser prompts. It needs a Pro, Max, Team, Enterprise or Console account.
- **Ollama** for local models (Ringo backend `ollama:MODEL`): see [ollama.com/download](https://ollama.com/download).

Ringo calls these CLIs with your existing sign-in. It does not need an API key.

### First run

```sh
python3 ringo.py work --spec examples/v3-codex.json --out ~/ringo-runs/first-run
# or, with Claude Code:
python3 ringo.py work --spec examples/v3-claude.json --out ~/ringo-runs/first-run-claude
```

The example asks for a small `slugify` function with explicit rules. Ringo then:
1. writes `slug.py`;
2. runs the example's check against it;
3. reviews the result;
4. puts the version ready for your review in `~/ringo-runs/first-run/final/`.

`report.json` lists the rounds, checks and token usage. Our first run of the Codex example took one round, two model
calls, about 2,800 tokens and 14 seconds. Run folders must be outside the Ringo checkout, which keeps private
material out of the repository. Ringo creates them with owner-only permissions.

### The `ringo` shell

`cargo install --path cli`, then type `ringo`: Ringo's own terminal session, in which you route work to Codex, Claude
Code and Grok, run Ringo's pipelines with them, and watch every job and subagent in a live panel. The first start
connects and signs in your agents. See [docs/SHELL.md](docs/SHELL.md).

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
plus at most one checker repair. `work` makes one writer call per round and one
review call after the first candidate that passes every executable check, at most
`max_rounds + 2` with the review's one retry. Failed checks and review findings
feed the next writer round directly. A task that opts into the Council makes judge
calls instead: the first `first_judges` (3 of the default 6), and the rest only if
those do not recommend, at most `max_rounds x (1 + samples)`. A call you stop can
add one retry, and a call that passes its time limit can add one probe call. There
is no call cap: the plan or the round count decides the calls, and `--max-cost` and
time limits bound money and time.

`--timeout` (review/design, default 900) and `timeout_s` (work, default 120) are soft: a call past it is not killed. Ringo checks it for free, sends one tiny probe call to the same backend (none under a cost cap), writes `incidents/<step>.<attempt>.json` in the trace (a private temporary folder without one) and prints one line, recommending a stop only when the evidence suggests a stall: the backend still answers and comparable calls in the run finished in a third of the time, or the output shows reconnects. Writing `stop` to the printed decision file stops the call and retries it once; nothing decides for you. Every call is stopped at twice its soft limit, never under 900 seconds (and, in `work`, at the run deadline), and is not retried after that.

A run with `--trace` (and every `work` run) keeps a small `state.json`; `python3 ringo.py wait DIR` blocks until
it finishes and prints it, so an agent waits with one command instead of polling.

`work` revises a declared set of text files: a writer model returns complete
files or exact search/replace edits, which Ringo turns into complete files; fixed
executable checks run on them. The first candidate that passes them gets one
review, and its findings drive one revision. A result that passes every check
after that is written to the run's `final/` folder for human review, with the
review it answered; nothing is applied, adopted or published. The Design Council
is opt-in: a task that names a council bridge and rubric (the council's
`rubrics/report.json` for documents and reports, `artifact_design.json` for visual
artifacts) gets a council decision instead of the review, judged with 6 samples by
default. In a controlled code test the Council cost 7x the tokens of checks alone
with no significant quality gain, and one review plus one revision delivered every
candidate passing all acceptance tests at a third of the Council's tokens. See
[the work guide](docs/COUNCIL-WORKFLOW.md) for the measurement, the task format,
outputs, budgets and trust limits.

Opt-in spec fields tune `work` for cost and safety, and the defaults are unchanged:
- `roles`: the model and reasoning effort for each call role (writer, revision, escalate, tests, review).
- `protocol: "text"`: plain `@@@` blocks instead of JSON-escaped files, with exact `REPLACE` edits for revisions.
- `protocol_repair`: one immediate call, asking for whole files, instead of losing the round to a reply that cannot be
  applied.
- `no_regression`: a revision that breaks a check, or a spec test the adopted candidate passed, is set aside. For
  documents with `scripts`, a revision may not introduce a number that no successful script printed.
- `scripts`: a document writer computes its numbers with a script that Ringo runs and shows to the writer and the
  review.
- `spec_tests` and `candidates`: blind tests written from the brief, and best-of-k first drafts selected by execution.

`candidates` adds k - 1 writer calls in round 1, `protocol_repair` at most one call per round and `spec_tests` one.

### V3: the recommended `work` settings

V3 is the configuration our experiments favour (see [Experiments](#experiments-what-we-tried-what-broke-what-we-measured)):
- one strong model at low effort on every role;
- the text protocol, protocol repair and the no-regression gate;
- review material without rewrite diffs;
- one review, and at most three rounds.

It adds nothing that costs extra tokens. To use it on your own task, add these fields to its spec:

```json
"protocol": "text", "protocol_repair": true, "no_regression": true, "max_rounds": 3,
"roles": {"writer": {"effort": "low"}, "revision": {"effort": "low"}, "review": {"effort": "low"}}
```

With a Claude backend, leave out `roles`, because Claude takes a model, not an effort. `"{python}"` in a check,
script or spec-test command means the interpreter running Ringo, so one spec runs on every OS. `scripts`
(writer-computed numbers for documents) is opt-in and not part of V3: it adds 1–2k tokens per document. Use it only
for a document whose numbers must be computed.

### Backends, findings and usage

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
endpoint overrides and disables the shell. Its calls are single tool-free turns, so
it also replaces the Codex agent prompt with one line and drops the code-mode,
multi-agent, skills and environment blocks Codex adds to every request (see the
[improvement log](docs/IMPROVEMENT-LOG.md)). The `codex` preset keeps its shell and
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
identified explicitly. The optimization goal is fewer whole-task tokens than the
same agent working alone at equal or better quality, including harness/Council
overhead and retries. No monetary spending cap has been set for this work.
Dollar estimates are omitted; protected evaluation gates remain unchanged.

## Experiments: what we tried, what broke, what we measured

Every number here comes from our own runs. The task contents, hidden tests and raw traces stay private so the tasks
can be reused. This section records the designs, the results and every mistake we found.

**The current adoption rule:** a change is adopted only if it uses no more tokens than what it replaces. Cheaper
credits do not justify more tokens.

### How we measure

- **Tasks:** each is a brief with hidden acceptance tests. Every hidden test traces to a sentence in the brief.
  Mutants (plausible wrong answers that still pass the public checks) show that the hidden tests catch real
  mistakes.
- **Held-out discipline:** held-out tasks are frozen by hash before any model call, and development uses separate
  tasks.
- **Arms:**
  - the Codex CLI agent alone at high effort (AH) and at low effort (AL);
  - Claude Code alone (CA);
  - Ringo configurations.

  Every arm gets the same brief, public checks and instructions.
- **Quality:** the share of hidden tests that the delivered files pass.
- **Cost:**
  - Tokens are all input (uncached and cached) plus all output, for every provider.
  - Codex credits use the published rates for GPT-6.1 Sol (50 per million uncached input tokens, 2.5 cached, 250
    output) and GPT-6 Luna (2.5, 0.25, 12.5).
- **Decision rule, written before the runs:** a harness wins outright only if its paired quality interval lies
  above zero and it costs less.
- **Statistics:** two runs per arm per task; a 95% paired bootstrap with 10,000 resamples.
- **Audit:** an agent run that touched the hidden material would be excluded. None did.

### 1. The Design Council (2026-10-27)

The multi-judge Council cost about 7× the tokens of checks alone, with no significant quality gain. One review and
one revision delivered every candidate passing all acceptance tests, at a third of the Council's tokens. The Council
became opt-in (#15).

### 2. Where the tokens go: one code task with 26 hidden tests, one run per arm, all 26/26

| Arm | Tokens | Credits |
|---|---|---|
| Codex agent alone, high effort | 190,695 | 5.11 |
| One bare model call, high effort | 9,305 | 1.93 |
| Ringo, writer plus 6 Council judges | 75,229 | 7.04 |
| Ringo, check-first Council | 71,941 | 6.61 |
| Ringo, one review, high effort | 54,275 | 6.61 |
| Ringo, one review, low effort | 21,754 | 2.05 |

What we learned:
- **Output tokens are the cost.** They cost 5× uncached input and 100× cached input.
- **The agent's tokens were mostly re-reads.** 84% were cached re-reads of its own session.
- **High effort on every call bought nothing.**
- **No caching across calls.** A fresh `codex exec` call gets no cache hits from an earlier call.

### 3. Current Ringo against the agents (held-out tasks, phase 1)

Six tasks were built for this study:
- four code tasks: semantic-version ranges, a ledger, cron schedules, patch application;
- two analysis memos: a vendor decision, an incident postmortem.

Each has 27–40 hidden tests, and each arm ran twice.

| Arm | Hidden tests passed | Credits per task | Tokens per task | Seconds |
|---|---|---|---|---|
| Codex agent, high (AH) | 100% | 2.94 | 153k | 115 |
| Codex agent, low (AL) | 100% | 1.52 | 89k | 46 |
| Claude Code (CA) | 100% | (Claude: $0.32 API-equivalent) | 150k | 66 |
| Ringo, every call high (RH) | 100% | 2.38 | 26k | 129 |
| Ringo, every call low (RL) | 100% | 1.49 | 20k | 45 |

- **No quality win was possible.** Every arm passed every hidden test.
- **RH:** used 1.57× AL's credits, so it lost to AL. High effort everywhere was the bloat.
- **RL:** used 0.98× AL's credits and 0.22× its tokens.

### 4. Building v2: trials and errors

We tuned only on development tasks: a 26-test job-queue task, an ISO 8601 duration library (40 hidden tests) and an
inventory reconciliation memo (35).

| We tried | What happened | What we changed |
|---|---|---|
| The text protocol as the default | 28 existing tests built on JSON broke | Made it opt-in (`protocol: "text"`) |
| The writer's `@@@ FILE` blocks | It closed them with `@@@ END`, so every round was malformed | `END` is accepted after any block |
| GPT-6 Luna writing spec tests | It quoted the brief in four-quote docstrings, which do not compile, so the suite was lost | Brief sentences go in `# brief:` comments; four-quote docstrings are repaired |
| Luna's edits | It wrote a bare `@@@` where `@@@ WITH` belonged | A bare marker is read by position |
| Luna's edit anchors | An anchor matched twice, and the round was lost | `protocol_repair`: one immediate call that asks for whole files |
| Three Luna drafts (job queue) | 0.24–0.31 credits against 5.11 for the agent, but 25/26 in four of four runs. Every run returned `''` where the brief said `empty`, and Luna's tests shared that blind spot | Luna is not a writer |
| Sol writer, Luna for everything else (job queue) | 26/26 at 0.81 credits. Half of Luna's tests were wrong, and an unchanged reply ended the run before the review | An unchanged reply stands by the code and goes on to the review |
| Two Sol drafts (duration library) | Both drafts passed 40/40, but revisions driven by wrong Luna tests delivered 36/40 twice | Dual agreement (a test every draft fails stops gating) and the no-regression gate, after which it passed 40/40 |
| Luna's rationale-only replies | Counted as malformed | They stand by the code |
| Three Luna drafts (memo) | Luna sent only scripts, the run ended (a bug), and Sol escalation then wrote the memo at 3.0 credits | Fixed the bug; dropped Luna as a writer |

### 5. v2 on the held-out tasks (phase 2)

| Arm | Hidden tests passed | Credits per task | Tokens per task |
|---|---|---|---|
| V2S: Sol writer; Luna tests, revision and review | 99.4% (one memo run lost 2 of 28) | 1.13 | 58k |
| V2K: V2S with two Sol drafts | 100% | 1.93 | 61k |

- **V2S** had the lowest credits, but nearly 3× RL's tokens and twice its time.
- **V2K** used more credits than AL.

Current Ringo was outperforming v2, and the data showed why:
- **The first draft was already right.** In all 36 runs we checked (RH, RL, V2S), Ringo's first draft passed every
  hidden test, so everything after round 1 was overhead.
- **The reviews' findings were all false.** Luna reviews raised findings on 12 of 12 V2S runs, all false. Sol
  reviews raised findings on 3 of 12 RL runs, also false but harmless.
- **The one loss:** the Sol writer had the right answer (40 customers, SEV2) for three rounds. Then a Luna review
  claimed the count was wrong, and a Luna revision changed it to 56 and SEV1 with no successful script behind it.

### 6. V3: removing the kinks

**Fixes:**
- **The number-evidence gate:** with `scripts`, a document revision may not introduce a number that no successful
  script printed. Replayed on the real loss, the revision's only unbacked number was the wrong 56, so the revision
  would have been set aside.
- **Review material:** a rewritten file gets no diff. A stub-to-implementation diff repeated the whole file, which
  was 2–3k of a code review's 8k input tokens.
- **Scripts:** a script whose output already matches the document no longer forces a round. Script logs no longer
  overwrite the round's check logs.
- **Prompts:** when feedback is wrong, change nothing and say why; resend the script when changing a number.

**Three Ringo code reviews of these changes found seven more bugs, all fixed with tests:**
- a spec-runner timeout crashed the run;
- unittest class fixtures were skipped;
- module fixtures were skipped;
- teardown errors were dropped;
- fixture errors were matched to the wrong module;
- the blind test writer was shown fixtures;
- the number gate ignored sign changes.

**Dev check (one run each):**

| Task | Current Ringo (RL) | V3 without scripts | V3 with scripts |
|---|---|---|---|
| Memo, 35 hidden tests | 34/35, 2.14 credits, 28k tokens | 34/35, 2.08 credits, 25k tokens | 35/35, 2.55 credits, 29k tokens |
| Duration library, 40 hidden tests | 40/40, 1.47 credits, 18k tokens | 40/40, 0.89 credits, 10k tokens | n/a (code) |

**Phase 3** used fresh repetitions 3 and 4 of the six held-out tasks. V3 was designed after seeing phases 1 and 2,
so this is a confirmation, not a clean held-out test.

| Arm | Hidden tests passed | Credits per task | Tokens per task | Seconds |
|---|---|---|---|---|
| V3 with memo scripts (V3s) | 100% | 1.44 | 17.7k | 46 |
| Current Ringo (RL) | 100% | 1.52 | 20.3k | 46 |
| Codex agent, low (AL) | 100% | 1.42 | 88.2k | 45 |

- **Against RL:** at equal quality, V3s used 5% fewer credits and 13% fewer tokens.
- **Against AL:** about equal credits (1.01×) and 80% fewer tokens.
- **By task:** on code, V3s used fewer tokens than RL on three of four tasks (14k against 25k, 11k against 14k, 14k
  against 21k) and the same on the ledger task (15k each), where RL used slightly fewer credits (1.18 against 1.20).
  On the memos the scripts added tokens: 23k against RL's 17k on the vendor memo. Under the adoption rule, the
  recommended V3 therefore leaves `scripts` out.

### 7. HumanEval+: the review earns its place on code (2026-10-28)

The first study with hidden-test headroom: 163 HumanEval+ tasks (EvalPlus v0.1.10, Apache-2.0; HumanEval/32 left
out because its own reference fails the benchmark's root check). The original HumanEval tests are the public check;
EvalPlus's extra inputs, about 80 times as many, are the hidden tests. Every task ran once on V3 (Codex, Sol low).
Details and the method are in [AGCL](docs/AGCL.md); task files and traces stay private.

**Against a standalone agent.** The same tasks also ran through the Codex CLI as a plain agent (Sol low, a fresh
workspace per task), in a fixed random order until an 80-credit ceiling stopped it after 154 tasks. On those 154:

| Arm | Hidden tests passed | Tokens per task | Credits per task |
|---|---|---|---|
| Codex agent | 148/154 | 50,325 | 0.513 |
| Ringo V3 | 147/154 | 3,500 | 0.235 |

Quality is a tie: the two disagree on 3 tasks (the agent alone passes 2, Ringo alone 1; exact McNemar p = 1.0), and 5
fail for both. Ringo uses 14.4 times fewer tokens and 2.2 times fewer credits. 87% of the agent's tokens are cached
input, its own context re-sent on each of about five steps. So Ringo does not beat the agent outright here: it
matches it at a fraction of the cost, and a quality win needs harder tasks.

| Idea | Measured | Decision |
|---|---|---|
| Stop after round 1 when the checks pass | Saves 53% of tokens. The review then caught one defect the public tests missed (HumanEval/137), so stopping harms 1 of 162 eligible runs: upper bound 2.9%, over the 2% bar set beforehand | Rejected; `stop_on_checks` removed |
| Keep the review on code | 24 planted defects that pass the public tests: the review flagged 24/24 and quoted the line on 23, with findings on 3/24 of the same drafts before seeding | Kept |
| A reviewer from another family (Claude Sonnet 5.5) | Same catch rate (24/24), no false alarms (0/24), but 51% more tokens per review (2,963 against 1,957) | Not adopted: adds tokens |
| Distinguishing inputs without a model: 200 inputs mutated from the public tests | Told 20 of 23 planted defects apart from the correct draft. Between Claude's and Codex's real drafts it flagged 1 of the 8 tasks Codex fails and 24 of the 150 it passes; it missed the one revision that mattered | Not adopted: misses the corners hidden tests reach |
| A second draft from another family | Either draft passes on 154 of 158 tasks, against 150 for Codex alone: real headroom, but no zero-token signal picks the right one | Open; nothing built on it |

**Mistakes found in the study itself:** HumanEval/38 and /50 test their function through a helper the prompt
defines, and the generated public check imported only the function, so it could never pass. Both were rerun with the
fix. 14 of 163 Claude drafts were replies Ringo could not apply; a `work` round answers that with one repair call, so
the drafts were redone with it.

### 8. Portability

- **`ringo.py wait`:** its liveness check used `os.kill(pid, 0)`, which terminates the process on Windows. On Windows
  it now opens the process and waits on it with a zero timeout instead.
- **Spec runner:** it used `SIGALRM`, which Windows lacks. It now runs without it there; the suite's timeout still
  bounds it.
- **Release bundles:** these left out `spec_runner.py`. It is now in the bundle.
- **CI:** it runs on Ubuntu, macOS and Windows, and the whole offline suite passes natively on Windows. The native
  port locks with `msvcrt`, ends a call's or check's process tree through a job object, and splits backend commands
  without treating `\` in Windows paths as an escape. Tests of POSIX-only properties (permission bits, `os.killpg`,
  FIFOs, the Linux ReviewBench image) are skipped there. Mode 0700 does not apply on Windows: a trace, store or
  release state keeps its parent folder's access list. Under your user profile that list admits you, SYSTEM and
  administrators, so keep run folders out of shared folders.

### 9. Ideas we considered and did not adopt

A deep-research pass checked 25 published claims: 19 were confirmed and 6 refuted. Its verdicts are included below.

| Idea | Evidence | Decision |
|---|---|---|
| High effort on every call | RH: 2.38 credits against RL's 1.49, same quality | Rejected: bloat |
| The Council by default | About 7× tokens, no quality gain | Opt-in |
| Luna as the writer | Cheap, but missed rules and could not write a memo | Rejected |
| Luna reviews and revisions | False findings on 12 of 12 runs; caused the one loss | Rejected: weak models may advise, never change the result |
| Spec tests by default | Never improved a result; wrong tests drove regressions | Opt-in, with dual agreement |
| Two Sol drafts (V2K) | 1.27× AL's credits, with no quality to buy | Opt-in (`candidates`) |
| Shared-primer session forking: a seed session holds the brief, and later calls fork from it to read it from cache | `codex exec fork --ephemeral` reuses the parent's cache routing (Codex CLI 0.164.0); cached input costs 0.05× (estimated 27–34% fewer credits, not measured). A separate seed adds about 10k tokens per memo run. Using the writer as the seed adds the writer's transcript to the reviewer's context | Not adopted: it adds tokens |
| A cheap scout plus a router | The published saving came from a cheap fixer, not from the router | Rejected; Luna as writer already failed here |
| Learned per-step routers | No measurable end-to-end gain; needs a fine-tuned model | Rejected |
| A strong advisor beside a weak executor | Published gains are confounded and inside noise | Deferred |
| Single-pass claim verification | Worked only with a fine-tuned small model | At most a pilot |
| Batching similar checks into one call | About 4× fewer reasoning tokens, in one preprint | At most a pilot |
| Stop after round 1 when the checks pass (`stop_on_checks`) | 1 harm in 162 HumanEval+ runs; bound 2.9% over the 2% bar (section 7) | Removed |
| A reviewer from another model family | Same catches, fewer false alarms, 51% more tokens per review (section 7) | Not adopted |
| Distinguishing inputs from mutated public inputs | Flagged 1 of 8 real hidden failures, with 24 false flags in 150 (section 7) | Not adopted |
| Letting a model rewrite an ambiguous brief directly | Below the original brief in 11 of 12 model-benchmark cells (SpecFix, arXiv 2505.07270) | Not tried: published as harmful |
| Regenerating when the model seems uncertain | Lower pass@1 in 5 of 6 configurations (arXiv 2608.14659; small models) | Not tried: published as harmful |
| Fixed multi-round repair | Repair with a noisy verifier turned correct answers wrong (arXiv 2607.17641); at a matched budget, more first drafts beat more repairs (Olausson et al., ICLR 2024) | Not tried; `max_rounds` stays bounded by the checks |
| Context compaction (shrinking history or material) | Vendors compact with a model call over the whole context, which adds tokens; deterministic masking matched LLM summaries at half the cost on SWE-bench Verified (arXiv 2508.21433). Ringo's pipelines keep no history. Offline, windowing changed files to ±15 lines would cut review material 39%, with untested recall ([details](docs/CONTEXT-COMPACTION.md)) | Not adopted now; not tested |

### 10. What would move it next

- **Tasks with headroom, measured against the agent.** On HumanEval+ the two tie on quality and Ringo costs far less
  (section 7); harder sets (MBPP+, LiveCodeBench) are next.
- **Choosing between two drafts.** Two drafts from different families pass more hidden tests than either alone, but
  the cheap signals tried so far cannot tell which one is right. A published e-process release rule
  (arXiv 2605.12947) and execution-based selection (DiffCodeGen, arXiv 2605.20473) are the starting points; both
  spend far more samples than Ringo, so the question is a gate that pays only when drafts disagree.
- **The IDE.** Two of the largest gains need a person in the loop: asking at the fork (a one-click choice where the
  spec has two readings) and remembering the user's conventions. Both belong in the IDE (see Roadmap).

## Roadmap

- **IDE:** a Rust engine that runs on macOS, with support for Python, Jupyter notebooks, Rust and C/C++. It will
  carry live context, one-click decisions where a spec is ambiguous, and per-repository preference memory. It will be
  a separate client of this harness.
  Its local model is meant to do more than complete code: keep Claude, Codex and other agents supplied with current
  facts about the code, so they spend fewer tokens finding them, and audit a codebase in the background, proposing
  small fixes that pass the project's checks before the user approves them. None of it is built or measured yet.
- **Native Windows model calls:** the offline suite passes natively; real Claude and Codex calls still need a run.
- **Audit-Gated Continual Learning ([AGCL](docs/AGCL.md)):** Ringo learns when it can stop spending tokens, and it
  changes behavior only through a release with a measured harm bound and human approval. Built: `shadow_labels.py`
  and the HumanEval+ studies in `evalplus_study.py`. The first candidate, stopping when the checks pass, was rejected
  by its sealed study; a new candidate needs a new study on untouched tasks.
- **Benchmarks with headroom,** so quality gains can be measured.

## Engineering optimization goals

Optimize the harness for working code, sound design judgment and architectural
insight, with concrete task outcomes and separate evidence for each. Whole-task
token efficiency remains an objective while correctness and quality hold. See
[engineering objectives](docs/ENGINEERING-OBJECTIVES.md) for task families,
measurement, Council responsibilities and the gaps in today's review-only
evaluator. This direction does not start an evaluation or improvement campaign.

## Evaluation and improvement

See [the harness guide](docs/HARNESS.md) and [the agent skill](skills/ringo-harness/SKILL.md).

The [improvement log](docs/IMPROVEMENT-LOG.md) records each measured improvement,
when it was measured and the change that made it. Every number in it is recomputed
by `improvement_log.py` from raw evidence committed under `docs/improvements/`, and
an entry whose evidence does not show the improvement is refused by the tests.

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
into this CLI. A `work` task that opts into the Council supplies an external
council bridge command and rubric explicitly: the council scores the replies it is
given and makes no model calls, while Ringo makes the writer and judge calls and
owns their budget.
Existing evaluation stores must be re-frozen for this source layout; do not reuse
old digests or copy sealed data into this repository.

No performance or quality gain is claimed from this repository extraction.

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
Basic integration exists: `work` combines generated files, fixed checks and an
opt-in council assessment. The research workflow those requirements describe is future work, and
the current harness has not been validated for that workload.

## License

Ringo is licensed under the [Apache License, Version 2.0](LICENSE).

# The `ringo` shell

Type `ringo` in a terminal and Ringo opens its own session, the way `claude` opens Claude Code. From that one
terminal you route work to the coding agents you already use (OpenAI Codex CLI, Claude Code, xAI Grok Build), run
Ringo's own pipelines with them, and watch every job and subagent in a live panel.

The shell is a small Rust program (`cli/`). Ringo's pipelines (`review`, `design`, `work`) stay in Python and run
unchanged, so the measured results in the [README](../README.md#experiments-what-we-tried-what-broke-what-we-measured)
still describe what they do.

## Install

You need Rust (once), git, Python 3.11+ for the pipelines, and at least one agent CLI.

```sh
# macOS:    brew install rust git python          (or: curl https://sh.rustup.rs -sSf | sh)
# Linux:    curl https://sh.rustup.rs -sSf | sh    (plus git and python3 from your package manager)
# Windows:  winget install Rustlang.Rustup Git.Git Python.Python.3.12
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
cargo install --path cli      # puts `ringo` in ~/.cargo/bin; rustup adds that to PATH
ringo
```

`ringo` runs the pipelines from the checkout it was built from. If you move the checkout, set `RINGO_HOME` to its
new path. On macOS and Windows `Ringo` also works, because their file systems ignore case; on Linux add
`alias Ringo=ringo` to your shell profile.

Windows: the shell builds and the pipelines run, but job cancellation and the agent fakes in the tests are only
exercised on macOS and Linux so far. Use WSL 2 if you hit a problem.

## First run

The first `ringo` asks which agents you use:

```
Welcome to Ringo.
Type the command you start an agent with, as you would in a terminal (codex, claude or grok).
agent> codex
  codex: installed ✓  signed in ✓ (ChatGPT)
Would you like to add another agent [y/n] y
agent> claude
  claude: installed ✓  not signed in (not signed in).
  Starting `claude auth login`: finish signing in, then you come back here.
  claude: installed ✓  signed in ✓ (claude.ai, max)
Would you like to add another agent [y/n] n
Set up: codex, claude. Next time just type ringo.
```

For each agent Ringo checks it is installed (and prints the official install command if not) and signed in, using
the agent's own status command, which makes no model call: `codex login status`, `claude auth status`. When an
agent is not signed in, Ringo runs that agent's own sign-in (`codex login`, `claude auth login`, or `grok`'s
browser sign-in) in your terminal. Ringo uses subscription logins only: it removes API-key variables from every
agent it starts, forces Codex's ChatGPT login, and stops a Claude turn that reports an API key. `/setup` in the
shell, or `ringo setup`, adds another agent later.

## Using it

```
 ringo  my-repo (main)  ·  2 running  ·  48.1k tokens this session  ·  pipelines: codex-chatgpt
┌ agents ───────────────────────────────────────────────────────────────────────────────────┐
│#1  codex!       ✓   0:41   34.8k  [change ready] Created the retry wrapper and its test.  │
│#2  claude       ●   0:12   13.3k  Grep fn retry                                           │
│     └ Explore: find callers           ● Read client.rs                                    │
│#3  ringo review ●   0:05       0 1c  review: calling codex-chatgpt                        │
└───────────────────────────────────────────────────────────────────────────────────────────┘
┌ #2 where else do we retry HTTP calls? ────────────────────────────────────────────────────┐
│ ...the selected job's live log...                                                         │
└───────────────────────────────────────────────────────────────────────────────────────────┘
┌ @codex · Enter sends · Tab jobs · /help ──────────────────────────────────────────────────┐
│ /review #1                                                                                │
└───────────────────────────────────────────────────────────────────────────────────────────┘
```

| You type | What happens |
|---|---|
| `/review #N` | Ringo reviews job N's change, with its task as the change description |
| `/review BASE [HEAD]` | Ringo reviews `BASE...HEAD` of the repository |
| `/design TEXT` | Ringo's one-call design pipeline on `TEXT` |
| `/work SPEC.json` | Ringo's checked work loop on a task spec |
| `@codex TASK` | an agent turn that may read the repository but not change it; the agent's conversation continues across these turns |
| `@codex! TASK` | an agent turn that may edit, in a private git worktree from `HEAD`; nothing reaches your tree until `/apply` |
| `@codex @claude TASK`, `@all TASK` | the same task to several agents at once |
| `{#N}` inside a task | hands job N's reply (and its change) to the agent; the shell shows exactly what was added |
| `/diff N`, `/apply N`, `/drop N` | see a write job's change, apply it to your clean tree (uncommitted), remove its worktree |
| `/show N`, `/kill N`, `/use NAME`, `/backend SPEC`, `/setup`, `/quit` | select a job, stop it with everything it started, set the default agent or pipeline backend, add an agent, quit |

A line without `/` or `@` goes to the default agent (the first one you set up). Tab and Shift-Tab select a job,
PgUp and PgDn scroll its log, Up and Down recall earlier lines, and Ctrl-C twice quits, stopping running jobs.

The panel shows each job's state, time, tokens, Ringo model calls and what it is doing now, with one row per
subagent: Claude Code's Task subagents and Codex's spawned agents. When two unapplied write jobs edit the same
file, both rows say so (`⚠ same files as #2 client.rs`) before you apply either. That check costs no tokens: it
compares the agents' own edit events as they arrive.

## Colours

The default theme, `siamese`, takes its palette from a seal-point Siamese: cream and fawn for text and frames, seal
brown for the header bar, and sapphire, the eye colour, for whatever is running and for the line you type in. Each
agent keeps one colour (Codex mint, Claude clay, Grok silver), finished jobs are sage, failures rose, and the
same-files warning amber.

| Theme | For |
|---|---|
| `siamese` | dark terminals (default) |
| `siamese-light` | light terminals |
| `midnight` | dark terminals, navy and violet |
| `mono` | no colour: bold, dim and reverse only |

`/theme NAME` switches and remembers the choice; `RINGO_THEME=NAME` overrides it for one start. Ringo uses 24-bit
colour when the terminal says it supports it (`COLORTERM=truecolor`, iTerm2, WezTerm, Ghostty, kitty, Alacritty,
Windows Terminal, VS Code) and the nearest of the 256 standard colours otherwise. `NO_COLOR` (https://no-color.org) or
`TERM=dumb` turns colour off. The theme is fitted once at start; running jobs show a spinner, redrawn about eight
times a second only while something runs (measured under 0.1% CPU with two jobs running; the shell idles at 2.5 MB).

## What it costs, and why the defaults are what they are

Ringo's measured advantage is that it sends models less: on HumanEval+ the pipeline passed 147 of 154 tasks at 3,500
tokens per task, against 148 at 50,325 for the stock Codex agent with the same model. The shell keeps that path one
command away (`/review`, `/design`, `/work`) and makes full agent turns explicit, with their tokens on screen. Agent
turns on repository-level work have not been measured against the pipelines yet.

Measured while building the shell (2026-10-29, one run each, trivial prompts):

| | Tokens |
|---|---|
| Claude Code turn with the owner's normal setup (138 tools, 21 MCP servers, CLAUDE.md, plugins) | 50,201 |
| the same turn with `--safe-mode`, which the shell uses | 16,567 |
| a Codex agent turn, first / resumed | 16,834 / 34,845 |
| a Claude agent turn, first / resumed | 16,501 / 33,056 |

So the shell starts Claude in safe mode (your CLAUDE.md, skills, plugins, hooks and MCP servers are left out; sign-in,
built-in tools and subagents work as usual) and appends the repository's own `CLAUDE.md` or `AGENTS.md`. A resumed
turn re-sends the conversation, so a follow-up costs about twice a first turn; start a write job (`@codex!`) for an
unrelated task instead of continuing a long read conversation. Nothing is shared between agents unless you write
`{#N}`. (`--bare` would cut more, but it signs in with an API key only, so the shell does not use it.)

The program itself: a 1.0 MB binary that starts in 2.4 ms and idles at 2.5 MB of memory with no measurable CPU,
against 35.5 ms and 24 MB for `python3 ringo.py --help` (macOS, Apple silicon). It redraws only on an event, a key,
or for the spinner while a job runs.

## Safety model

- **Read turns:** Codex runs with its `read-only` sandbox. Claude runs with `--permission-prompts none` and deny
  rules for `Bash`, `Edit`, `Write` and `NotebookEdit`, which win over any allow rules in your own settings. Grok has
  no read-only mode Ringo has verified, so it only runs write jobs. Checked live: asked in a read turn to create a
  file, both refused and no file appeared, but trying cost Claude 98k tokens and Codex 40k. Use `@agent!` when you
  want edits.
- **Write turns** run in a worktree under Ringo's private state folder: Codex with `workspace-write` (its sandbox allows commands in
  the worktree), Claude with `acceptEdits` and a deny rule for `Bash` (edits allowed, shell commands refused: Claude
  has no sandbox here), Grok with `--always-approve` (every tool call approved, so
  it is not contained; its flags come from xAI's documentation and are untested here). A worktree keeps parallel
  edits apart; it is not a security boundary.
- **Applying:** `/apply` needs a finished job and a clean tree, checks the patch first, and applies it with
  `git apply --3way` without committing. A conflict leaves your tree and the job untouched.
- **Stopping:** `/kill` stops the job's whole process group. A Ringo pipeline is interrupted first, so Python's own
  cleanup ends its model calls, which run in sessions of their own.
- **No recursion:** agents run with `RINGO_CHILD=1`, and both `ringo` and `ringo.py` refuse to start under it.
- **Fail closed:** a line that is not a JSON event, a failed turn, a missing completion record or an empty reply
  fails the job; it is never shown as done.
- **Limits:** three jobs at a time, and one turn at a time per agent conversation; more is refused, not queued.
- **State:** `~/.local/share/ringo/shell/` (or under `RINGO_RELEASE_HOME`), owner-only. Each job folder keeps its
  task, raw events, reply, stderr and patch. Logs can contain your code: delete them yourself when done.

## For the IDE

The shell is a thin front end over the `ringo_cli` library: `Session` launches and stops jobs, `pump()` folds their
events into `Job` records (status, activity, tokens, subagents, touched files), and `diff` / `apply` / `drop_worktree`
/ `overlaps` handle changes. A Rust IDE links the library and renders the same records.

## Not yet

A JSON-lines mode for a non-Rust front end, line editing beyond the basics, a model-chosen router, and measured
repository-level comparisons of agent turns against the pipelines.

# The `ringo` shell

Type `ringo` in a terminal and Ringo opens its own session, the way `claude` opens Claude Code. From that one
terminal you route work to the coding agents you already use (OpenAI Codex CLI, Claude Code, xAI Grok Build), run
Ringo's own pipelines with them, and watch every job and subagent in a live panel.

The shell is a small Rust program (`cli/`). Its `/review`, `/design` and `/work` start the same binary, which runs
Ringo's pipelines natively, sending the same requests as the Python runtime (see the
[README](../README.md#the-native-runtime-and-why-it-moved-to-rust)), or hands them to `ringo.py` when your release
state says so.

## Install

You need Rust (once), git, at least one agent CLI, and Python 3.11+ for `work` checks on Python tasks and for
reviews of Python changes.

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
╭ agents ─────────────────────────────╮╭ #2 where else do we retry HTTP calls? ─────────────────╮
│#1 codex!              ✓ 0:41 34.8k  ││ ...the selected job's live log, or the console...       │
│   [change ready] Created the retry… ││                                                         │
│#2 claude              ⠼ 0:12 13.3k  ││                                                         │
│   Grep fn retry                     ││                                                         │
│   └ ⠼ Explore: find callers: Read … ││                                                         │
│#3 ringo review        ⠼ 0:05 0 1c   │╰─────────────────────────────────────────────────────────╯
│   review: calling codex-chatgpt     │╭ @codex · Enter sends · Tab jobs · /help ────────────────╮
│                                     ││ /review #1                                              │
╰─────────────────────────────────────╯╰─────────────────────────────────────────────────────────╯
```

From 90 columns wide, the agents sit in a column on the left and the chat beside them. On a narrower terminal the
panel moves above the chat, one row per job.

| You type | What happens |
|---|---|
| `TEXT` (no `/` or `@`) | a question: one tool-free call with your last few questions and answers as context; it cannot see your files. Measured: about 1,300 tokens for a design question that cost about 19,700 as a Claude Code turn |
| `/review #N` | Ringo reviews job N's change, with its task as the change description |
| `/review BASE [HEAD]` | Ringo reviews `BASE...HEAD` of the repository |
| `/design TEXT` | Ringo's one-call design pipeline on `TEXT` |
| `/research TOPIC` | Ringo's research role: one live web search call, every quoted source fetched and its quote checked in code, one write call that cites only verified evidence ([details](RESEARCH-ROLE.md)). Measured once at about 68,000 tokens, most of them search results |
| `/work SPEC.json` | Ringo's checked work loop on a task spec |
| `@codex TASK` | an agent turn that may read the repository but not change it; the agent's conversation continues across these turns |
| `@codex! TASK` | an agent turn that may edit, in a private git worktree from `HEAD`; nothing reaches your tree until `/apply` |
| `@codex @claude TASK`, `@all TASK` | the same task to several agents at once |
| `{#N}` inside a task | hands job N's reply (and its change) to the agent; the shell shows exactly what was added |
| `/diff N`, `/apply N`, `/drop N` | see a write job's change, apply it to your clean tree (uncommitted), remove its worktree |
| `/route on` | route suggestions: a line that reads like a research, design, review or edit task, or a question about this folder's files, is not sent; the matching command (`/research ...`, `/design ...`, `/review ...`, `@codex! ...`, or `@codex ...` for a read-only look at the files) appears in the input line with what it costs, for you to send with Enter, change, or ask as a plain question with Esc. Word rules only, no model call; an agent turn is only ever suggested. On 100 held-out lines the suggestions were acceptable 35 times in 38 and covered about half of the tasks; the rest are asked as plain questions. `/route off` turns it off |
| `/show N`, `/kill N`, `/backend SPEC`, `/theme NAME`, `/setup`, `/quit` | select a job, stop it with everything it started, set the backend for questions and pipelines, change colours, add an agent, quit |

Start `ringo` in any folder, as you would `claude` or `codex`. Inside a git project the agents can also edit
(`@agent!`, in a worktree) and `/review BASE` reviews its commits; in any other folder, a repository that holds your
home folder (a dotfiles repository) included, they read and answer, and Ringo says why an edit needs a project.
Ringo's private state stays out of every repository; its folder carries a `.gitignore` of its own.

Use `@codex` or `@claude` when the answer needs your files: an agent turn costs about 17,000 tokens before it
starts, and a vague question can send it searching (one such follow-up cost about 450,000). Tab and Shift-Tab select a job,
PgUp and PgDn scroll its log, Up and Down recall earlier lines, and Ctrl-C twice quits, stopping running jobs.

The panel shows each job's state, time, tokens, Ringo model calls and what it is doing now, with one row per
subagent: Claude Code's Task subagents and Codex's spawned agents. When two unapplied write jobs edit the same
file, both rows say so (`⚠ same files as #2 client.rs`) before you apply either. That check costs no tokens: it
compares the agents' own edit events as they arrive.

### Agent updates

When the shell opens it says, in the background, which agents changed version since the last session and which have
a newer release out, with the command that updates that install (`brew upgrade --cask codex`, `claude update`, or
`npm install -g ...@latest`). Ringo never updates an agent itself: a new version of the engine can change answers and
token use, so it is your call when to take one. The release lookup asks the Homebrew or npm registry at most once a
day (no model call, no account); `RINGO_NO_UPDATE_CHECK=1` turns it off. Versions are kept in
`engine-versions.json` in Ringo's state folder. Grok is only reported when its version changes: it has no public
release feed.

## Colours

The default theme, `siamese`, takes its palette from a seal-point Siamese: cream and fawn for text and frames, seal
brown for the header bar, and sapphire, the eye colour, for whatever is running and for the line you type in. Each
agent keeps one colour (Codex mint, Claude clay, Grok silver), finished jobs are sage, failures rose, and the
same-files warning amber.

| Theme | For |
|---|---|
| `siamese` | dark terminals (default) |
| `siamese-light` | light terminals |
| `pink` | dark terminals, blush and berry |
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

The program itself: a 3.2 MB binary (the pipelines included) that starts in 2.1 ms and idles at 2.5 MB of
memory with no measurable CPU. A native pipeline peaks at 8.1 MB, against 24.5 MB for the same run under Python
(macOS, Apple silicon). It redraws only on an event, a key,
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

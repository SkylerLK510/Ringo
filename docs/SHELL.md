# The `ringo` shell

Type `ringo` in a terminal and Ringo opens its own session, the way `claude` opens Claude Code. From that one
terminal you route work to the coding agents you already use (OpenAI Codex CLI, Claude Code, Google Antigravity CLI,
xAI Grok Build, or one you add), run
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
Type the command you start an agent with, as you would in a terminal (codex, claude, grok, agy).
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
agent is not signed in, Ringo runs that agent's own sign-in (`codex login`, `claude auth login`, or the browser
sign-in `grok` and `agy` open on their first start) in your terminal. Ringo uses subscription logins only: it removes API-key variables from every
agent it starts, forces Codex's ChatGPT login, and stops a Claude turn that reports an API key. `/setup` in the
shell, or `ringo setup`, adds another agent later.

### Agents Ringo does not ship

Ringo never runs a program to find out what it is. A name that is not a program on your PATH gets
"`tle` is not an agent: nothing called tle is installed." An installed program Ringo has no entry for gets
"`git` is a program, but not an agent Ringo knows.", then the offer to add it: you give the command that runs one
prompt and prints the answer, with `{prompt}` where the prompt goes and `{cwd}` for the folder (without `{prompt}`,
the prompt goes on stdin; quotes group words, as in a shell, but no shell runs it). An agent you add takes `@name!` write turns in its own worktree, with its reply read as
plain text; it has no read turns, because Ringo cannot verify a read-only mode for it.

Your entries live in `agents.json` in Ringo's state folder, beside the list of agents you set up, and you can edit
them there:

```json
{"agents": ["codex", "claude", "mytool"],
 "custom": [{"name": "mytool", "args": ["run", "--prompt", "{prompt}", "--dir", "{cwd}"],
             "company": "My tool", "install": "pipx install mytool"}]}
```

Fields: `name` (letters, digits, `-`, `_`, `.`), `program` (default: the name; a path works), `args`, `company`,
`install`, `login` (its sign-in arguments), `npm` (a package to check for updates), and `adapter`: `plain`
(default), or `codex`, `claude` or `agy` for another install of one of those (say, a second Claude Code at another
path), which then gets that agent's own write turns, and its read turns with `"read": true`. The agents Ringo ships are the same kind of entry,
in `cli/src/agents.json`; how each protocol is spoken stays in code.

### Who takes which turn

Codex owns the abstract work: Ringo's pipelines (`/research`, `/design`, `/review`) and plain asks run on it.
Claude owns writing and reading code: with `/route on`, an edit line is suggested as `@claude! ...` and a question
about the files as `@claude ...`. The preference is soft. When Claude is not set up, or a read turn finds its
conversation busy, the suggestion names Codex, then any other agent that can take the turn; typing `@codex`,
`@agy` or any `@agent` yourself always picks for you.

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
| `TEXT` (no `/` or `@`) | a question: one tool-free call with your last few questions and answers as context; it cannot see your files. Measured: about 1,300 tokens for a design question that cost about 19,700 as a Claude Code turn. When the call reads the line as a task (files to change, read or review, or live sources) it answers with one `ROUTE:` line instead, and the shell starts the task: an agent turn (claude, or codex when claude is not set up) or `/research`; a review is put in the input line for you to say what to review. Live: "fix the off-by-one in counter.py and add a test for it" came back `ROUTE: edit` in 3.2 s for 892 tokens |
| `/mode ask\|auto\|read`, `/auto`, Shift-Tab | how far every agent may act without asking (see [Modes and permissions](#modes-and-permissions)); `/auto` toggles auto and ask, Shift-Tab cycles ask, auto, read. Remembered for next time |
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
| `/route on` | route suggestions: a line that reads like a research, design, review or edit task, or a question about this folder's files, is not sent; the matching command (`/research ...`, `/design ...`, `/review ...`, `@claude! ...`, or `@claude ...` for a read-only look at the files; see "Who takes which turn") appears in the input line with what it costs, for you to send with Enter, change, or ask as a plain question with Esc. Word rules only, no model call; an agent turn is only ever suggested. On 100 held-out lines the suggestions were acceptable 35 times in 38 and covered about half of the tasks; the rest are asked as plain questions. `/route off` turns it off |
| `/show N`, `/kill N`, `/backend SPEC`, `/theme NAME`, `/setup`, `/quit` | select a job, stop it with everything it started, set the backend for questions and pipelines, change colours, add an agent, quit |

Start `ringo` in any folder, as you would `claude` or `codex`. Inside a git project the agents can also edit
(`@agent!`, in a worktree) and `/review BASE` reviews its commits; in any other folder, a repository that holds your
home folder (a dotfiles repository) included, they read and answer, and Ringo says why an edit needs a project.
Ringo's private state stays out of every repository; its folder carries a `.gitignore` of its own.

Use `@codex` or `@claude` when the answer needs your files: an agent turn costs about 17,000 tokens before it
starts, and a vague question can send it searching (one such follow-up cost about 450,000). Tab selects a job, Shift-Tab changes the mode,
PgUp and PgDn scroll its log, Up and Down recall earlier lines, and Ctrl-C twice quits, stopping running jobs.

The panel shows each job's state, time, tokens, Ringo model calls and what it is doing now, with one row per
subagent: Claude Code's Task subagents and Codex's spawned agents. When two unapplied write jobs edit the same
file, both rows say so (`⚠ same files as #2 client.rs`) before you apply either. That check costs no tokens: it
compares the agents' own edit events as they arrive.

## Modes and permissions

One switch sets how far every agent may act without asking; it applies to the turns started after it changes.

| Mode | Claude | Codex |
|---|---|---|
| `ask` (the default) | reads, and edits its worktree, freely (Claude Code's own rules: plain file commands such as `mkdir` in the worktree run without asking); every other action waits for you: a command such as `curl`, `gh` or `npm` in a write turn, a connector call, a file outside its folder. Read turns (`@claude`) run in your own tree, so they never run commands; give a task that needs them to `@claude!` | works inside its sandbox (read-only for read turns, its worktree for write turns, no network); it cannot stop to ask, so what the sandbox refuses fails |
| `auto` | Claude Code's auto mode: it acts on its own and its safety review blocks what is dangerous; what that review blocks repeatedly, and what an always-ask rule names, waits for you | write turns run with `--approve-for-me` (Codex's own reviewer answers its approval requests) and the network on in the sandbox, for git, gh and installs |
| `read` | no edits, no commands, no connectors; `@agent!` is refused | read-only sandbox; `@agent!` is refused |

When an agent waits, a box above the input line shows the whole action, every line of a command included, and
`y` allows it once, `a` allows it and the same rule for the rest of the session (in every later Claude turn too, as
`--allowedTools`; an always-ask action is allowed once only, whichever key), `n` or Esc declines it and the agent
carries on without it. A request longer than its box shows its first lines and how many more there are; all of it is
also in the job's log (Tab to the job, PgUp). The keys answer only when the input
line is empty and not in the first 0.3 s after the box appears, so a word you are typing never answers by accident.
Tab, PgUp and PgDn still let you look at the job first. Every answer is kept in `actions.jsonl` in the session's
state folder, with the job, agent, action, rule and mode.

**Always ask.** In ask and auto mode alike, these wait for you whatever an allow rule or a safety review says (ask
rules win over allow rules): `sudo`, `dd`, `mkfs`, `diskutil`, `git push --force` and `-f`, `gh release`,
`gh repo create` and `delete`, `gh gist create`, and edits to `~/.claude`, `~/.codex` and `~/.gemini`. Claude's own
auto-mode review blocks more by default: force pushes however written, destroying files that existed before the
session, downloading and running code, sending data out, production deploys. Add your own rules, in Claude's
permission-rule syntax, as `{"ask": ["Bash(terraform apply:*)"]}` in `permissions.json` in Ringo's state folder.
These rules are Claude's: Codex has no per-run rule list Ringo can pass, so its guard is its sandbox (it writes only
in its worktree) and, in auto mode, its own reviewer.

**Connectors.** A Claude turn whose task names a service only your connectors reach (Jira, Confluence, ClickUp,
Gmail, email, calendar, Google Drive or Docs, Slack, Linear, Notion, Figma, Datadog, Intercom) runs with your MCP
servers and plugins loaded, but without your CLAUDE.md, memory, skills or main agent; any other turn runs in safe
mode without them. Measured with Claude Code 2.1.316, one trivial turn each: 16,567 tokens in safe mode, 27,522 with
the connectors, 57,799 with everything. Without your CLAUDE.md, an instruction in it to use Ringo cannot start Ringo
again either. Your own hooks do run in these turns, as in your normal Claude Code. Checked live through the same
flags: a read-only Jira question asked before each of its two Jira calls
and answered correctly (5 model calls, 18.5 s, 167,291 tokens, most of them cache reads).

**Checked live** (2026-10-31, Claude Code 2.1.316, codex-cli 0.165.0, one run each in a throwaway repository):

| Check | Result | Tokens |
|---|---|---|
| ask mode, Claude: `curl -sI https://example.com \| head -1` | waited, allowed once, `HTTP/2 200` | 38,195 |
| auto mode, Claude: `mkdir -p made && ls`, then `sudo -n true` | `mkdir` ran without asking; `sudo` waited, was declined, Claude carried on | 57,691 |
| auto mode, Codex: the same `curl` | `HTTP/2 200` (in ask mode its sandbox has no network) | 35,326 |

**What the modes cost.** One trivial Claude turn each, the shell's flags before and after (2026-10-31, Claude Code
2.1.316, one run each):

| Turn | Before | Ask mode | Auto mode |
|---|---|---|---|
| read turn (`@claude`) | 16,547 | 16,547 | 16,547 |
| write turn (`@claude!`) | 17,139 | 17,133 | |

The first version cost 18,113 for the read turn: with a permission host, Claude Code also offers `AskUserQuestion`,
`EnterPlanMode` and `ExitPlanMode`, which Ringo does not serve, so the shell denies them. The always-ask rules add
nothing. Times ranged from 1.4 s to 3.8 s across these single runs with no pattern by mode, so no latency change can be
claimed either way. Claude Code does not report its auto-mode review's own calls, so whether they count against a
plan's usage is not known.

Not yet: Codex cannot stop to ask, so it does not take ask mode's prompts. Its app-server protocol has the same
three answers (`accept`, `acceptForSession`, `decline` for command, file-change and permission approvals) and could
bring them, but it is marked experimental. agy keeps the deny rules described under Safety model.

### Agent updates

When the shell opens it checks, in the background, which agents changed version since the last session and which
have a newer release out, and says so. `/update` installs those releases for you: the shell steps aside, runs the
update that matches how each agent was installed (`brew upgrade --cask codex`, `npm install -g @openai/codex@latest`,
`claude update`, or the vendor's install script), then comes back and reports the new versions. No model call is made
and no tokens are used. `/update auto on` makes the shell run Homebrew, npm and `claude update` updates by itself
when it opens (output in `engine-update-NAME.log` in Ringo's state folder); install-script updates still wait for
`/update`. It is off by default because a new engine version can change answers and token use, and benchmark
results are only comparable on the same version. An update replaces the agent's program, so `/update` waits while
jobs run, an automatic update starts only when none is running, and no job starts until it finishes. An update
that fails or brings no newer version stays on the list for the next `/update`. Claude Code installed with its own installer already updates itself; the shell reports when that happened.

The release lookup asks the Homebrew or npm registry about each agent at most once a day (no account);
`RINGO_NO_UPDATE_CHECK=1` turns it off. Versions are kept in `engine-versions.json` in Ringo's state folder. Grok and
agy are only reported when their version changes: Ringo knows no public release feed for them (agy updates itself
with `agy update`).

## Colours

The default theme, `siamese`, takes its palette from a seal-point Siamese: cream and fawn for text and frames, seal
brown for the header bar, and sapphire, the eye colour, for whatever is running and for the line you type in. Each
agent keeps one colour (Codex mint, Claude clay, every other agent silver), finished jobs are sage, failures rose, and the
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
| a Codex agent turn, first / one-word follow-up | 16,834 / 18,011 |
| a Claude agent turn, first / one-word follow-up | 16,501 / 16,555 |
| an agy turn, first / one-word follow-up (2026-10-31, agy 1.4.12) | 7,064 / 7,314 |

This table first listed the follow-ups as 34,845 and 33,056 tokens, which were running totals, not what the
follow-ups used. A resumed Codex or agy turn reports its whole conversation's usage so far, and the shell had shown
that number as the turn's own. It now counts only what each turn adds. Claude reports each run on its own; its
33,056 was the two turns added together.

So the shell starts Claude in safe mode (your CLAUDE.md, skills, plugins, hooks and MCP servers are left out; sign-in,
built-in tools and subagents work as usual), unless the task names a connector (see Modes and permissions), and
appends the repository's own `CLAUDE.md` or `AGENTS.md`. A follow-up
re-sends the conversation so far, so it costs a first turn plus that history. A short exchange adds little, but a
long read conversation grows with every turn, so start a write job (`@codex!`) for an unrelated task. Nothing is
shared between agents unless you write `{#N}`. (`--bare` would cut more, but it signs in with an API key only, so
the shell does not use it.)

agy's floor is its own: it declares 60 tools, about 20 of them for the browser, whatever the deny rules, mode or
sandbox. Its startup event lists them, and no setting Ringo can pass removes them.

The program itself: a 3.2 MB binary (the pipelines included) that starts in 2.1 ms and idles at 2.5 MB of
memory with no measurable CPU. A native pipeline peaks at 8.1 MB, against 24.5 MB for the same run under Python
(macOS, Apple silicon). It redraws only on an event, a key,
or for the spinner while a job runs.

## Safety model

- **Permission prompts:** Claude's prompt is one stream-json message on stdin, which stays open until the turn's
  result: Claude Code sends each permission request to Ringo (`--permission-prompt-tool stdio`, the channel the
  Agent SDK uses; `--permission-prompts host` alone denies them) and Ringo writes your answer back. A request Ringo
  does not serve is refused at once, so a turn never waits on it. Codex and agy cannot ask.
- **Read turns:** Codex runs with its `read-only` sandbox. Claude runs with deny rules for `Bash`, `Edit`, `Write`
  and `NotebookEdit`, which win over any allow rules in your own settings (it works in your own tree with no sandbox,
  and a command could write files there); its connector calls go through the mode, and in read mode nothing may
  prompt (`--permission-prompts none`). agy runs
  with session-only deny rules for `write_file(*)`, `command(*)`, `unsandboxed(*)`, `mcp(*)` and
  `execute_url(*)`, which win over its accept rules; asked to edit, it gets an error it can see and answers anyway
  (checked live with agy 1.4.11: no file appeared). Grok and agents you add have
  no read-only mode Ringo has verified, so they only run write jobs. Checked live: asked in a read turn to create a
  file, both refused and no file appeared, but trying cost Claude 98k tokens and Codex 40k. Use `@agent!` when you
  want edits.
- **Write turns** run in a worktree under Ringo's private state folder: Codex with `workspace-write` (its sandbox allows commands in
  the worktree; in auto mode `--approve-for-me` and the network), Claude with `acceptEdits` in ask mode and auto mode
  in auto mode (edits allowed; any other command waits for you, or for its review), agy with `--mode accept-edits` and the same deny rules minus `write_file` (edits allowed;
  commands, MCP tools and browser actions refused, as for Claude), Grok with `--always-approve` (every tool call approved, so
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

# The `ringo` shell

Type `ringo` in a terminal and Ringo opens its own full-screen session, the way `codex` opens Codex. You say what
you want in plain words and the agent you are talking to (Claude Code, OpenAI Codex CLI, Google Antigravity CLI, xAI
Grok Build, or one you add) does it in that folder, asking before it runs a command; `/undo` takes a turn's change
back. From the same screen you give a task to several agents at once, each in its own copy, run Ringo's own
pipelines with them, and watch every job and subagent as it runs.

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
the prompt goes on stdin; quotes group words, as in a shell, but no shell runs it). An agent you add works only in its own copy of the folder (a line for it goes there), with its reply read as plain
text, because Ringo cannot verify a sandbox for it in your folder.

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
path), which then works in copies as that agent does, and in your folder too with `"read": true`. The agents Ringo ships are the same kind of entry,
in `cli/src/agents.json`; how each protocol is spoken stays in code.

### Who takes which turn

Claude owns writing and reading code: a plain line goes to Claude when it is set up, else to Codex, else to any
agent that works in the folder. `@codex` (or any `@agent`) switches who you talk to, and later plain lines go there
too, each agent keeping its own conversation. Codex owns the abstract work: Ringo's pipelines (`/research`,
`/design`, `/review`) and `/ask` run on it.

## Using it

```
╭──────────────────────────────────────────────────╮
│ ✻ Welcome to Ringo                               │
│                                                  │
│ folder  ~/code/my-repo                           │
│ agents  claude, codex (/setup adds another)      │
│ mode    ask (Shift-Tab changes it)               │
╰──────────────────────────────────────────────────╯

› add a retry to the HTTP client

#1 claude
  • Grep fn send in src · Found 2 files
  • Read client.rs · 214 lines
  • Edit client.rs
  • $ cargo test retry

  Added a retry with backoff to `send` in **client.rs**, and a test for it.

✓ #1 claude done · 0:41 · 34.8k tokens · changed client.rs (/undo takes it back)

╭──────────────────────────────────────────────────────────────────────────╮
│ › tell claude what you want, in your own words · / commands · ? keys     │
╰──────────────────────────────────────────────────────────────────────────╯
 ringo  my-repo (main) · ask mode · talking to claude · 34.8k tokens   ? keys
```

The shell fills the terminal (the alternate screen, as Codex's does). The conversation takes the top: your lines,
every reply, and what each agent does under its job number: one `•` row per tool call or command once it has run,
with what came of it (how many files a search found, how many lines a read took, an error, a failed command's exit
code; a step too long for the row loses the end of its pattern or command to `…`, and `/show N` has it whole), then
one line when a job ends with its time, tokens and Ringo model calls, and the files it changed. Replies render their
markdown: headings, emphasis and code styled, lists and quotes wrapped under their own text, and tables as aligned
columns (a long last column wraps under itself), or, when even that does not fit, as one block per row with each cell
under its column's name. Everything is fitted again when the window's width changes. The mouse wheel and PgUp/PgDn
scroll it, and it stays where you scrolled while new lines arrive below; typing goes back to the end. A drag over
the conversation selects its text alone (never the input or the status line) and copies it when you let go, with
the system's own clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip`) or, where there is none, such as a
server over ssh, by asking the terminal (OSC 52). `/copy` copies the last answer. Below the conversation are the
running jobs, the lines queued behind the current turn, a permission request or the input box, and a status line.
When you quit, the conversation is saved as `conversation.txt` in the session's state folder and the shell prints
where. The manual, the step-by-step [user guide](USER-GUIDE.md), is not on screen until you ask for it: `/help` (or
`/usrman`) prints it into the conversation, followed by the settings in force, and `?` prints just its keys.

| You type | What happens |
|---|---|
| `TEXT` (no `/` or `@`) | a turn of the agent you are talking to, in this folder, continuing its conversation: it reads, changes files there, and asks before anything else (in `ask` mode). One turn at a time works in the folder: a line typed while one runs waits under the input and goes when it ends (Esc stops the turn and gives the waiting lines back to the input) |
| `/undo` | the files the latest turn here changed go back as they were before it; each `/undo` goes one turn further back. A file you edited since makes it refuse and touch nothing |
| `/ask TEXT` | a quick answer: one tool-free call with your last few `/ask` questions and answers as context; it cannot see your files. Measured: about 1,300 tokens for a design question that cost about 19,700 as a Claude Code turn. When the call reads the line as a task (files to change, read or review, or live sources) it answers with one `ROUTE:` line instead, and the shell gives it to the agent you are talking to, or to `/research`; a review is put in the input line for you to say what to review |
| `/clear` | a new conversation: each agent's next turn starts fresh |
| `/mode ask\|auto\|read`, `/auto`, Shift-Tab | how far every agent may act without asking (see [Modes and permissions](#modes-and-permissions)); `/auto` toggles auto and ask, Shift-Tab cycles ask, auto, read. Remembered for next time |
| `/review #N` | Ringo reviews job N's change (a copy's, or a turn's here from its before and after), with its task as the change description |
| `/review BASE [HEAD]` | Ringo reviews `BASE...HEAD` of the repository |
| `/design TEXT` | Ringo's one-call design pipeline on `TEXT` |
| `/research TOPIC` | Ringo's research role: one live web search call, every quoted source fetched and its quote checked in code, one write call that cites only verified evidence ([details](RESEARCH-ROLE.md)). Measured once at about 68,000 tokens, most of them search results |
| `/work SPEC.json` | Ringo's checked work loop on a task spec |
| `@codex TASK` | talk to Codex from now on: this turn and later plain lines go to it, in this folder |
| `@codex! TASK` | Codex in its own copy of the folder (a git worktree of it as it is now, uncommitted changes included); nothing reaches your files until `/apply` |
| `@codex @claude TASK`, `@all TASK` | the same task to several agents at once, each in its own copy, to compare |
| `{#N}` inside a task | hands job N's reply (and its change) to the agent; the shell shows exactly what was added |
| `/diff N`, `/apply N`, `/drop N` | see a copy's change, apply it to your files (uncommitted, onto what is there now), remove the copy |
| `/show N`, `/kill N`, `/copy`, `/backend SPEC`, `/theme NAME`, `/setup`, `/quit` | print a job's log (its last 2,000 lines; the whole stream is in its folder), stop it with everything it started, copy the last answer, set the backend for `/ask` and pipelines, change colours, add an agent, quit |

Start `ringo` in any folder, as you would `claude` or `codex`. In a git project each turn's change can be undone,
copies work and `/review BASE` reviews its commits; in any other folder, a repository that holds your home folder (a
dotfiles repository) included, the agents still work in the folder, and Ringo says that `/undo` and copies need a git
project. Ringo's private state stays out of every repository; its folder carries a `.gitignore` of its own.

A turn costs about 12,000 to 15,000 tokens before the agent starts (measured under
[Modes and permissions](#modes-and-permissions); Claude Code's own first turn measured about 50,000), and a vague
request can send it searching (one such follow-up cost about 450,000); `/ask` answers a question that needs none of
your files for about 1,300. Enter sends; Shift-Enter (in terminals with the kitty keyboard protocol: kitty, Ghostty,
WezTerm, foot, iTerm2 with "Report keys using CSI u" on), Ctrl-J, Alt-Enter or `\` then Enter starts a new line. A
paste goes into the input box with its lines kept, and nothing is sent until you press Enter (in a terminal without
bracketed paste, keys that arrive together, as a paste's do, are kept the same way, its last newline too). Typing
`/` at the start of a line, or `@` at the start of a word, opens a menu under the input of the commands or agents
that match: Up and Down choose, Tab or Enter completes (Enter sends a command that takes nothing more). Up and Down
otherwise move between the lines of a message, then through earlier messages, which are kept between sessions (the
last 1,000, in `shell-history.jsonl` in Ringo's private state). Shift-Tab changes the mode. Esc clears the input,
and with the input empty stops the turn running here. Ctrl-C clears the input, then stops the turn, then (twice)
quits, stopping running jobs. The line-editing keys are a shell's: Ctrl-A and Ctrl-E (or Home and End) go to the
start or end of the line, Ctrl-U and Ctrl-K cut to either, Alt-Left and Alt-Right (or Alt-B and Alt-F, Ctrl-Left and
Ctrl-Right) move a word, and Ctrl-W or Alt-Backspace cuts the word before the cursor (Alt-D the one after).

The rows above the input show each running job's time, tokens, Ringo model calls and what it is doing now, with one
row per subagent: Claude Code's Task subagents and Codex's spawned agents. A Claude turn's tokens count up as its
messages report them, so a turn that searches far shows it while it runs; on a real 452.9k-token turn the running
count reached 93% of the turn's own total, which replaces it at the end (a message's output is counted before it is
written). Codex reports its tokens only when a turn ends. When two unapplied copies edit the same file, the running
job's row and the line it ends with say so (`⚠ same files as #2 client.rs`) before you apply either. That check costs
no tokens: it compares the agents' own edit events as they arrive.

## Modes and permissions

One switch sets how far every agent may act without asking; it applies to the turns started after it changes.

| Mode | Claude | Codex |
|---|---|---|
| `ask` (the default) | reads, and edits files in its folder (yours, or its copy), freely: Claude Code's accept-edits mode, in which plain file commands such as `mkdir` there run without asking; every other action waits for you: a command such as `curl`, `gh` or `npm`, a connector call, a file outside its folder | works inside its sandbox (`workspace-write`: edits in its folder, no network); it cannot stop to ask, so what the sandbox refuses fails |
| `auto` | Claude Code's auto mode: it acts on its own and its safety review blocks what is dangerous; what that review blocks repeatedly, and what an always-ask rule names, waits for you | a new conversation runs with `--approve-for-me` (Codex's own reviewer answers its approval requests), and every turn has the network on in the sandbox, for git, gh and installs (`exec resume` takes no `--approve-for-me`) |
| `read` | no edits, no commands, no connectors; `@agent!` is refused | read-only sandbox; `@agent!` is refused |

When an agent waits, a box takes the place of the input and says in plain words what it wants (to run a command,
open a web page, use a connector), shows the whole action, every line of a command included, and offers the answers:
`1. Yes` (`y` or Enter) allows it once, `2. Yes, and allow ... for the rest of this session` (`a`) allows it and the
same rule in every later Claude turn too, as `--allowedTools` (an always-ask action offers only once and no), and
`3. No` (`n` or Esc) declines it and the agent carries on without it; Up and Down move between them. A request longer
than its box shows its first lines and how many more there are; all of it is also in the conversation above, where it
printed when it arrived. The keys answer only when the input line is empty (a line being typed keeps the input box,
under the request) and not in the first 0.3 s after the box appears, so a word you are typing never answers by
accident.
Every answer is kept in `actions.jsonl` in the session's
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
| read-only turn (now read mode) | 16,547 | 16,547 | 16,547 |
| turn that edits (now a plain line, or `@claude!`) | 17,139 | 17,133 | |

The first version cost 18,113 for the read turn: with a permission host, Claude Code also offers `AskUserQuestion`,
`EnterPlanMode` and `ExitPlanMode`, which Ringo does not serve, so the shell denies them. The always-ask rules add
nothing. Times ranged from 1.4 s to 3.8 s across these single runs with no pattern by mode, so no latency change can be
claimed either way. Claude Code does not report its auto-mode review's own calls, so whether they count against a
plan's usage is not known.

**Tools a Ringo turn never needs.** Claude Code also gives every turn tools that act outside it: scheduling (`Cron*`,
`ScheduleWakeup`), `RemoteTrigger`, `PushNotification`, sending to other projects, `EnterWorktree`/`ExitWorktree` on
your repository, design sync, review reports and multi-agent `Workflow`. Their schemas are sent in full, so the shell
denies them in every mode; subagents, skills, search and the file and shell tools stay. Codex turns lose its ask tool
and the collaboration-mode text that goes with it, and analytics; unless the task names a connector they also lose
plugins and apps (13 of the 17 skills listed, recommended plugins, and a computer-use tool that acts outside the
sandbox). One trivial turn each (2026-10-31, Claude Code 2.1.316, codex-cli 0.165.0):

| Turn | Before | After |
|---|---|---|
| Claude read-only turn (now read mode) | 16,547 | 11,984 |
| Claude turn that edits (now a plain line, or `@claude!`) | 17,133 | 12,570 |
| Codex turn, per call | 17,133 | 14,350 |

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

Measured while building the shell (2026-10-29, one run each, trivial prompts; today's turns are lighter, see Modes and
permissions):

| | Tokens |
|---|---|
| Claude Code turn with the owner's normal setup (138 tools, 21 MCP servers, CLAUDE.md, plugins) | 50,201 |
| the same turn with `--safe-mode`, which the shell used then | 16,567 |
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
- **Turns in your folder** run where you started `ringo`, continuing the agent's conversation, one at a time: Codex
  with `workspace-write` (edits and commands inside the folder, no network; in auto mode the network and, for a new
  conversation, `--approve-for-me`), Claude with `acceptEdits` in ask mode and auto mode in auto mode (edits in the
  folder allowed; any other command waits for you, or for its review), agy with `--mode accept-edits` and
  session-only deny rules for `command(*)`, `unsandboxed(*)`, `mcp(*)` and `execute_url(*)` (edits allowed;
  commands, MCP tools and browser actions refused, as it cannot ask). Grok and agents you add have no sandbox Ringo
  has verified, so they only work in a copy, and a line for one goes there.
- **Undo:** in a git project, Ringo takes the folder's files (every file git does not ignore, untracked ones too) as
  a git tree before a turn that may edit and again when it ends, with a private copy of the index: your index, stash
  and HEAD are never touched. The files that differ are the turn's change; `/undo` applies the reverse patch with
  `git apply`, which checks every file first, so a file you edited since makes it refuse and change nothing. Outside
  a git project there is no undo.
- **Read mode:** Codex runs with its `read-only` sandbox. Claude runs with deny rules for `Bash`, `Edit`, `Write`
  and `NotebookEdit`, which win over any allow rules in your own settings, and nothing may prompt
  (`--permission-prompts none`). agy adds `write_file(*)` to its deny rules; asked to edit, it gets an error it can
  see and answers anyway (checked live with agy 1.4.11: no file appeared). Checked live: asked in a read-only turn to
  create a file, Claude and Codex both refused and no file appeared, but trying cost Claude 98k tokens and Codex 40k.
- **Copies** (`@agent!`, or several agents at once) run in a worktree under Ringo's private state folder, made from
  the folder as it is (its files as a commit no branch points at), with the same settings as a turn in the folder,
  and Grok with `--always-approve` (every tool call approved, so it is not contained; its flags come from xAI's
  documentation and are untested here). A worktree keeps parallel edits apart; it is not a security boundary.
- **Applying:** `/apply` needs a finished job, checks the patch first, and applies it with `git apply` onto your
  files as they are, without staging or committing; where that does not apply and your tree is clean (you committed
  since), with `git apply --3way`. A conflict leaves your files and the job untouched.
- **Stopping:** `/kill` stops the job's whole process group. A Ringo pipeline is interrupted first, so Python's own
  cleanup ends its model calls, which run in sessions of their own.
- **No recursion:** agents run with `RINGO_CHILD=1`, and both `ringo` and `ringo.py` refuse to start under it.
- **Fail closed:** a line that is not a JSON event, a failed turn, a missing completion record or an empty reply
  fails the job; it is never shown as done.
- **Limits:** three jobs at a time, and one turn at a time in your folder; the shell queues the lines for the folder,
  and refuses a fourth job.
- **State:** `~/.local/share/ringo/shell/` (or under `RINGO_RELEASE_HOME`), owner-only. Each job folder keeps its
  task, raw events, reply, stderr and patch. Logs can contain your code: delete them yourself when done.

## For the IDE

The shell is a thin front end over the `ringo_cli` library: `Session` launches and stops jobs, `pump()` folds their
events into `Job` records (status, activity, tokens, subagents, touched files), and `diff` / `apply` / `drop_worktree`
/ `overlaps` handle changes. A Rust IDE links the library and renders the same records.

## Not yet

A JSON-lines mode for a non-Rust front end, an edit's diff shown in the conversation as it lands, undo outside a
git project, and measured repository-level comparisons of agent turns against the pipelines.

Route suggestions (`/route on`: word rules that put the matching command in the input for a line that read like a
task; acceptable 35 times in 38 on 100 held-out lines, covering about half of the tasks) were removed when a plain
line became an agent turn in the folder: there was nothing left for them to suggest.

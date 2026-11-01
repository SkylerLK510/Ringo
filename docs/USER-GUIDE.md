# How to use the `ringo` shell

Type `/usrman` (or `/help`) in the shell to see this guide again. The full reference, with what each mode allows
and what it was measured to cost, is [docs/SHELL.md](SHELL.md).

## Start

1. Open a terminal in your project. Agents can edit only inside a git repository; anywhere else they can read and
   answer.
2. Type `ringo`.
3. The first time, Ringo asks which agents you use. Type the command you start each one with (`claude`, `codex`,
   `agy`, `grok`) and sign in when it asks. Ringo uses your subscription logins, never API keys.
4. The line at the bottom shows the folder, the branch, the mode, how many jobs are running and the tokens used.

To add an agent later, type `/setup`.

## Ask a question

Type it and press Enter, with no `/` or `@` in front.

- It is one quick model call, with your last few questions and answers as context. It cannot see your files.
- When the line is really a task (change files, review code, look something up online), Ringo starts the job for
  you: Claude for code (Codex when Claude is not set up), `/research` for sources.

## Ask an agent about your code

Put the agent's name first:

```
@claude where do we retry failed requests?
@codex what does parse_args return on bad input?
```

The agent reads the repository but cannot change it. Later `@claude` lines continue the same conversation.

## Have an agent change code

1. Add `!` to the name: `@claude! add a retry to the HTTP client`.
2. The agent works in its own copy of the repository, a git worktree made from your last commit. Your uncommitted
   changes are not in it, and nothing touches your files yet.
3. Each step it takes prints as a `•` line, with what came of it. When it finishes you see
   `✓ #1 claude! done · … · change ready: /diff 1, /apply 1`. If it changed nothing, the line says `no change`
   instead; `/drop 1` removes its worktree.
4. Read the change with `/diff 1`. For a second opinion, `/review #1`.
5. Commit or stash your own changes, then `/apply 1`. The change lands in your tree, uncommitted, for you to check
   and commit.
6. `/drop 1` removes the job's worktree; its log stays.

## Use several agents at once

- `@claude! @codex! TASK` gives the same task to both, each in its own worktree. Compare `/diff 1` with `/diff 2`
  and apply the one you prefer.
- `@all TASK` asks every agent you set up; `@all! TASK` lets them all edit.
- When two unapplied jobs edit the same file, their lines say so (`⚠ same files as #2 client.rs`) before you apply
  either.
- To hand one job's answer and change to another agent, write `{#1}` in the task:
  `@codex check this change: {#1}`. The shell shows what it adds before it is sent.

## Approve what an agent wants to do

In `ask` mode (the default), Claude waits for you before anything beyond reading and editing its own worktree: a
command in an edit turn, the network, a connector such as Jira, a push or a PR. A box above the input shows the
whole action:

| Key | Answer |
|---|---|
| `y` | allow it once |
| `a` | allow it, and the same rule for the rest of the session |
| `n` or Esc | decline; the agent carries on without it |

The keys answer only while the input line is empty. Codex cannot stop to ask: in `ask` mode its sandbox (no
network, writes only in its worktree) refuses the rest. Even in `auto` mode, Claude always waits before `sudo`,
disk tools, force pushes, publishing a release, or editing an agent's settings.

## Choose how much agents may do

| Mode | Agents may, without asking |
|---|---|
| `ask` | read, and edit their own worktree |
| `auto` | act on their own; only what their safety review flags waits |
| `read` | look only: no edits, no commands; `@agent!` is refused |

Shift-Tab cycles the modes, `/mode ask|auto|read` sets one, and `/auto` switches between auto and ask. A new mode
applies to the turns started after it and is remembered for next time.

## Run Ringo's pipelines

These run the models as workers, with executable checks and one review:

| Command | What it does |
|---|---|
| `/review #N` | review job N's change |
| `/review BASE [HEAD]` | review `BASE...HEAD` of this repository |
| `/design TEXT` | a design for `TEXT`, in one call |
| `/research TOPIC` | a web search; every quoted source is checked, and the answer cites only those |
| `/work SPEC.json` | the checked work loop on a task spec |
| `/backend SPEC` | the backend for questions and these pipelines |

## Keep track of jobs

| Command | What it does |
|---|---|
| `/show N` | print job N's log and where its files are |
| `/diff N` | show job N's change |
| `/apply N` | put job N's change in your tree, uncommitted |
| `/drop N` | remove job N's worktree |
| `/kill N` | stop job N and everything it started |

While jobs run, the area above the input shows one row for each, and for each of its subagents, with its time,
the tokens it has used so far and what it is doing now. A turn that runs away shows it there: `/kill` it.

## Keys

| Key | What it does |
|---|---|
| Enter | send |
| Shift-Enter, Ctrl-J | new line (so do Alt-Enter, and `\` then Enter); the input box says which works here |
| `/` or `@` | a menu of commands or agents: Up and Down choose, Tab or Enter completes |
| Up, Down | move between lines, then through earlier messages, kept between sessions |
| Ctrl-A, Ctrl-E | the start or end of the line; Ctrl-U and Ctrl-K cut to either |
| Alt-Left, Alt-Right | a word back or on (also Alt-B, Alt-F); Ctrl-W or Alt-Backspace cuts the word before |
| Shift-Tab | change the mode |
| Esc | clear the input |
| Ctrl-C twice | quit, stopping running jobs |

A paste keeps its lines and is not sent until you press Enter. The conversation is in your terminal's scrollback:
scroll it and select from it as in any other program.

## Settings

| Command | What it does |
|---|---|
| `/setup` | add an agent and sign in to it |
| `/update` | update the agents that are out of date, with no tokens; `/update auto on` does it when the shell opens |
| `/route on` | when a line reads like a task, put the matching command in the input for you to send, change, or ask as it is with Esc |
| `/theme NAME` | colours: siamese, siamese-light, pink, midnight or mono |
| `/quit` | leave |

## What it costs

- A question: one small call, about 1,300 tokens when measured.
- An agent turn: about 12,000 to 15,000 tokens before the agent starts, and far more when it searches many files.
  Each job's line shows its count; a narrow question keeps it low.
- `/research`: about 70,000 tokens, most of them search results.

## When something goes wrong

| You see | Do this |
|---|---|
| `your tree has uncommitted changes` | commit or stash them, then `/apply N` again |
| `#N is untouched; inspect it with /diff N` | the change did not apply cleanly and your tree is unchanged; read it with `/diff N` |
| `needs a git project` | quit, `cd` into a git repository, and start `ringo` there |
| `has no verified read-only mode` | that agent only takes edit turns: use `@name!` |
| `no agent NAME` | add it with `/setup` |
| `#N is running; /kill it first` | `/kill N`, then try again |
| a job that seems stuck | `/show N` to read its log, `/kill N` to stop it |

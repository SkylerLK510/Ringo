# How to use Ringo

Type `/help` in Ringo to see this guide again, or `?` for just the keys. The full reference, with what each mode
allows and what it was measured to cost, is [docs/SHELL.md](SHELL.md).

## Start

1. Open a terminal in the folder you want to work on.
2. Type `ringo`.
3. The first time, Ringo looks for the AI coding agents on your computer (Claude Code, Codex, Antigravity) and helps
   you sign in to them. It uses your own subscription logins, never API keys.
4. Ringo fills the window. The conversation is at the top, you type in the box at the bottom, and the line under the
   box shows the folder, the mode, which agent you are talking to, and how many tokens you have used.

## Ask for what you want

Type it in your own words and press Enter. For example:

```
explain what this project does
fix the failing test
add a dark mode switch to the settings page
```

- The agent you are talking to (Claude, or Codex when Claude is not set up) reads the files in this folder, changes
  them if that is what you asked for, and answers.
- Its reply appears as it is written. Each step it takes shows as its own line, with what came of it.
- While it works, the row above the box shows its time, the tokens it has used and what it is doing now. Press Esc
  to stop it.
- You can type the next message while it works. It waits under the box and goes when the turn ends.
- The agent remembers the conversation, so you can follow up with "now add a test for it" or "why did you do that?".
  `/clear` starts a new conversation. When a long conversation makes each turn cost a lot, `/compact` has Claude
  shrink it to a summary: one call over the whole conversation, and every turn after it is smaller.

## Take back a change

When a turn changes files, its last line says which ones, for example `changed app.py (/undo takes it back)`.
Type `/undo` to put those files back as they were before that turn. Each `/undo` goes one turn further back. If you
edited any of those files yourself since then, Ringo leaves everything as it is and says so.

`/undo` works in any folder. In a git project Ringo keeps each turn's before and after with git; anywhere else,
such as a class folder, it keeps them in a store of its own, and your folder is left as it is. A few places are left
out: your whole home folder; a folder with a git project inside it (start Ringo in that project instead, where
`/undo` works); and a folder with more than 5,000 files or 200 MB in it (not counting installed packages and caches),
where typing `git init` in a terminal there once turns `/undo` on. In those, Ringo says `/undo` is off when it starts.

## Answer when an agent asks

In `ask` mode (the default), the agent changes files in this folder on its own, but asks you before anything else:
running a command, using the network, a connector such as Jira, a push or a PR. A box takes the place of the input
and shows exactly what it wants to do:

| Answer | Key |
|---|---|
| Yes, this once | `1`, `y` or Enter |
| Yes, and the same for the rest of this session | `2` or `a` |
| No; it carries on without it | `3`, `n` or Esc |

Up and Down move between the answers too. If you were typing when the box appeared, send or clear your line first.
Some actions always ask, even in `auto` mode: `sudo`, disk tools, force pushes, publishing a release, and editing an
agent's settings.

## Choose how much agents may do

| Mode | Agents may, without asking |
|---|---|
| `ask` | read, and change files in this folder |
| `auto` | act on their own; only what their safety review flags waits |
| `read` | look only: no changes, no commands |

Shift-Tab cycles the modes, `/mode ask|auto|read` sets one, and `/auto` switches between auto and ask. A new mode
applies to the turns started after it and is remembered for next time. Codex cannot stop to ask, so in `ask` mode it
changes files in this folder but has no network.

## Choose how hard agents think

| Tier | What it does | Tokens |
|---|---|---|
| `quick` | agents think less (low effort) | fewer |
| `standard` | each agent's own default | as before |
| `deep` | agents think more (extra-high effort) | more, and slower |
| `ultra` | `deep`, then Ringo reviews each change a turn makes here; when the review finds defects, the agent gets one turn to fix them | one review (about 70,000) and at most one more turn |

`/tier deep` sets one; `/tier` alone shows the one in use. It applies to turns started after it, for this session
only: Ringo starts in `standard` every time, so a tier that spends more is never left on by accident. The top line
names the tier when it is not `standard`. In `ultra`, lines you send while the review runs wait for it (Esc stops the
review and gives them back), and nothing follows the one fixing turn. Ringo's own measurements found that higher effort alone did not change results on the
tasks it was tried on, while one review and one fix did, so `ultra` is the tier for work you want checked.

`ultra` works in any folder where `/undo` does, git project or not. The review runs on Codex; when Codex's plan has
used up a limit, it runs on Claude instead. Ringo checks both plans as the turn starts (no tokens), and when neither
has room it says so then, so you can stop the turn or let it run without a review.

## See what you have used

`/usage` shows:

- the tokens this session has used, by agent;
- the last 24 hours and the last 7 days, from a record Ringo keeps of every job it runs;
- how much of each plan's limits is used and when each starts over: Claude's five-hour and weekly limits, Codex's
  five-hour and weekly ones and the full resets you have left. Ringo asks Claude and Codex directly; that takes
  about a second and no tokens.

## Talk to a different agent

Put its name first: `@codex why is this slow?`. The rest of the conversation goes to Codex too, until you pick another
with `@claude`. Each agent keeps its own conversation.

## Compare agents

- `@claude! TASK` gives the task to Claude in its own copy of the folder, so your files are not touched.
- `@claude @codex TASK` gives the same task to both, each in its own copy, and `@all TASK` to every agent you set up.
- When a copy is done, its line says `change ready`. `/diff 2` shows what it changed and `/apply 2` puts that change
  into your folder. `/drop 2` throws the copy away.
- When two copies change the same file, their lines say so before you apply either.
- To hand one job's answer and change to another agent, write `{#1}` in the message: `@codex check this: {#1}`.

Copies need a git project.

## Run Ringo's pipelines

These run the models as workers, with checks Ringo runs itself, and one review:

| Command | What it does |
|---|---|
| `/review #N` | a second opinion on job N's change |
| `/review BASE [HEAD]` | review `BASE...HEAD` of this repository |
| `/design TEXT` | a design for `TEXT`, in one call |
| `/research TOPIC` | a web search; every quoted source is checked, and the answer cites only those |
| `/ask TEXT` | a quick answer from one cheap call that cannot see your files |
| `/work SPEC.json` | the checked work loop on a task spec |
| `/backend SPEC` | the backend for `/ask` and these pipelines |

## Keep track of jobs

Every turn and pipeline is a numbered job.

| Command | What it does |
|---|---|
| `/show N` | job N's log, and where its files are |
| `/kill N` | stop job N and everything it started |
| `/copy` | copy the last answer |

## Keys

| Key | What it does |
|---|---|
| Enter | send |
| Shift-Enter, Ctrl-J | new line (so does `\` then Enter) |
| Esc | clear the input; with it empty, stop the turn |
| Up, Down | move between lines, then through earlier messages |
| `/` or `@` | a menu of commands or agents: Up and Down choose, Tab or Enter completes |
| PgUp, PgDn | scroll the conversation (the mouse wheel does too) |
| drag | select text in the conversation and copy it |
| Ctrl-A, Ctrl-E | the start or end of the line; Ctrl-U and Ctrl-K cut to either |
| Alt-Left, Alt-Right | a word back or on (also Alt-B, Alt-F); Ctrl-W cuts the word before |
| Shift-Tab | change the mode |
| Ctrl-C | stop the turn; with nothing running, twice to quit |

A paste keeps its lines and is not sent until you press Enter. Shift-Enter works where the terminal can report it
(iTerm2, kitty, WezTerm, Ghostty); in macOS Terminal use Ctrl-J. When you quit, the conversation is saved as a text
file and Ringo prints where.

## Settings

| Command | What it does |
|---|---|
| `/setup` | add an agent and sign in to it |
| `/update` | update the agents that are out of date, with no tokens; `/update auto on` does it when Ringo opens |
| `/tier NAME` | how hard agents think: quick, standard, deep or ultra (above) |
| `/usage` | what you have used, and how much of your plans is left |
| `/theme NAME` | colours: siamese, siamese-light, pink, midnight or mono |
| `/quit` | leave |

## What it costs

- A message: one agent turn, about 12,000 to 15,000 tokens before the agent starts, and far more when it searches
  many files. Claude Code measured about 50,000 for the same first turn. A narrow request keeps it low.
- `/ask`: one small call, about 1,300 tokens when measured, for a question that needs none of your files.
- `/research`: about 70,000 tokens, most of them search results.
- `/tier ultra`: a turn that changes files adds one review, about 70,000 tokens, and, when the review finds a
  defect, one more turn.
- `/usage`: no tokens.

## When something goes wrong

| You see | Do this |
|---|---|
| `nothing to undo` | no turn has changed files here yet |
| `FILE changed after #N, so nothing was undone` | you changed that file since; undo it by hand |
| `#N is working in this folder` | wait for it to end, or press Esc to stop it |
| `/undo is off here: ...` | start Ringo in the folder you work on (not your home folder, and inside a git project when it names one); for a very large folder, type `git init` in a terminal there once |
| `your files changed where #N edits them` | `/diff N` shows the change; apply it by hand, or commit yours and `/apply N` again |
| `needs a git project` | copies and `/review BASE` work only in a git project (`/review #N` works wherever `/undo` does) |
| `ultra: ... so #N's change will not be reviewed` | both plans have used up a limit; the line says when each starts over |
| `no agent NAME` | add it with `/setup` |
| `#N is running; /kill it first` | `/kill N`, then try again |
| a turn that seems stuck | Esc stops it; `/show N` reads its log |

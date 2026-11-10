# The `ringo` shell

Type `ringo` in a terminal and Ringo opens its own full-screen session, the way `codex` opens Codex. You say what
you want in plain words and the agent you are talking to (Claude Code, OpenAI Codex CLI, Google Antigravity CLI, xAI
Grok Build, or one you add) does it in that folder, asking before it runs a command; `/undo` takes a turn's change
back. From the same screen you give a task to several agents at once, each in its own copy, run Ringo's own
pipelines with them, and watch every job and subagent as it runs.

The shell is a small Rust program (`cli/`). Its `/review`, `/design` and `/work` start the same binary, which runs
Ringo's pipelines natively, sending the same requests as the Python runtime (see the
[native runtime notes](NATIVE-RUNTIME.md)), or hands them to `ringo.py` when your release
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

### Agents of your own

Claude Code and Codex let a person or a team write a specialist: a file of standing instructions (what it knows,
how it works, what it must not do) that makes the model that agent. Until this change such a specialist could not
work in Ringo: a Claude turn runs in safe mode, which drops custom agents with the rest of the owner's
customizations, and nothing gave Codex one either. Now it is an agent like any other:

```
/agent add billing ~/notes/billing.md          Claude, with that file as its standing instructions
/agent add billing ~/notes/billing.md codex    the same specialist on the Codex plan
/agent add shop-billing                        an agent made for Claude Code, by its name
/agent add billing ~/notes/billing.md skills   on Claude, with your Claude Code skills, plugins and connectors
/agent                                         your own agents, and the Claude Code agent files found
/agent remove billing                          the entry goes; the file is yours and stays
```

`/agent add` writes an entry in your `agents.json` (`"instructions": PATH`, the base agent's `adapter` and `program`,
`"read": true`, and `"skills": true` when asked) and sets the agent up at once. A name alone, or a word that is not
a file, is looked up as `NAME.md` in `.claude/agents` of this folder and then of your home folder, where Claude
Code keeps agent files; `/agent` lists the ones found that are not agents here yet.

How the instructions reach the model, each turn:

- The file is read when the turn starts, so an edit is in the next turn. A leading `---` block (a Claude Code agent
  file opens with its name and description there) is left out. A file that is missing, empty or over 400,000 bytes
  stops the turn before it starts: a turn without them would not be that agent's.
- **Claude:** appended to the system prompt (`--append-system-prompt-file`), after your `rules.md` and before the
  project's `CLAUDE.md` or `AGENTS.md`. The turn stays in safe mode, as lean as any other.
- **Codex:** the thread's `developerInstructions`, given on `thread/start` and `thread/resume`. For that agent's
  turns they stand in for any `developer_instructions` in your Codex settings.
- **The file's `model` and `tools` lines (Claude only):** an agent file made for Claude Code can name both in its
  opening block. The model is the one the specialist runs on unless you chose one with `/model` (`inherit` means
  the agent's own). The tools listed are the only built-in tools it is offered (`--tools`); `tools: Read, Grep`, a
  `[Read, Grep]` list and `- Read` lines all read the same. It can only narrow: what the mode denies stays denied,
  and a connector's tools are not Ringo's to offer. On Codex both lines are left out, because they name Claude
  Code's models and tools, and `/agent add` says so.
- **`skills` (Claude only):** outside read mode the turn runs without safe mode and keeps Claude Code's commands,
  so your skills, plugins, hooks and connectors load. Your own `CLAUDE.md` and memory still do not (so an
  instruction there to use Ringo cannot start Ringo inside Ringo). In read mode the turn is as lean as any other
  and loads none of it: a connector's tool that your own settings allow could write, and read mode's promise is
  that nothing can. So a specialist in a talk (`/discuss`, always read mode) goes without its skills.

A specialist is a separate agent: its own conversation, its own row in the side panel and in `/usage`, its own
`/model`. In the conversation the agents share it hears what the others did and they hear it (see
[One conversation](#one-conversation-and-a-fresh-start-past-64k)). It runs on the plan of the agent it is made from,
so the plan's gauge shows once, under that agent's name.

Checked live with a team's 49.6k-character agent file, one trivial read-mode turn each
(`cargo run --example own -- REPO BASE FILE [skills]`; Claude Code 2.1.322, codex-cli 0.167.0; one run each, so an
instance and not a rate):

| Turn | Tokens | Time | What came back |
|------|--------|------|----------------|
| the specialist on Claude | 30,002 | 4.5 s | answered as the specialist, naming what its file says it knows |
| the specialist on Codex | 25,596 | 3.9 s | the same |
| on Claude, asked to name skills it can invoke | 29,857 | 3.4 s | "NONE": no Skill tool in a safe-mode turn |
| on Claude with `skills`, same question, ask mode | 42,784 | 3.0 s | named three of the owner's skills |
| a 23-byte file with `model: sonnet` and `tools: Read, Grep`, asked to list its tools | 4,691 | 1.7 s | ran `claude-sonnet-5-5`; listed Grep and Read, nothing else |

The same turn with a ten-byte instructions file was 10,069 tokens on Claude and 13,375 on Codex, so this file costs
about 20k tokens a call on Claude and 12k on Codex, and `skills` about 13k more with 99 skills installed. That is
the cost of the feature, paid only by the specialist's own turns: a turn of `claude` or `codex` is unchanged.

Not done: agy and plain agents take no instructions, and a removed name stays taken until Ringo restarts.

### Who takes which turn

Claude owns writing and reading code: a plain line goes to Claude when it is set up, else to Codex, else to any
agent that works in the folder. `@codex` (or any `@agent`) switches who you talk to, and later plain lines go there
too; `@codex` alone switches without a turn. A plain line also goes to whichever agent's words came last in this
folder, unless you named an agent after them: you answer what you read last. A line you have started typing keeps its
agent, and the input box always names it (` claude ❯ `). Waiting lines show whose they are (`↳ next for claude: …`).
It is one conversation: an agent is told what the others did since its last turn (see
[One conversation, and a fresh start past 64k](#one-conversation-and-a-fresh-start-past-64k)). Codex owns the abstract work: Ringo's pipelines (`/research`,
`/design`, `/review`) and `/ask` run on it.

### The agents' talk

Working with several agents used to mean asking one, reading its answer, then typing `@codex what do you think?`
and carrying each view to the next agent by hand. `/discuss TOPIC` has them talk it over among themselves while you
read:

```
 you  /discuss Is this folder's layout fine as it is, or would you move anything?
  claude and codex talk it over, one at a time, looking but not touching · at most 2 turns each · a line of
  yours joins in · Esc stops them

 claude  #1 · claude-fable-5-2
  The layout is fine as it is and I would not move any file: ...

 codex  #2 · gpt-6.1-sol
  I agree with keeping the layout ... I'd keep the timeout default in Client::new() for now, because ...

 claude  #3 · claude-fable-5-2
  AGREED

── the talk is over: no one has more to add · 3 turns · 0:32 · 90.0k tokens · /discuss goes on with it
```

How it works:

- **Who:** every agent set up that works in this folder, or the ones named (`/discuss @claude @codex TOPIC`). An
  agent of your own takes part like any other; one that works only in its own copy hears no one and is refused. It
  needs the conversation the agents share, so `/separate` refuses it.
- **Order:** one speaks at a time. The agent you talk to opens a topic. With no topic (`/discuss` alone) they talk
  over the last turn, so the others answer the agent that took it.
- **What each hears:** Ringo's record of the turns it was away for, as for any turn in the shared conversation
  (see [One conversation](#one-conversation-and-a-fresh-start-past-64k)), so nothing is sent twice and no call
  summarises anything. A talk's replies are heard whole (up to 6,000 characters), not by their first paragraph.
  The topic is said once, to the first speaker; the others read it in the record.
- **Read mode:** a talk's turns look and do not touch, whatever the session's mode. Acting on the outcome is your
  next line.
- **You:** a plain line typed while they talk joins in: the next speaker is sent it (`I add: ...`), and each agent
  may answer once more. Esc stops the running turn and the talk. `@agent ...` still goes to that agent alone.
- **The end:** a reply that is the one word AGREED adds nothing, and when every agent but the last to say
  something has agreed, the talk is over. Otherwise it ends when each has spoken twice: with `n` agents a talk is
  at most `2n` turns, and each line of yours adds at most `n`. It also ends when a turn fails or the next agent
  is at work on something else. Its turns have no `done` rows of their own: the last row counts them all.

Checked live through the real shell in a pty, Claude Code 2.1.322 and codex-cli 0.167.0 on a small Rust project,
one run each:

| Wording of the first turn | Turns | Time | Tokens | What happened |
|---------------------------|-------|------|--------|---------------|
| "Talk this over with codex, among yourselves" | 2 | 1:46 | 155.4k | Claude looked for a way to run Codex itself, reported that it could not reach it, and gave no view of its own; Codex replied AGREED |
| "I want you and codex to talk this over ... Your reply is shown to them and theirs come back to you here" | 3 | 0:32 | 90.0k | Claude gave its view; Codex read the files, agreed on one point and differed on another with its reason; Claude replied AGREED |
| the same, Claude and agy (agy 1.5.5), agy told where the whole record is | 2 | 0:41 | 117.0k | agy read the project's files, then opened the record, which is outside its folder: its turn ended with no reply, 82.5k tokens in |
| the same, agy not told where it is | 4 | 1:40 | 165.2k | agy agreed and added a point; Claude differed on it with its reason; agy replied AGREED |

The first wording was dropped: an agent told to talk to another, and not told that its words are carried, goes
looking for the other. The second is what ships. A talk is about 30k tokens a turn here, most of it each agent
reading the files for itself: it is the cost of asking each agent, without the typing in between.

While they talk, `/separate` and taking a talker away (`/agent remove`) are refused, and `/clear` ends the talk with
the turn then running: the conversation they talked in is gone.

agy takes part as the others do, since the third run above. Every agent that goes on in the shared conversation is
told where the whole record of the turns it missed is kept. That file is in Ringo's state folder, and a read agy has
no permission for ends its turn (the others are refused such a read and go on). So agy is no longer told where it
is: it gets the record itself, as before, and nothing to open. This holds for any turn of agy's after another
agent's, not only in a talk.

Not done: an agent cannot ask another on its own in the middle of a turn. A talk under way ends with the shell: it is
not kept across `/restart`.

### A message while it works

A line you send to the agent whose turn is running in the folder goes into that turn, as Claude Code and Codex take
a message typed while they work, and the shell says `↳ #N takes this as it works`. Claude gets it as another
stream-json message on the stdin Ringo already keeps open for permission answers. It reads it with its next tool
result, or, when the line arrives during its final answer, answers it in a run of its own right after its result;
both runs count in the turn. Claude echoes each message as it takes it in (`--replay-user-messages`, which adds no
tokens), and Ringo closes its stdin only once every line has been taken in and answered. Codex gets `turn/steer`,
which names the running turn and adds the line to it, a final answer included (one `turn/completed`). A line the turn
never took in (Codex refused it because the turn had just ended, the input had closed, or the agent stopped or failed
first) goes as the next turn instead, or back in the input if you stopped the turn, so none is lost. agy's stdin closes
after its prompt and a `/compact` turn takes none, so a line for them waits under the box, as does a line typed behind
one already waiting, and one that names a connector when the turn was started without them. Your words go into Ringo's
record of the turn ("then, while it worked: ...") and into what `/review` and ultra's review judge the change by.

Checked live with Claude Code 2.1.316 and codex-cli 0.166.0, with "Also end your reply with the word KIWI." sent
while each worked. During `sleep 8 && echo first` (sent 4 s in), Claude answered `first … KIWI` in one result. During
a 250-word story with no tools (3 s in), Claude finished the story, then wrote it again ending in KIWI, as a second
result. Codex ended both with KIWI, in one turn each.

## Using it

An empty conversation shows only where to start, in its middle; it goes once you say something, and `/usrman` has the
rest (where Ringo is, who works here, the guide and the settings in force):

```
 ▲▲ ringo  my-repo (main)                                  ask mode · 0 tokens
────────────────────────────────────────────────────────────────────────────────

                                    ▲▲ ringo

               Tell Ringo what you want done, in your own words.
                  /usrman how it works · / commands · ? keys

────────────────────────────────────────────────────────────────────────────────
 claude ❯ Tell Ringo what you want to do                      ⇧⇥ mode · ? keys
```

Then each speaker starts with a chip in their own colour (shown here as plain text):

```
 you  add a retry to the HTTP client

 claude  #1
  ✓ read 1 file, searched 1 time
  ✓ edit client.rs                                 +3 −1
      - let resp = self.http.send(req)?;
      + let resp = retry(3, || self.http.send(req.clone()))?;
      + // backs off 100 ms, then doubles
  ✓ run the retry tests

  Added a retry with backoff to `send` in **client.rs**, and a test for it.

  ✓ done · 0:41 · 34.8k tokens · changed client.rs (/undo takes it back)
```

The shell fills the terminal (the alternate screen, as Codex's does). The conversation takes the top: your lines,
every reply, and what each agent does under its job number: one row per step once it has run, with what came of it
(an error, a failed command's exit code). A command reads as the agent described it ("run the retry tests"), not as
its shell line; Codex's commands lose the shell they run in. An edit shows the lines it changed under it (removed in
red, added in green, up to 8, then a count; a new file its first 4) and `+3 −1` at the row's end. Reads, searches and
folder listings that went through are counted on one row that grows as the agent looks ("read 6 files, searched 4
times"), until another step or the reply comes; Codex's commands that only read or searched count the same way. A
step too long for the row loses its end to `…`, and `/show N` has every step whole, each read and search too. Then
one line when a job ends with its time, tokens and Ringo model calls, and the files it changed. A reply appears as it
is written (Claude's and agy's text deltas, Codex's `item/agentMessage/delta`), and the whole reply replaces the
pieces when it is complete. What has been written does not change under the reader ([details](#a-reply-as-it-is-written)). Replies render their
markdown: headings, emphasis and code styled, lists and quotes wrapped under their own text, and tables as aligned
columns (a long last column wraps under itself), or, when even that does not fit, as one block per row with each cell
under its column's name. Everything is fitted again when the window's width changes. The mouse wheel and PgUp/PgDn
scroll it, and it stays where you scrolled whatever is written meanwhile, below it or above it: the place is kept as
a row of an entry, not as a count from the end, so the reply of an agent that started first can grow above what you
are reading of the second without moving it. The input's hint counts the rows below (`scrolled up · 14 rows below`),
so nothing arrives unseen. Typing goes back to the end; with nothing typed, End does too, and Home goes to the start. A drag over
the conversation selects its text alone (never the input or the status line) and copies it when you let go, with
the system's own clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip`) or, where there is none, such as a
server over ssh, by asking the terminal (OSC 52). `/copy` copies the last answer. Below the conversation are the
running jobs, the lines queued behind the current turn, a permission request or the input box, and a status line.
When you quit, the conversation is saved as `conversation.txt` in the session's state folder and the shell prints
where. `ringo --resume` (`-r`) goes on with one of the folder's conversations after a quit, a crash or a closed
terminal: after every turn and at the end, the shell keeps what /restart hands on (each agent's conversation, the turns
it has yet to hear, its size and Ringo's record, whether /separate is on, and which agent's last turn called a
connector, so a follow-up keeps them) in `resume.json`, written whole and then renamed. The list shows the
newest ten for this folder by the owner's latest message. Once the shell that went on keeps its own, the old file is
renamed `resumed.json`, so one conversation is listed once. /restart goes through the same file. The manual, the step-by-step [user guide](USER-GUIDE.md), is not on screen until you ask for it: `/help` (or
`/usrman`) prints it into the conversation, followed by the settings in force, and `?` prints just its keys.

| You type | What happens |
|---|---|
| `TEXT` (no `/` or `@`) | a turn of the agent you are talking to, in this folder, continuing its conversation: it reads, changes files there, and asks before anything else (in `ask` mode). Each agent takes one turn at a time in the folder, and the agents work side by side: a line for an agent whose turn runs goes into that turn (Claude, Codex) or waits under the input for it (Esc stops the latest turn and gives its agent's waiting lines back to the input) |
| `/undo` | the files the latest turn here changed go back as they were before it; each `/undo` goes one turn further back. A file you edited since makes it refuse and touch nothing |
| `/ask TEXT` | a quick answer: one tool-free call with your last few `/ask` questions and answers as context; it cannot see your files. Measured: about 1,300 tokens for a design question that cost about 19,700 as a Claude Code turn. When the call reads the line as a task (files to change, read or review, or live sources) it answers with one `ROUTE:` line instead, and the shell gives it to the agent you are talking to, or to `/research`; a review is put in the input line for you to say what to review |
| `/clear` | a new conversation: each agent's next turn starts fresh, and a plain question carries no earlier answers |
| `/separate`, `/together` | `/separate`: each agent keeps a conversation of its own, and nothing of one is told to another. `/together` (the default): one conversation again. Either way the agents work in the folder at the same time, one turn each. Text after either is sent as a line of its own once it has switched (`/separate @codex write the README`); any other command that takes nothing refuses text after it rather than drop it. The head says `separate` while it is on |
| `/mode ask\|auto\|read`, `/auto`, Shift-Tab | how far every agent may act without asking (see [Modes and permissions](#modes-and-permissions)); `/auto` turns auto on (typed with auto on, it says so: a toggle would turn it off unseen), Shift-Tab cycles ask, auto, read. Remembered for next time |
| `/tier quick\|standard\|deep\|ultra` | how hard agents think (see [Tiers](#tiers)); for this session only |
| `/discuss TOPIC`, `/discuss`, `/discuss @A @B TOPIC` | the agents talk it over among themselves, one at a time, while you read (see [The agents' talk](#the-agents-talk)); alone, the last turn |
| `/agent`, `/agent add NAME FILE [codex] [skills]`, `/agent remove NAME` | your own agents (see [Agents of your own](#agents-of-your-own)): a file of standing instructions makes Claude or Codex a specialist under its own name, talked to as `@NAME` |
| `/model`, `/model AGENT NAME` | which model each agent's turns run on (see [Models](#models)): alone, who runs on what and the models your Codex plan offers; with an agent and a name, that agent's turns run on it from the next one, remembered for next time (`/model AGENT default`: its own again) |
| `/usage` | this session's tokens by agent, the last 24 hours and 7 days from `usage.jsonl`, and each plan's limits as Claude and Codex report them (no model call) |
| `/review #N` | Ringo reviews job N's change (a copy's, or a turn's here from its before and after), with its task as the change description |
| `/review BASE [HEAD]` | Ringo reviews `BASE...HEAD` of the repository |
| `/design TEXT` | Ringo's one-call design pipeline on `TEXT` |
| `/research TOPIC` | Ringo's research role: one live web search call, every quoted source fetched and its quote checked in code, one write call that cites only verified evidence ([details](RESEARCH-ROLE.md)). Measured once at about 68,000 tokens, most of them search results |
| `/work SPEC.json` | Ringo's checked work loop on a task spec |
| `@codex TASK` | talk to Codex from now on: this turn and later plain lines go to it, in this folder (`@codex` alone switches without a turn) |
| `@codex! TASK` | Codex in its own copy of the folder (a git worktree of it as it is now, uncommitted changes included); nothing reaches your files until `/apply` |
| `@codex @claude TASK`, `@all TASK` | the same task to several agents at once, each in its own copy, to compare |
| `{#N}` inside a task | hands job N's reply (and its change) to the agent; the shell shows exactly what was added |
| `/diff N`, `/apply N`, `/drop N` | see a copy's change, apply it to your files (uncommitted, onto what is there now), remove the copy |
| `/restart` | start the installed Ringo in this terminal, the conversations going on (applies an update; see Ringo's own updates) |
| `/update`, `/update approve` | update the agents found out of date; when a release runs the pipelines, check this Ringo's against it offline, then move them on your approval (see The pipelines' release) |
| `/show N`, `/kill N`, `/copy`, `/backend SPEC`, `/theme NAME`, `/setup`, `/quit` | print a job's log (its last 2,000 lines; the whole stream is in its folder), stop it with everything it started, copy the last answer, set the backend for `/ask` and pipelines, change colours, add an agent, quit |

Start `ringo` in any folder, as you would `claude` or `codex`. In a git project each turn's change can be undone,
copies work and `/review BASE` reviews its commits; in any other folder, a repository that holds your home folder (a
dotfiles repository) included, the agents still work in the folder and `/undo` works from Ringo's own store (see the
safety model), and so does `/review #N` of a turn there, while copies and `/review BASE` need a git project. Ringo's private state stays out of every repository;
its folder carries a `.gitignore` of its own. Opened in the home folder, the shell says once where a turn's edits all
went, when they all went to one git project, so the next session can start there.

When a step's output says a read was refused ("Operation not permitted": on a Mac, its privacy protection on another
app's data or a personal folder), the turn shows you once that what it could not read may still be there: agents
take a refused read for a missing thing (one said a browser extension was not set up). Only you see the line, so it
costs no tokens.

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

### A reply as it is written

A reply's pieces arrive a few characters at a time, and markdown is not known until its marks close. Shown as they
come, rows already on screen change: a word that outgrows its row moves to the next, a link loses its brackets when
it closes, and a table's columns shift with every wider cell (and it starts as a row of bars). So three things wait
until they are whole (`steady`), and nothing that has shown is taken back:

- the word being written, until its end comes or 150 ms pass without a piece (a path or URL past 32 characters, and
  Chinese, Japanese or Korean text, which has no spaces, show as they grow);
- a link whose text has a space in it, until it closes;
- a table, until a line that is not one of its rows comes or 1.5 s pass: meanwhile one line says
  `a table is being written · 3 rows so far…`.

A step, the turn's end or the pause shows everything kept back, so the last word before a tool call never waits.

Measured by writing a document in pieces of 4 to 24 characters and comparing each frame's rows with the one before
(release build, 120 columns; `measure_a_reply_being_written` prints it):

| Written in pieces | Frames | Rows above the last rewritten | The last row took text back |
|---|---|---|---|
| the 14.8k user guide, before | 1,060 | 85 | 18 |
| the same, now | 1,060 | 0 | 1 |
| this 56k page, before | 4,007 | 157 | 105 |
| the same, now | 4,007 | 3 | 10 |

What is left: a code span or a link that goes on over a line end, which the last line alone does not show. A frame
costs what it did: 0.2 ms on average for the guide and about 1 ms at the end of the 56k page, each frame parsing the whole
reply again, so the parse was left as it is. Saving a 300-turn conversation at a turn's end took 16 ms.

## Modes and permissions

One switch sets how far every agent may act without asking; it applies to the turns started after it changes.

| Mode | Claude | Codex |
|---|---|---|
| `ask` (the default) | reads, and edits files in its folder (yours, or its copy), freely: Claude Code's accept-edits mode, in which plain file commands such as `mkdir` there run without asking; every other action waits for you: a command such as `curl`, `gh` or `npm`, a connector call, a file outside its folder | works inside its sandbox (`workspace-write`: edits in its folder, no network); it cannot stop to ask, so what the sandbox refuses fails |
| `auto` | Claude Code's auto mode: it acts on its own and its safety review blocks what is dangerous; what that review blocks repeatedly, and what an always-ask rule names, waits for you | approvals `on-request` with Codex's own reviewer answering them (`approvalsReviewer: auto_review`, what `--approve-for-me` sets), and the network on in the sandbox, for git, gh and installs, on every turn, a resumed one too |
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
servers and plugins loaded, but without your CLAUDE.md, memory, skills or main agent, and so does the turn after one
that called a connector ("and the overdue ones?" names none); any other turn runs in safe mode without them. Before
that, the follow-ups to a ClickUp question ran without ClickUp: one read Ringo's own job logs for the earlier answer
(104k tokens, three minutes, a command to allow) and the other said the data was probably a day old. Measured with Claude Code 2.1.316, one trivial turn each: 16,567 tokens in safe mode, 27,522 with
the connectors, 57,799 with everything. Without your CLAUDE.md, an instruction in it to use Ringo cannot start Ringo
again either. Your own hooks do run in these turns, as in your normal Claude Code. Checked live through the same
flags: a read-only Jira question asked before each of its two Jira calls
and answered correctly (5 model calls, 18.5 s, 167,291 tokens, most of them cache reads).

Such a turn also gets one note from Ringo after your message: load only the tools it needs (ToolSearch with at most
5 results, since every loaded tool is re-read by each later call), and ask the service for only what answers it,
filtered on its side (your own items unless you say otherwise, only the dates and fields needed). A Codex turn gets
the second half. Without it, "please give me the clickup tasks for the week" loaded 30 ClickUp tools to use 3 and
read the whole workspace's 156 tasks due that week before asking for yours. Measured the same afternoon, one run each
(Claude Code 2.1.316, auto mode, read-only): that question 278,928 tokens without the note and 128,191 with it (8
calls and 6), both listing the same 5 tasks; "what clickup tasks are overdue?" 146,950 and 128,328, the first
answering for the whole workspace (49 tasks), the second for you (your 6, all in the first list, and one due that
morning, which the first missed). The note itself is about 70 tokens.

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
your repository, design sync, review reports, multi-agent `Workflow`, and messages to other sessions (`ListAgents`,
`SendMessage`). Their schemas are sent in full, so the shell denies them in every mode; subagents, search and the file
and shell tools stay. Claude Code's own skills go too (scheduling, loops, sending to other projects, plugin authoring
and 16 more were listed in every call, and no turn in Ringo's record had called one); a `/compact` turn keeps them,
since it is one of the commands. Codex turns lose its ask tool
and the collaboration-mode text that goes with it, and analytics; unless the task names a connector (or follows a
turn that called one) they also lose
plugins and apps (13 of the 17 skills listed, recommended plugins, and a computer-use tool that acts outside the
sandbox), and always image generation and goals (an aim Codex pursues by itself over many turns). One trivial turn
each (2026-10-31, Claude Code 2.1.316, codex-cli 0.165.0; the last two rows 2026-11-02, codex-cli 0.166.0):

| Turn | Before | After |
|---|---|---|
| Claude read-only turn (now read mode) | 16,547 | 11,984 |
| Claude turn that edits (now a plain line, or `@claude!`) | 17,133 | 12,570 |
| Codex turn, per call | 17,133 | 14,350 |
| Claude turn that edits, without Claude Code's skills | 12,432 | 9,320 |
| Codex turn, per call, without image generation and goals | 14,733 | 13,564 |

**Your own Codex instructions.** Codex reads `~/.codex/AGENTS.md` (or `$CODEX_HOME`'s) into every turn, and Claude's
safe mode has no counterpart for it. When that file sends Codex to Ringo, as a Ringo user's often does, a turn in the
shell followed it: asked to add one function to a file and check it, Codex read Ringo's 25 KB guide and its objectives,
started a Ringo review inside the turn and used 283,594 tokens. So when the file mentions Ringo, the shell tells Codex
in one sentence that Ringo launched it, so it should not read Ringo's guides or run Ringo, and should check its work
as it normally would: the same task then took 55,966 tokens, with the same result and none of those steps. Not when
your Codex settings give `developer_instructions` of their own, which the sentence would replace. The file itself is
still sent; a private `CODEX_HOME`, as the pipelines' calls have, would drop it, but turns would lose your Codex
settings and conversations.

Not yet: Codex cannot stop to ask, so it does not take ask mode's prompts. Its app-server protocol has the same
three answers (`accept`, `acceptForSession`, `decline` for command, file-change and permission approvals) and could
bring them, but it is marked experimental. agy keeps the deny rules described under Safety model.

### Ringo's own updates

Ringo updates itself the way Claude Code does. When the shell opens, and then once an hour, it asks GitHub for the
newest `main` in the background: one `git fetch` into Ringo's own copy of the history in its state folder
(`install/repo.git`). It never uses your checkout for this. If there is a newer version, Ringo builds it at low
priority into a folder of its own (`install/COMMIT`, with its prompts and pipelines). All versions share one build
folder, and only one builds at a time, so only Ringo itself compiles again. The shell then says
`Ringo updated to 1a2b3c4 (2 new changes) · /restart to apply`, and the hint by the input box says `update ready`.
That line scrolls away and the hint gives way to others while a turn runs, so the head of the screen also says
`↑ update ready · /restart` until you do.

The update is put where the running `ringo` is when the shell ends or restarts. Until then the open shell, and the
pipelines it starts, stay on its own version. `/restart` (once no job runs) starts the new version in the same
terminal, and what the screen showed comes back as plain text. Each agent's conversation goes on as it would have
without the restart: Ringo hands on how full each conversation is and its own record of it. On its first turn, each
agent is told the turns it had not heard yet, or, if its conversation is past 64k, starts fresh from the record (see
[One conversation, and a fresh start past 64k](#one-conversation-and-a-fresh-start-past-64k)); a later fresh start
still begins with the turns before the restart. Until 11/07 the size was not handed on, so a long conversation went
on after every restart: two turns on 11/04 re-read 98k and 155k tokens at every call. Ringo keeps the three newest versions and any version an open
shell used in the last three days, and removes the rest. Build output is in `install/build.log`.

Ringo leaves some builds for you to update yourself:
- one built outside git, or with uncommitted changes to what goes into the binary (`ringo --version` then shows no
  commit);
- one whose checkout has moved on since it was built;
- one from a commit that is not on `main`;
- one run with `RINGO_HOME` set.

These are told as well, when `main` has changes they lack: `Ringo 1a2b3c4 is out (2 new changes). This build does not
update itself (it was built with changes of its own): git pull in ~/Ringo, then cargo install --path cli there`. The
head then says `↑ update available · /update`, and `/update` says the line again. A build with no commit is compared
from where its checkout is, and a branch that already holds main's newest hears nothing. Until 2026-11-10 these
builds were told nothing: on one machine the shell had been installed by hand from a checkout with changes of its
own, and the newest version its updater had built was a week behind `main`.

`RINGO_NO_UPDATE_CHECK=1` turns the check off. Updating needs git and cargo (a `cargo install` needed them already).
No model call is made, and the release gate still decides what the pipelines run (next section).

### The pipelines' release

Once a release is active ([RELEASES.md](RELEASES.md)), `/review`, `/design`, `/research`, `/work` and plain lines
run that release, not the files of the Ringo you are using. An update to Ringo leaves the pipelines where they were.
When the shell opens and the active release is behind this Ringo, it says so. `/update` then checks the new one in
the background:
1. It stages this Ringo as a release (`release update`). Staging activates nothing.
2. It runs the offline speed check against the active release (`bench_latency.py`, `active-fixture` mode, 100
   pairs of each workload). The backends are simulated, so no model is called. It takes about 3 minutes.
   It times processes, so it starts only when no job runs (and after an agents' update), and new jobs wait
   until it ends. Plain lines queue as usual.
3. It shows the result, for example `0 slower, 3 negligible, 6 uncertain; averages moved -1.5% to +0.9%`.

`/update approve` activates the new release, recording you as the approver. It also records, as your reason for
accepting any uncertain workload, the result the shell showed you. A slower release is never offered, and
activation refuses it in any case. The next pipeline run uses the new release; one already running keeps its own.
The release before stays available: the shell prints the `ringo release rollback` command that goes back to it.

Only `suite.json` and the raw `execution.jsonl` are kept, in `checks/NEW-ACTIVE/` in Ringo's state folder. Asking
again, or after a restart, reuses them instead of measuring again. The per-run output (about 28 MB) is removed.
With no release active, the pipelines run this Ringo's own files, and `/update` says nothing about them.

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
appends the repository's own `CLAUDE.md` or `AGENTS.md`. Your own rules for every Claude turn can go in `rules.md`
in Ringo's state folder (`~/.local/share/ringo/rules.md` on macOS and Linux): when that file has text, it is appended
ahead of the repository's file. Every line in it is read on every call, so keep it short; with no file nothing is
added. Codex reads your `~/.codex/AGENTS.md` itself.

A follow-up re-sends the conversation so far, so it costs a first turn plus that history, and within a turn every
call re-reads everything before it. On a subscription login Claude runs Opus with a 1M window, where Claude Code
compacts a conversation only near 967k tokens: one long turn started in a home folder read 3.9M tokens in all, its
last calls 140k each. So the shell runs Claude with `CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000`, the window Claude Code
compacts at on a 200k model, and Claude compacts by itself when a conversation passes it, mid-turn as on resuming.
Codex compacts near its own 475k window (one long turn here read 2.9M tokens, its calls still growing past 137k), so
it runs with `-c model_auto_compact_token_limit=200000` too, unless your `config.toml` sets the key. A compaction
takes a minute or more (82 s for 171k tokens, measured), and the turn's row says "compacting the conversation" meanwhile.
Compaction keeps the system prompt (with the files appended above) and re-reads the five files last changed; the
rest of the history becomes Claude's summary. Set the variable yourself to choose another window (100000 to
1000000). `/compact` still compacts at once. A short exchange adds little, but start a write job (`@codex!`) for an
unrelated task, or `/separate`. (`--bare` would cut more, but it signs in with an API key only, so the shell does not
use it.)

agy's floor is its own: it declares 60 tools, about 20 of them for the browser, whatever the deny rules, mode or
sandbox. Its startup event lists them, and no setting Ringo can pass removes them.

The program itself: a 3.2 MB binary (the pipelines included) that starts in 2.1 ms and idles at 2.5 MB of
memory with no measurable CPU. A native pipeline peaks at 8.1 MB, against 24.5 MB for the same run under Python
(macOS, Apple silicon). It redraws only on an event, a key,
or for the spinner while a job runs.

### One conversation, and a fresh start past 64k

Every turn in the folder belongs to one conversation, whichever agent takes it. An agent continuing its own is sent,
ahead of the owner's message, Ringo's record of the other agents' turns it has not been told; an agent starting one
(its first turn, or after `/clear`) is sent the record of the whole conversation. Agents work side by side, so a
turn can end after a later one began: Ringo keeps which turns each agent has been told, never a place in the list, so
each turn is told once and none is missed. A turn that starts while another agent's runs here is also told, in one
line each, what that turn was asked and which files it has edited so far, so it leaves that work alone; it hears how
the turn ended at its next turn. When an agent's window passes
64,000 tokens (its latest call's input and output, as Claude's message usage and Codex's `tokenUsage.last` report
it), its next turn does not resume: it starts fresh from the record of the whole conversation, if the record is at
most half that size. The turn's log says which of these happened and how big the record is.

Ringo's own code writes the record, from what it keeps of each turn, with no model call, and rebuilds it from the
originals every time, so it is never a summary of a summary:

- the owner's message, word for word (one past 4,000 characters is cut, and the cut said);
- the agent's reply: the newest whole (to 6,000 characters), older ones' first paragraph (to 600);
- the steps it took: every failed one (up to five) and, for the newest turn, its last five;
- the files it changed, and whether `/undo` took them back; where git cannot say (a folder with no `/undo`, such as
  the home folder, or another turn at work here that ran commands), the files it said it edited;
- the change the turns made, as `git diff` from before the first to after the last, in those files only.

The turns go in newest first, to about 60,000 characters (about 15,000 tokens); an older turn past that is named
only. The record uncut is `record.md` in the turn's job folder, and the turn is told where it is. Inside a long
turn, each agent's own compaction still applies.

Measured before release, in a pilot of 4 three-turn coding tasks for each agent with hidden checks, against
resuming as before, with the threshold at 12k so that later turns started fresh (all 8 of Codex's later turns, 3 of
Claude's 8): every run passed every check either way, 28 of 28 for each agent. Codex's later turns used 15% fewer
tokens (365k against 431k, fewer in all 8 pairs); Claude's differences were within run-to-run noise (its later
turns without a handoff varied by up to 29k between the two runs). The sample rules out a large loss of accuracy,
not a small one, and at 64k the record covers longer histories than the pilot's did.

The agents' own compaction is a model call that summarises the whole transcript. Simple deterministic records have
matched it in published work (observation masking matched LLM summaries at about half the cost on SWE-bench Verified,
JetBrains 2025), and summaries of summaries lose rules.

Two agents' turns can change the folder at the same time, together or separate. A turn's undo then keeps only the
files it reported editing (Claude's Edit and Write, Codex's file changes) that the other turn did not, and none when
the other ran a shell command (Claude's Bash, a Codex command that did more than read), which may have written any
file without saying so. The rest are named in its log and stay out of `/undo`, so one agent's undo never takes
back the other's work. The limit: a file only a command wrote is left out, and an edit the other turn made just
before this one ended may not be known yet.

## Tiers

| Tier | Claude | Codex | agy | and |
|---|---|---|---|---|
| `quick` | `--effort low` | `-c model_reasoning_effort="low"` | `--effort low` | |
| `standard` | nothing: each agent's own default and the owner's settings (a Codex `config.toml` that sets an effort keeps it) | | | |
| `deep` | `--effort xhigh` | `"xhigh"` | `--effort high` | |
| `ultra` | `--effort xhigh` | `"xhigh"` | `--effort high` | Ringo's review of each turn here that changes files, then one revision turn when it finds defects |

A tier changes no prompt: effort is a model setting, not text. The shell starts in `standard` every time, so a tier
that spends more is never left on from an earlier day. `deep` is xhigh, not high, because an owner's own settings can
already run Claude at xhigh (Claude Code reads `effortLevel` from its settings under `--safe-mode` too), and high
would lower it; a setting of max is lowered to xhigh while `deep` or `ultra` is on. agy gets high: its default model
has no xhigh, and given it every turn failed at once (agy 1.4.12, gemini-3.8-flash).

`ultra` builds on the one extra spend Ringo measured to raise quality. On COMPLEX-01, checks alone passed 3 of 11
seeded candidates, and one review plus one revision passed 11 of 11. Higher effort alone measured no gain on the
tasks tried (high at 54,275 tokens against low at 21,754, both 26 of 26), and neither did more reviewers or more
rounds (see README). So after a turn in the folder that changed files, the shell runs `/review #N` (the review
pipeline, one call, about 70k tokens of material at the median). Outside a git project the turn's before and after
are commits in Ringo's undo store, and the review reads them there: it reads only git objects (`ls-tree`, `cat-file`,
`diff`), and both runtimes build the same material from a bare store as from a project. The call goes to the
session's backend unless that plan has a limit used up: as an ultra turn starts, the shell reads the plans the way
`/usage` does (about a second each, no model call; the other plan only when the first has no room), and a used-up
Codex plan sends the review to the `claude` preset, a used-up Claude plan to `codex-chatgpt`. A plan it cannot read
counts as having room, and a backend of your own is used as it is. When neither plan has room the shell says so while
the turn runs, and no review follows it. Lines sent meanwhile wait;
Esc or Ctrl-C stops the review and gives them back to the input.
A review with findings queues one turn for the same agent, which continues its conversation and is handed the
review (`{#R}`); no review follows that turn. Where `/undo` is off there is no before and after to review, and
`/tier ultra` says so. The revision is Ringo's line, so the conversation does not show it as yours. Claude Code's own
`ultracode` setting is not used: it runs Claude Code's dynamic workflows, which
`--safe-mode` turns off and which Ringo does not measure.

## Models

Each person's plan includes different models, so which model does what is theirs to set. A turn runs on its agent's
own default until `/model AGENT NAME` names another:

- **Claude:** `--model NAME`, an alias (`fable`, `opus`, `sonnet`, `haiku`) or a model's full name. Claude Code has
  no list of a plan's models to read.
- **Codex:** `model` in the thread's `thread/start` and `thread/resume`, so a conversation under way goes on with the
  new one. `/model` asks its app-server what the plan offers (`model/list`, no thread and no model call; 0.3 s for 8
  models, codex-cli 0.167.0) and prints each name with what OpenAI says of it and which is the default.
- **agy** and agents you add with the plain adapter choose their own: `/model` refuses a name for them and says so.

The choice is one word that goes to the agent's own program as an argument (letters, digits and `. _ - : / [ ]`); it
is kept in `shell-settings.json` (`"models": {"claude": "opus"}`) and applies from the agent's next turn, a running
turn keeping its own. A conversation under way moves to a newly named model as it is. Going back
(`/model AGENT default`) names none, and a conversation resumed with no model named keeps the one it was last
given, so the agent's next turn starts its conversation again from Ringo's record of it (see
[One conversation](#one-conversation-and-a-fresh-start-past-64k)). Ringo does not check a name against the plan: the agent does, when the turn starts. A turn
that fails on a chosen model ends with `it ran on NAME, your choice for claude: /model claude default goes back to
its own`. Each turn's first line names the model its agent reports (` claude  #3 · claude-opus-5-5`), chosen or
not, so which model did a piece of work is on screen and in `usage.jsonl`.

The pipelines' model is the backend's: `/backend claude@opus` or `/backend codex-chatgpt@MODEL/EFFORT` sets it for
`/ask`, `/review`, `/design`, `/research` and ultra's review. So turns and reviews can run on different models.

Checked live, one trivial read-mode turn each (`cargo run --example model -- REPO AGENT MODEL`; Claude Code 2.1.322,
codex-cli 0.167.0, one run each, so the times are an instance and not a rate):

| Asked for | Ended | Time | Tokens | Model the agent reported |
|-----------|-------|------|--------|--------------------------|
| claude `sonnet` | done | 4.4 s | 8,541 | `claude-sonnet-5-5` |
| codex `gpt-6-luna` | done | 2.9 s | 12,643 | `gpt-6-luna` |
| claude `no-such-model-xyz` | failed in 1.1 s | | 0 | none: "There's an issue with the selected model (no-such-model-xyz). It may not exist or you may not have access to it." |

A name the plan does not have costs no tokens: the agent refuses it before any model call.

The same conversation, one more trivial turn after the first (`... MODEL then=OTHER` and `... MODEL then-default`):

| First turn on | Then | The second turn ran |
|---------------|------|---------------------|
| codex `gpt-6-luna` | changed to `gpt-5.6-luna` | `gpt-5.6-luna` |
| claude `sonnet` | changed to `haiku` | `claude-haiku-5-5` |
| codex `gpt-6-luna` | no model named | `gpt-6-luna` still |
| claude `sonnet` | no model named | `claude-sonnet-5-5` still |

The last two rows are why going back to the default starts the conversation again.

## Usage

`/usage` adds no tokens. It shows the tokens that went through Ringo, then each plan's limits, as gauges:

```
  tokens through Ringo · this session 0 · last 24 hours 8.64M · last 7 days 8.64M
    claude ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰ 8.64M  in 7 days · 8.64M in 24 hours · 0 this session
    codex  ▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱    0  in 7 days · 0 in 24 hours · 0 this session
  asking claude and codex for your plan's limits…
  claude · max plan
    5 hours       ▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱  12%  starts over in 3 h 40 min
    week          ▰▰▰▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱  64%  starts over in 2 d 7 h
  codex · plus plan · 2 full resets left
    5 hours       ▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱  14%  starts over in 4 h 0 min
```

A limit's gauge is green under 60% used, amber to 85% and red past it; an agent's token gauge is its share of the
week's, in its colour. A wide terminal's side panel keeps a small gauge for every limit (`claude 5h ▰▱▱ 12%`),
read when the shell opens and again after a turn when the last read is ten minutes old: no tokens either.

- **This session:** the jobs in memory, by agent.
- **The last 24 hours and 7 days:** from `usage.jsonl` in Ringo's state folder. Each finished job that used tokens
  appends one line: when, which agent (or `ringo` for a pipeline), and its input, cache-read, cache-write and output
  counts; and what they are counts of: the tier, the model when the agent names it, a pipeline's command, and the
  job's folder (a pipeline's trace there names each call's model and effort). A job that finished without reporting
  any count appends a line with `"unknown": true` and no counts: unknown is not zero, and the sums leave it out.
- **Plans:** asked of each set-up agent in the background, about a second each:
  - **Claude:** Claude Code's `get_usage` control request (2.1.316), sent to a print-mode session that is never given
    a message, so no model call (its answer says cost 0). Claude marks it experimental. When it fails, or succeeds
    with no limits in it (its own read of the plan failed), the last `rate_limit_event` a Claude turn in this session
    sent stands in.
  - **Codex:** `account/rateLimits/read` over `codex app-server` (no thread is started): each window's used percent,
    length and reset time, and the full resets left. `account/rateLimitResetCredit/consume`, which spends one, is never
    called.
  - **agy:** it reports no plan usage.

A resumed Claude turn counts its own tokens: `model_usage_since_last_result`, whenever it is there (an empty one is a
run with no model call), comes before `modelUsage`, which on a resumed session is the whole session's so far. One
second turn was counted at 452,900 tokens before this fix; it used 433,179. A Codex turn that fails or is stopped
before `turn/completed` is counted from its last `thread/tokenUsage/updated`, so it reaches the record too.

## Safety model

- **Permission prompts:** Claude's prompt is one stream-json message on stdin, which stays open until the turn's
  result: Claude Code sends each permission request to Ringo (`--permission-prompt-tool stdio`, the channel the
  Agent SDK uses; `--permission-prompts host` alone denies them) and Ringo writes your answer back. A request Ringo
  does not serve is refused at once, so a turn never waits on it. Codex and agy cannot ask; a request Codex's
  app-server sends anyway is refused at once in the same way.
- **Turns in your folder** run where you started `ringo`, continuing the agent's conversation, one at a time for
  each agent: Codex
  with `workspace-write` (edits and commands inside the folder, no network; in auto mode the network and Codex's own
  reviewer for its approval requests), Claude with `acceptEdits` in ask mode and auto mode in auto mode (edits in the
  folder allowed; any other command waits for you, or for its review), agy with `--mode accept-edits` and
  session-only deny rules for `command(*)`, `unsandboxed(*)`, `mcp(*)` and `execute_url(*)` (edits allowed;
  commands, MCP tools and browser actions refused, as it cannot ask). Grok and agents you add have no sandbox Ringo
  has verified, so they only work in a copy, and a line for one goes there.
- **Undo:** in a git project, Ringo takes the folder's files (every file git does not ignore, untracked ones too) as
  a git tree before a turn that may edit and again when it ends, with a private copy of the index: your index, stash
  and HEAD are never touched. The files that differ are the turn's change. `/undo` first compares those files with
  how the turn left them, and any difference (an edit of yours anywhere in one, even far from the turn's lines) makes
  it refuse and change nothing; then it applies the reverse patch with `git apply`. Neither `/undo` nor `/apply` runs
  while a turn works in the folder, whose own after would take them in. Outside a git project the same trees go
  into a git store of Ringo's own, one per folder, under its state folder (`undo/`), run with `--git-dir` and
  `--work-tree`: the folder gets no repository, and an enclosing dotfiles repository never sees the store. The store
  keeps its own index, so only new or changed files are hashed. It ignores the folder's `.gitignore` entries and
  `node_modules`, virtualenvs and caches. Left out: the home folder itself, a folder holding Ringo's state, a folder
  with a git project inside it (git would keep a link to that project's commit, not the files a turn edits there),
  and a folder with more than 5,000 files or 200 MB (`git init` there turns undo on); Ringo says which when it opens. A
  folder moved or renamed starts a new store, and stores are never pruned; deleting `undo/` reclaims the space.
- **Commits and PRs** a turn makes are yours, as ones made by hand are: Claude turns pass `"attribution":
  {"commit": "", "pr": ""}`, so no co-author trailer and no "Generated with" line (Claude Code 2.1.316; it also drops
  the attribution reminder from every request, 626 bytes). Codex 0.165.0 adds none (`codex_git_commit` is removed),
  and agy has none.
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
- **Limits:** three jobs at a time, and one turn of each agent's at a time in your folder; a line for an agent
  working there goes into its turn (Claude, Codex) or waits for it (agy), and the shell refuses a fourth job.
- **State:** `~/.local/share/ringo/shell/` (or under `RINGO_RELEASE_HOME`), owner-only. Each job folder keeps its
  task, raw events, reply, stderr and patch. Logs can contain your code: delete them yourself when done.

## For the IDE

The shell is a thin front end over the `ringo_cli` library: `Session` launches and stops jobs, `pump()` folds their
events into `Job` records (status, activity, tokens, subagents, touched files), and `diff` / `apply` / `drop_worktree`
/ `overlaps` handle changes. A Rust IDE links the library and renders the same records.

## Not yet

A JSON-lines mode for a non-Rust front end, and measured repository-level comparisons of agent turns against the
pipelines.

Route suggestions (`/route on`: word rules that put the matching command in the input for a line that read like a
task; acceptable 35 times in 38 on 100 held-out lines, covering about half of the tasks) were removed when a plain
line became an agent turn in the folder: there was nothing left for them to suggest.

Lookups by a lighter agent (2026-11-10) were built, checked live and removed. A plain line that only asked where
something is in the code (word rules, no model call) went to agy as a turn that only read, in every mode, and was
asked to name each place as `path:line`; `/look TEXT` asked for one outright. One run each of "where is the retry
count configured" in a four-file repository, through that same read-only turn (agy 1.5.3 to 1.5.5: it updated itself
meanwhile; Claude Code 2.1.322, codex-cli 0.167.0):

| Agent | Time | Tokens | Answer |
|---|---|---|---|
| agy | 53.4 s | 32,687 | none: it started a subagent and ended its turn with "waiting for results" |
| agy, told to use its own read tools and no subagent | 70.1 s | 183,863 | right, 2 places |
| Claude | 10.7 s | 34,481 | right, 4 places |
| Codex | 14.6 s | 41,519 | right, 4 places |

agy began with `find` and `ls` as commands, which Ringo refuses it because it cannot ask. Told to read for itself, it
opened files by guessed paths (9 of its 14 reads failed) over 15 calls, and none read from a cache: 8,559 to 13,448
input tokens each. So a lookup sent to agy was slower and larger than one left with the agent you talk to. The word
rule also took none of the 113 plain lines in one owner's shell history, and none of them was a lookup: they were
tasks, answers and follow-ups. One run each on a toy repository is enough not to adopt it and is not a measure of
agy. An agy turn in your folder that has to search meets the same refusals. To try again: a lighter agent that reads
from a cache between calls and can search without a command, or a lighter model of Claude's or Codex's own, measured
the same way.

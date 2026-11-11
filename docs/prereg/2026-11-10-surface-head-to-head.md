# Preregistration: the same tasks through three surfaces (Claude Code alone, Claude Code driving Ringo, the Ringo terminal)

Registered 2026-11-10, before any session on the tasks. The commit that adds this file fixes the arms, the tasks, the
order, the measures and how they are read. Any change after it is a deviation, recorded at the end of this file with
its reason. It replaces an unrun design of 2 November that compared the terminal with each agent alone and had no arm
for Claude Code driving Ringo.

## Why

The owner's logs show the terminal spending fewer tokens per model call than Claude Code does when it drives Ringo
(15.8k against 40.9k weighted tokens, at a median context of 68k against 258k). Those are different tasks on each
side, and nothing in them says whether the work is as good. This is a first paired comparison: the same tasks, the
same model, three surfaces.

## Arms

Claude Fable 5.2, effort xhigh, Claude Code 2.1.322 and permission mode auto in all three.

- **stock.** Claude Code alone in print mode with `--safe-mode`: none of the owner's instructions, memory, skills,
  main agent or connectors. Safe mode still lists Claude Code's 19 bundled skills and six plugins, two of which the
  owner installed; the terminal's turns list the same six. No Ringo.
- **cc-ringo.** Claude Code in print mode with the owner's setup as it is: their instructions (which name Ringo as the
  default workflow for substantive work), main agent, skills, plugins and connectors. Whether it launches Ringo on a
  task is its own decision and is a result.
- **terminal.** The `ringo` installed on the owner's machine on 10 November (binary sha256
  `9fc2b9f46d141b6234f11544ebff7686c2d7d604fa9744069f2acce2e81d0c34`),
  auto mode, tier ultra, every turn typed as `@claude <turn>`. Ultra runs the agent at xhigh, then has Ringo review
  each change a turn makes and gives the agent one revision turn when the review finds defects. The terminal passes
  no model; it is set from the environment (`ANTHROPIC_MODEL`) and read back from each session's records. The
  pipelines are the active release (`496afe0b9bf282bdb777d1858f38659c5369de1b3053bba8cee58d90df6afac8`).

  That binary was built from commit `f36fe74` with uncommitted changes (image attachment, and a rewritten user
  guide that the binary embeds), so no commit names it exactly and its hash is what fixes it. It is 23 commits
  behind `main` (`ffdd253`) and lacks pull requests #54 to #56. The run uses a copy of the binary, so a reinstall during the run cannot change the build
  under test, and gives each session its own copy of the terminal's state folder (the active release, the settings
  and the list of agents, without the owner's history, usage log or earlier sessions).

The model, the effort and the review step are the same kind in cc-ringo and terminal. They differ in what surrounds
the agent: the instructions and tools it is given, who starts the review, and when the conversation is compacted (the
terminal sets 200k; the two Claude Code arms keep Claude Code's default; every session starts with that setting
absent from its environment). This build of the terminal also starts the
agent afresh from its own record of the conversation when a turn begins past 64,000 tokens of context; turns where
it does are reported.

## Tasks

Six small Python tasks written on 2 November 2026 for this comparison and kept private: 19 to 152 lines in 4 to 7
files, two or three turns each (13 turns in all). Each has hidden checks that fail on the starting code where the
task changes behaviour and pass on a reference solution. None has been run by any arm, and no Ringo change has been
tuned on them. They are fixed by a SHA-256 over each folder's file paths and contents: tasks
`b0bf78442673f30edad5c958ca2afb4ec8ec4f0b8b212cd843941221dd1dd8fe`, hidden checks
`7ae62993025bf691001dd5770d085bd93b0e8620a1c4be48e934c734c09ac592`. Each arm gets its own fresh copy of each task as its own git repository.

## How sessions run

A script sends every turn; no person and no agent is in the loop. One session at a time, the tasks in a fixed order
and, within a task, the three arms in an order drawn with seed 20261110. A turn is sent word for word, the next only
when the last has finished. Every session starts from an environment that carries nothing of the session the script
was started from.

- stock and cc-ringo: one `claude -p` process a turn, the first with a new session id and the later ones resuming
  it.
- terminal: the real `ringo` on a pseudo-terminal. A turn has finished when the agent's turn, the review and any
  revision turn have all finished.
- A permission request that reaches the terminal is denied and counted. In print mode nothing can be asked, and what
  auto mode blocks is refused; denying in the terminal gives the three arms one rule.
- A turn still running after 30 minutes is ended; the session stops there and is scored as it stands.
- A session in which the agent's process, or the script that drives the terminal, ends in an error other than that
  time limit is kept as a first attempt and run once more from a fresh copy of the task. The second attempt stands
  whatever happens. Both are reported and both count toward the budget. A review call that fails is not such an
  error: the surface carries on without it, and it is counted.
- The hidden checks are run by the scoring script after the last session. No session is started in, or shown, the
  folder that holds them.

## Measures

- **Quality.** Tasks fully passed (every hidden check), and hidden checks passed in total.
- **Cost.** Weighted Claude tokens per task: input + 0.1 x cache reads + 1.25 x cache writes kept 5 minutes + 2 x
  cache writes kept an hour + 5 x output. They are summed from the `result` records of every Claude Code process in
  the session, which carry the final counts; Claude Code's own transcripts are the cross-check for the two print-mode
  arms. Codex tokens of Ringo's review calls are reported beside them (uncached + 0.05 x cached + 5 x output) and
  never added to them: in the terminal from the summary each review job writes, in cc-ringo from the summary line
  each Ringo launch prints or the trace folder it names. Token volume (every token, unweighted) is reported beside
  both.
- **Time.** Seconds from the first turn sent to the last turn finished.
- **Also.** Model calls, context per call (median and largest), Ringo launches, permission requests refused, and
  both plans' usage windows read before and after every session (no model call).

## Analysis

The terminal against cc-ringo is the comparison asked about. The terminal against stock and cc-ringo against stock say
what each adds over the agent alone. Per pair over the six tasks: fully passed, exact McNemar; weighted Claude
tokens and seconds, a one-sided sign test and the geometric mean ratio. Six tasks cannot show that two arms are
equal. Everything is reported, including tasks where the terminal does worse.

## How the result is read

- **The terminal holds:** it fully passes no fewer than cc-ringo minus one task, passes at least 90% of the hidden
  checks cc-ringo passes, and spends fewer weighted Claude tokens than cc-ringo on at least 5 of 6 tasks (sign test
  p = 0.109 at 5 of 6, 0.016 at 6 of 6). The owner's trial of the terminal on real work then starts.
- **The terminal loses quality:** it fully passes two or more fewer tasks than cc-ringo. For each such task both
  records are read and what the terminal lacked is named. Each is a candidate change, measured before it is adopted.
- **No cost gain:** the terminal is not cheaper on 5 of 6. Reported as it is.
- **Ceiling:** if every arm fully passes 5 or more tasks, quality is reported as "no difference detectable at this
  task size", not as "nothing is lost".

Nothing is adopted from this experiment. Which surface the owner uses, and what their instructions say, stays theirs
to decide.

## Budget and stops

- At most 15,000,000 weighted Claude tokens over the 18 sessions and, for Ringo's review calls, 40 calls and
  2,000,000 uncached Codex input tokens. The run stops before a session when the total so far plus the largest session
  of that arm so far would pass a ceiling.
- The run stops before a session when a plan window is at or over: Claude 5-hour 80%, Claude weekly 85%, ChatGPT
  5-hour 70%, ChatGPT weekly 75%. Sessions are independent, so it resumes after a reset; a resumed run is not a
  deviation.

## Tooling check before this file (not on the tasks)

Two trivial turns per arm in a throwaway folder, to confirm that the model and the mode are the same in the three
arms, that each arm's tokens are found, and that the terminal's review step runs.

| Arm | Model, mode | Loaded | Context per call | Weighted Claude tokens, two turns |
|---|---|---|---|---|
| stock | `claude-fable-5-2`, auto | 24 tools, 19 bundled skills, no connectors | about 18.7k | 31,161 |
| cc-ringo | `claude-fable-5-2`, auto | 202 tools, 190 skills, connectors | about 52.4k | 161,766 |
| terminal | `claude-fable-5-2`, auto, ultra | 10 tools, no skills, no connectors | about 10.5k | 13,210 |

Every arm made the one-line edit, and no permission request was refused. The terminal's review of that edit ran the
active release as one Codex call (1,401 input tokens, 65 output), found no defects, and so gave no revision turn.
Claude Code's transcripts matched the result records in both print-mode arms. cc-ringo did not launch Ringo on
these turns, so its count of Ringo launches has been checked on made-up records only.

The script that types into the terminal was first run against stand-in agents (no model calls): 23 checks of its
own, then an attempt to break it that found five defects, all fixed. Among them, a failed turn was reported as
finished, and quitting with a permission request open would have answered yes. All 13 task turns reached the
stand-in byte for byte.

One Codex review of this file before it was committed found that the stock arm was described as loading no skills
or plugins when it lists the bundled ones; the description above is the corrected one.

## Limits known now

- Small tasks. Every arm may pass everything, and sessions stay far below the context sizes where the owner's
  Claude Code cost was measured, so a cost gap is likely understated.
- cc-ringo runs without the owner's memory notes. They are kept per project folder and a task folder is a new one.
- All three arms run Claude Code in print mode. None has a person steering.
- One run per task and arm. Run-to-run variation is not measured.
- Weighted tokens depend on whether a session's opening context is already in the provider's cache, which depends
  on what ran in the hour before, the owner's own sessions included. Token volume and context per call do not, and
  are reported beside.
- The terminal is the build installed on the owner's machine today, which is older than this repository's main
  branch and cannot be rebuilt from a commit alone.
- The owner's other use of both plans continues during the run. The usage windows read around a session are stop
  rules, not measures of the session.
- The terminal's reviews are started by launcher code in the checkout the binary was built in and run the active
  release's pipeline. Each review records the release it ran, and the checkout's commit and a hash of its
  uncommitted code are recorded around every terminal session; a change of either during the run is a deviation.

## Deviations

1. 10 November, during the first session, before any session had finished and before any hidden check was run. The
   Claude plan's 5-hour limit ended the first session (cc-ringo on the first task) seven minutes in, and its one
   rerun started while the limit still applied and ended at once. By the rule under "How sessions run" the second
   attempt would stand. It does not, because a session that the plan's limit ended says nothing about the arm. From
   this point a session ended by a usage limit (the provider's status 429) is set aside, and the task is run again
   from a fresh copy when the window has room. It is kept on record, counts toward the budget and does not use up
   the one rerun. The two sessions set aside so far spent 435,683 weighted Claude tokens and one Codex call. The run
   now also stops if a session did not start on the fixed model and mode.

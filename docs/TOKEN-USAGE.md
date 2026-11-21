# Token usage reporting

Default reporting uses token usage rather than dollar estimates. Existing runtime
summaries already provide the required counts; this reporting preference adds no
provider calls, tokenizer dependency or normal-path processing.

| Summary field | Report label |
|---|---|
| `input` | Uncached input tokens |
| `cache_read` | Cached input tokens |
| `cache_write` | Cache-write input tokens |
| `output` | Output tokens |
| `reasoning_output` | Of `output`, the tokens spent on reasoning (Codex reports them; absent when no call did) |
| `weighted` | Cost-weighted usage (W) of the run's paid calls, per provider unit; see [Cost-weighted usage (W) and the ledger](#cost-weighted-usage-w-and-the-ledger) |

The reported total is `input + cache_read + cache_write + output`;
`reasoning_output` is already inside `output` and is never added to it. These are
Ringo's normalized fields: native Codex usage includes cached tokens within raw
input, so Ringo subtracts that subset from `input` and records it in `cache_read`.
Do not add raw input to normalized cached input and count the cache twice.

Include every attempt that reports usage, including retries, malformed replies
and failed calls. Report call count and elapsed time alongside tokens. If
`untokened_calls` is nonzero, label the total **partial** and state how many
attempts lack usage. Do not estimate missing usage from text length or treat it
as zero consumption. Tokens from different model tokenizers are model-specific;
record the backend/model when comparing efficiency. Each call's trace record
names the `backend` and the `model` and `effort` Ringo asked for; `null` means the
CLI's default, which is not recorded, so pin both (`codex-chatgpt@MODEL/EFFORT`)
for runs that will be compared. `elapsed_s` covers the whole task, the findings
export included, and a failed run reports it too.

## Cost-weighted usage (W) and the ledger

Two numbers describe a task's consumption. **T** is raw tokens: `input + cache_read + cache_write + output`, with
failed attempts counted. **W** weighs the same four counts by what each kind costs the owner's plan. They can
disagree: an offline replay (see [Long conversations](#long-conversations)) found a change that cut raw input by
about 3% while raising credit-weighted cost, because cached tokens are cheap and uncached ones are not. Reporting T
alone could pass a change that costs more plan quota, so reports carry both. W adds no model call, and nothing Ringo
decides or sends depends on it.

**Weights.** One table per runtime (`harness.py`, mirrored in `cli/src/rt/harness.rs`, kept equal by the parity gate),
held as integers over a divisor so both runtimes add exactly the same numbers.

| Provider and model | Uncached input | Cached input (read) | Cache write | Output | Unit |
|---|---|---|---|---|---|
| Codex, GPT-6.1 Sol | 50 | 2.5 | 50 | 250 | credits per million tokens |
| Codex, GPT-6 Luna | 2.5 | 0.25 | 2.5 | 12.5 | credits per million tokens |
| Claude (any model) | 1 | 0.05 | 2.0 | 5 | weighted tokens: multiples of the uncached input price |

- The Codex rates are the published ones in [EXPERIMENTS.md](EXPERIMENTS.md) ("Cost"). OpenAI publishes no cache-write
  rate, so a Codex `cache_write` (normally 0) is charged at the uncached rate.
- The Claude weights are relative to each model's own uncached input price, so they do not depend on the model. The
  cache write uses the 1-hour figure (2.0, not the 5-minute 1.25) because the code already assumes a 1-hour cache for
  Claude Code subscriptions (`CLAUDE_TTL_S = 3600` in the context policy). Reasoning tokens are already inside `output`.
- The model is matched exactly. In a pipeline it is the one Ringo asked for (`--model`, `codex-chatgpt@MODEL`); a call
  on the CLI default names none. A shell row's model is the one the agent itself reports, when it reports one (Codex
  does, so shell Codex rows carry their default model). A Codex model not in the table, or a call that names none,
  makes W **unknown**: it is `null` and the model is listed (`"(not named)"` for none). Ringo never guesses a price. A call without complete counts is
  unknown too.
- W is in credits (Codex) or weighted tokens (Claude), never in dollars, and never summed across providers: a run
  that used both reports `unit: "mixed"` with `value: null` and keeps each in `by_unit`.

**In a pipeline run.** Every paid attempt's `usage` in the trace names the `surface` (`cli:claude`, `cli:codex`,
`local:ollama` or `plain`), the `billing` (`subscription`, `local` or `unknown`) and `batch` (`false`: there is no
batch path yet). They are added to a copy; usage that was `null` stays `null`, and a record that names no backend is
left as it was. The run summary (state.json, summary.json, the `harness:` stderr line, `work`'s report) ends with

```json
"weighted": {"unit": "codex_credits", "value": 80.0, "unknown_models": [], "unweighted_calls": 0,
             "by_unit": {"codex_credits": 80.0}}
```

`value` is `null` when no paid call ran on a weighted surface, when any such call was unknown, or when there is more
than one unit. `unweighted_calls` counts paid attempts on surfaces without weights (plain commands, Ollama); they are a
different unit and are reported beside W, not inside it. `task-report.json` copies the block into
`consumption.weighted`.

**In the shell.** Each line of `usage.jsonl` gains these keys. Older lines lack them and every reader (`/usage`
included) reads old and new lines alike.

| Key | Written when | Meaning |
|---|---|---|
| `surface` | always | `cli:codex`, `cli:claude`, `cli:agy`, `plain` or `pipeline` |
| `billing` | always | `subscription` for the three CLIs; `unknown` for a plain agent and a pipeline (its per-call billing is in its trace) |
| `calls` | the job's model calls were reported | Codex and Claude report each model call; a plain agent reports none, so the key is absent, never 0 |
| `ctx_base`, `ctx_fixed`, `ctx_carry`, `ctx_growth` | the agent's base is measured | the split below |
| `record_inline_bytes`, `record_whole_bytes` | the turn started fresh from Ringo's record | the inline handoff text sent, and the size of `record.md` |

The context split is over each call's input side (what it read, cached or not: Codex `last.inputTokens`; Claude input
plus cache read plus cache write). With `first` the turn's first call and B the agent's fixed base: **fixed** is
`min(call, B)`, **carry** is `max(0, min(call, first) - B)` (the conversation the turn began with), **growth** is
`max(0, call - first)` (what the turn added). The parts can exceed the total when a turn's first call is below B.
B is the median of the session's last three measured bases (what a new conversation's first call read beyond the
message); it is per session and not persisted, so a shell that has not yet run a fresh read turn for the agent writes no
`ctx_` keys. The base itself is measured from input plus output, and the split uses input alone; the difference is one
call's output.

**The offline tool.** `ledger.py` reads files and makes no model call:

```
python3 ledger.py report --window 24h --window all     # by agent, surface, model and window
python3 ledger.py attribute --agent codex --base 13022  # the split, from the shell's events.jsonl
```

`report` shows per group the jobs, calls, the four counts, T, W and the fixed/carry/growth shares. A line from before
the ledger gets its surface from the agent's name and its calls are unknown; a pipeline line is replaced by one entry per
paid attempt in its `trace/trace.jsonl` when that exists (`--no-traces` keeps the line, `--trace DIR` adds a trace).
`attribute` re-splits the shell's saved events with the shell's own call rule and lists why each report that another
counting would take for a call was not one (`before_turn`, `other_turn`, `behind_or_malformed`, `no_last`, `repeat`).
It measures consumption; it makes no claim about what any change saves.

## Codex call overhead

A native Codex call carries more than Ringo's prompt. In codex-cli 0.164 the
stock request added the Codex agent prompt, code-mode and multi-agent tool
schemas, skills, permission and environment blocks to every call, about 52 KB per
request and roughly ten thousand input tokens, before Ringo's own text. A work
round with one writer and six judges paid it seven times. The `codex-chatgpt`
preset now removes it (see `docs/HARNESS.md`, **Codex children**); the measured
effect is in the [improvement log](IMPROVEMENT-LOG.md). The remaining cost of a
Council round is Ringo's own evidence, sent whole to each judge; `work` now runs the Council only when a task
opts in, and makes one review call by default.

## Long conversations

The shell's default handoff threshold remains 64,000 reported context tokens.
Between turns it starts a fresh native conversation only when the inline record
is estimated to be at most half the previous context. This does not interrupt or
bound a long agent turn already in progress.

For Codex that is also its in-turn compaction limit, so by default a resumed Codex thread is rarely started fresh
between turns (only when a large tool result overshoots the limit before Codex compacts). An earlier build used 48,000 for Codex; an offline replay of the owner's traces (an estimate) found it
cut raw input by about 3% but raised credit-weighted cost, since a fresh start trades cached carry for an uncached
record. A turn policy's `handoff_at` is used exactly. `RINGO_CONTEXT=adaptive` (opt-in, not yet evaluated, no saving claimed) replaces the size test with a
token cost model at the start of each read turn, `static` (the default) logs a `ctx ... (shadow)` line when it would
have decided differently, and `off` never starts an existing thread fresh. None of these adds a model call. See
[One conversation](SHELL.md#one-conversation-and-a-fresh-start-past-64k).

The record a fresh start sends names the files the turns read (one line of paths, no contents) and, for a conversation
that is being replaced, keeps older replies and steps in their whole form up to a byte cap of half the cost model's
saving (`4 x 3/8 x (f - B)` bytes), never past the record's 60,000 bytes. The cost model also resumes when the saving
does not cover twice the record. The effect on quality or cost is not measured.

Within a running turn, Codex now receives
`model_auto_compact_token_limit=64000`; Claude receives
`CLAUDE_CODE_AUTO_COMPACT_WINDOW=100000`, its minimum supported window. Previously
both defaults were 200,000. Claude subtracts output reserve and compaction
headroom from its window, so the two settings are not identical trigger points.
These native controls summarize history as the agent works, including history
being re-read from cache. They apply to new and resumed conversations. Explicit
Codex config or a Claude window environment variable takes precedence; native
settings disabling compaction are not overridden. agy's compaction remains under
its own control.

The threshold applies to context size, not the accumulated cache-read counter.
Compaction cannot erase tokens already consumed or cap total task usage. It also
has its own token and latency cost, and a large tool result can overshoot the
threshold before the next native compaction boundary. Ringo keeps provider usage
totals across compactions and clears stale context sizes when compaction completes
so a later handoff does not discard the native summary based on its old size.
These defaults reduce the context allowed to accumulate; savings and task quality
still require matched task measurements.

The inline **record body** has a hard 60,000 UTF-8 byte budget shared by turns,
file lists, diffs, cut notices, restart separators and saved-record pointers.
The current owner message, handoff envelope, standing instructions and native
agent overhead are outside that text budget. Bytes are not provider tokens.
Older material is omitted first; the full record is kept outside the project for
recovery. Inline changed-file lists show at most 20 names and the omitted count.
A record already supplied whole inline gets no redundant file pointer. A cut
record points at the full file with instructions to recover only needed details
(agy receives the pointer only in read mode).

These bounds add no inference calls. Offline regressions cover 200-turn
histories, large file lists and failure messages, Unicode diffs and 25 repeated
restart carries. They establish bounded prompt text and recovery availability,
not equivalent task quality or savings against standalone agents. Recovering
omitted evidence can still consume tokens; include those reads in comparisons.

## Optimization objective

The current user objective is for Ringo plus its agent to consume fewer total
tokens than the same agent working alone on matched tasks, through more efficient
execution while preserving or improving result quality. No monetary spending cap
has been set for this work. Do not invent a cap or convert tokens into dollar
estimates. If the user later supplies explicit limits, honor them.

Measure total tokens per successfully completed task, including the outer agent,
all Ringo and Council calls, retries, failed attempts and corrections. Keep failed
tasks in the evaluation denominator and report their tokens too. Ringo's internal
CLI counters alone do not measure the whole system: if outer-agent usage is
unavailable, report partial coverage and leave the full comparison unresolved.

Use the same model/version, tasks, tool access and independently fixed quality
criteria in the standalone and Ringo arms. Report task success, quality, elapsed
time and the token breakdown together. Include setup overhead or explicitly
declare how it is amortized. Reduced quality, omitted failures or work shifted
outside the measured process cannot establish an efficiency gain. A saving is
an empirical result to demonstrate, not a property already established here.

Protected evaluator/campaign dollar-based gates and existing accounting records
are unchanged by this reporting clarification. They are not an active monetary
limit for ordinary reviews. A future token-based evaluation gate requires its
own declared protocol; this objective does not start an unbounded campaign.

Example: the preceding runtime publication's two native reviews reported
269,849 uncached input + 17,920 cached input + 0 cache-write + 1,184 output =
**288,953 tokens**. That is reported usage, not a new benchmark score or an
estimate of the outer agent's entire session consumption.

## Optional admission limit and reporting

`review` and `design` accept `--max-tokens N`; `work` specs accept `"max_tokens": N`.
The nonnegative integer limits admission using recorded input, cache-read, cache-write
and output counters across all attempts. In-flight calls may exceed the limit; their
known overshoot is reported. Missing or invalid counters block subsequent calls under
a token limit. This is an admission limit, not a provider-side generation cap.

Private traces retain full failed output and bounded ledger excerpts. `task-report.json`
derives usage and coverage from that ledger, including paid attempts with unknown usage.
Python library callers can opt into feedback/material byte caps and coverage-tracked
review batches; those prompt-bounding options are not exposed by the native CLI yet.
Byte counts do not establish token savings or quality improvements.

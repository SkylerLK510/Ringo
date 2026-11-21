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

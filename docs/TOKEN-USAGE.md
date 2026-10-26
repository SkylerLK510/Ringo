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

The reported total is `input + cache_read + cache_write + output`. These are
Ringo's normalized fields: native Codex usage includes cached tokens within raw
input, so Ringo subtracts that subset from `input` and records it in `cache_read`.
Do not add raw input to normalized cached input and count the cache twice.

Include every attempt that reports usage, including retries, malformed replies
and failed calls. Report call count and elapsed time alongside tokens. If
`untokened_calls` is nonzero, label the total **partial** and state how many
attempts lack usage. Do not estimate missing usage from text length or treat it
as zero consumption. Tokens from different model tokenizers are model-specific;
record the backend/model when comparing efficiency.

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

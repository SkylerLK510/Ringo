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

Do not convert tokens into an inferred dollar price in ordinary reports.
Explicit spending limits remain enforced, and protected evaluator/campaign cost
gates are unchanged. This preference changes presentation and the default
reported efficiency measure, not those gates or stored accounting records.

Example: the preceding runtime publication's two native reviews reported
269,849 uncached input + 17,920 cached input + 0 cache-write + 1,184 output =
**288,953 tokens**. That is reported usage, not a new benchmark score or an
estimate of the outer agent's entire session consumption.

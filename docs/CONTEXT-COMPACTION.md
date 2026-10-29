# Context compaction: what we found, and why Ringo does not compact yet

Status, 2026-10-29: researched and measured offline, **not adopted, not tested.** Nothing here changes Ringo. It
records the evidence so a later decision starts from it.

## The question

Frontier coding agents shrink a long conversation ("compaction") so it fits and costs less. Could Ringo do something
better, under its rule that nothing may add total token volume per task, even if it lowers cost?

## What the vendors do

A deep-research pass checked 25 published claims across 22 sources: 19 were confirmed and 6 refuted.

- **Every vendor's in-place compaction is a model call over the whole context.**
  - Anthropic's server-side compaction runs a separate, billed sampling step. Its default trigger is 150,000 input
    tokens (minimum 50,000), and the docs' own example bills 180,000 input and 3,500 output tokens for one compaction
    ([docs](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold)).
  - Claude Code's automatic and `/compact` compaction is an LLM summary
    ([docs](https://code.claude.com/docs/en/context-window)).
  - OpenAI's Responses API compaction returns an encrypted item a client cannot read or audit
    ([guide](https://developers.openai.com/api/docs/guides/compaction)).
- **Two zero-token mechanisms exist.** Anthropic's context editing clears stale tool results and leaves placeholders
  ([announcement](https://www.anthropic.com/news/context-management)). After compacting, Claude Code re-reads up to
  five recent files by rule, and refers to files over 5,000 tokens by path.

## What controlled studies show

- **Masking ties the summarizer at half the cost.** JetBrains ran SWE-bench Verified (500 tasks) with SWE-agent and
  held the model constant ([arXiv 2508.21433](https://arxiv.org/html/2508.21433v2)).
  - Masking keeps the last 10 tool outputs and replaces older ones with placeholders, with no model call. It halved
    cost against the raw agent and matched the LLM summary's solve rate.
  - Summaries made Qwen3-Coder runs 13–15% longer. The authors think a summary hides the signs that a run is failing.
- **The large published gains come from training model weights.** These are context folding, SUPO and MEM1, which
  Ringo cannot use ([2510.11967](https://arxiv.org/pdf/2510.11967), [2510.06727](https://arxiv.org/abs/2510.06727),
  [2506.15841](https://arxiv.org/html/2506.15841v2)). Without training:
  - folding beat summarization by 0.4 points;
  - summarize-and-reset helped only at 3–24× the token budget.
- **ACON cut peak context by 26–54%.** Its figures leave out the compressor's own tokens, so it shows no total saving
  ([2510.00615](https://arxiv.org/abs/2510.00615v2)).
- **LongCodeZip compressed code context up to 5.6× with a small quality loss.** It needs a local language model's
  per-token scores ([2510.00446](https://arxiv.org/html/2510.00446v1)).

Caveats: most findings rest on one primary source each, and the vendor effect numbers are unaudited internal tests
outside coding. Nothing was found on Codex CLI's local compaction, Gemini CLI, Cursor, Manus or Factory internals, so
the research cannot show that any approach is unused by every vendor.

## What it means for Ringo

A summarizer call breaks the volume rule by itself, because its input is the whole context it summarizes. That rules
out every vendor approach above that calls a model.

Ringo's pipelines have no history to compact: each call is rebuilt from git objects. Two places remain.

### 1. Agent turns in the shell

A resumed turn re-sends the conversation: Codex grows from 16.8k to 34.8k tokens, Claude Code from 16.5k to 33.1k.

The alternative is to rebuild instead of resume, with no model call. Start a fresh turn whose context comes from the
repository (the diff, the touched files, the current failing check output), and carry only the task and the model's
last reply as text.

- Because the context holds real check output, not a summary, it cannot hide a failing run.
- Its accuracy can be checked by re-running the checks.
- It saves tokens only while the rebuilt context stays under about 18.0k tokens (Codex) or 16.6k (Claude Code).

Not measured. The open risk is that a fresh agent spends the saving re-reading files with its tools.

### 2. Review material: measured offline, no model calls

We measured 115 distinct review materials that Ringo built in recorded runs. The median material was 283 KB, about
70k tokens per review call.

| Part of the material | Share of all bytes |
|---|---|
| Changed files, sent whole | 55.9% |
| Unchanged context (definitions, callers, subclasses, tests) | 25.1% |
| The diff | 13.7% |
| README, manifest, PR text | 5.2% |

About 70% of the changed-file text is more than 15 lines from any edit. Ringo already sends unchanged files as windows
around their matches, but sends changed files whole. Windowing the changed files as well would shrink the material:

| Lines kept around each edit | Material shrinks by |
|---|---|
| ±15 lines | 39% |
| ±40 lines | 34% |

A parser-based form would keep each enclosing function or class whole and only the signatures of the rest. It costs
a few milliseconds per file in the Rust runtime, under 0.1% of a review's time.

The cost is a risk to recall, not latency: a defect can sit in the unchanged lines of a changed file. The first check
is free. Count how many of the benchmark's labeled defect lines would survive the trimming. Only if most survive is a
preregistered, paired study worth its model calls.

## Decision

Not now. No measured gain exists on Ringo's own tasks, and the owner's priority is the CLI.

Revisit if either:
- shell agent turns become a large, measured share of tokens; or
- the free recall check above shows the trimming keeps nearly all labeled defects.

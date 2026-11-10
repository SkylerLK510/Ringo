# Preregistration: what a review needs to be shown (pilot)

Registered 2026-11-10, before any model call of this pilot. The commit that adds this file fixes the arms, the
reviews it runs on, the measures and how they are read. Any change after it is a deviation, recorded at the end of
this file with its reason.

## Question

`review --repo` shows the model the diff, every changed file whole, the README, and windows around references to
the changed symbols, up to a 400,000-byte budget. Does the review keep its findings when it is shown only the
change with 20 lines of context?

## Development evidence (no model calls)

A stored review is a record of what a reviewer used: every finding gives a `file:line` and quotes the lines it rests
on. Those references were replayed against smaller materials, the way a paging policy is tested on a recorded
reference string. The scripts and the per-case results are kept with the private run records.

| Stored reviews | Reviews | Findings | Whole changed files | Reference windows | README | Diff |
|---|---|---|---|---|---|---|
| A public review benchmark's development split, 12 prompt variants of one reviewer on 30 pull requests | 360 | 1,488 graded as true | 57.7% of bytes | 27.4% | 3.0% | 8.9% |
| This repository's own changes | 62 | 112 | 61.8% | 17.0% | 8.0% | 11.2% |

- Benchmark: 244 of the 249 labelled true findings lie within 20 lines of a changed line. Of the 1,488 true
  findings the reviews made, 3 cite a reference window and none cites the README.
- Own changes: none of the 112 findings cites a reference window or the README. 108 cite only lines within 20
  lines of a changed line. The 62 reviews read 6,013,665 input tokens, 96,994 each on average.
- The change with 20 lines of context is 14.9% of the benchmark materials' bytes and 20.9% of the own-change
  materials'. It holds every cited line of 96.8% and 96.4% of the findings.
- A citation is not everything a finding needed. Counting also the definition of every identifier a finding names,
  the lean material holds all of it for 60.1% of the benchmark's true findings and 92.0% of the own-change
  findings. The truth lies between the two figures; this pilot is the first measurement of it.

## Design

- **Reviews.** Stored `review --repo` runs on this repository's commits, backend `codex-chatgpt`, with at least one
  finding, one per commit pair. Set S: every such review that read at most 70,000 input tokens (9 reviews, 15
  findings). Set L: the 10 most recent of the others (24 findings). The list of commit pairs has sha256
  `a9ef70f5a6c60303...`; the heads start `4c15b035`, `42bdcff8`, `e2a2947e`, `c3b0ab31`, `5e12b834`, `9489e6f4`,
  `828aca2b`, `ead86819`, `31f28800` (S) and `2d951eda`, `f6f3f5b4`, `5795109c`, `6ca5cbbe`, `7cec5a23`,
  `4fccf819`, `f259b0c0`, `af0c0dff`, `b05b06b4`, `eb3fb581` (L).
- **Arms.** A0: the stored review (already paid). A1, set S only: the stored material, byte for byte, reviewed again
  now. A2: the stored PR description, `git diff -M -U20 base...head`, and a three-line manifest that says what is
  shown, what is not, and that a claim resting on what is not shown is a question that names the file and symbol
  needed. Nothing else.
- **Pipeline.** The active release (`496afe0b...`) through its launcher: `review --input MATERIAL --backend
  codex-chatgpt --timeout 180`. The review plan and prompt are the release's own. A1 and A2 differ only in the
  material.
- **Order.** Set S first, A2 and A1 of a review side by side, two calls at a time; then set L (A2 only).
- **Budget and stopping.** At most 45 calls and 900,000 input tokens, counted from every trace, failed calls
  included. The run stops before a pair that would pass either limit, and when the plan's 5-hour window reaches 70%
  or its weekly window 75%. A failed call is not retried.

## Measures

- **Primary: retention.** An A0 finding is retained by a later review when that review has a finding about the
  same defect. One blind matcher call per review decides: the benchmark's matcher prompt (sha256
  `9f0bf7c80fef7b55...`) through `codex-chatgpt`, with the A0 findings as the reference set and the A1 and A2
  findings pooled in a seeded random order, arms hidden. A rule without a model is reported beside it: same file,
  line ranges within 10 lines.
- **Secondary.** Input tokens per review. Findings per review. Findings no A0 finding matches, each one checked
  against the code by hand and listed with its verdict. Open questions per review, and how many name a file or
  symbol that exists at head. Retention among the A0 findings whose lines a later commit of the same branch changed.

## Analysis

Retention is reported as a proportion with a Wilson 95% interval: A1 on S, A2 on S, A2 on L. On S, A1 and A2 are
compared per A0 finding with an exact McNemar test. With 15 findings on S the pilot cannot show equivalence; it
estimates the size of any loss and what the lost findings needed.

How the result is read:
- A2 retains within 20 points of A1 on S, and on L not below A1's retention on S minus 20 points: the lean material
  goes to a confirmatory study (larger, sealed, with a same-material control on every review).
- Otherwise: the lean material is not pursued as is. Each lost finding is classified by what it needed (a
  definition in the same file, a caller, another file, the README), and that classification is the result.

Nothing is adopted from this pilot. `review --repo` stays as it is until a confirmatory study and the owner's review.

## Deviations

1. **Budget counter, during set L.** The counter added the matcher calls twice and stopped the run after the sixth
   L review at a true 36 calls. It was corrected and the run went on in the same order. No call was repeated.

2. **Paging probe added, 2026-11-10, after sets S and L were scored and before any further call.** Calls so far: 43,
   with 712,533 input tokens.

   Result so far, by the blind matcher: on S, A1 retained 13 of 15 stored findings and A2 retained 13 of 15, with
   one finding kept by each arm alone. On L, A2 retained 5 of 24. By the reading rule above the lean material is not
   pursued as is, and the lost findings are classified: for 19 of the 21 findings A2 lost, every line the stored
   finding cites was in the lean material. The cited code was there; what the reviewer lacked was the code around
   it. The lean reviews say so themselves: they left 35 open questions, and a rule without a model finds the
   definition a question names, at head, for 26 of them.

   The probe, exploratory: for every L review that lost a stored finding and has an open question that rule
   answers (9 reviews), one follow-up call through the same pipeline. Its material is the PR description, the lean
   review's open questions, the diff hunks that name the paged symbols (at most 24,000 bytes) and the paged
   definitions with line numbers (at most 24,000 bytes). Then one matcher call per review, the stored findings as
   the reference set. Measures: stored findings retained by the lean review or its follow-up, and the input tokens
   of the two calls against A0's. There is no same-material control on L, so the probe can show that paging
   recovers findings, not how the pair compares with a second whole-file review.

   The call ceiling rises from 45 to 63 for the probe. The 900,000 input-token ceiling and the plan-window stops
   are unchanged.

3. **One matcher result corrected.** The first matcher pass ran while set L was still running and read one L
   review's output file before it was written, so it recorded "no shared file" for that review without making a
   call. The file was complete and shared a file with the stored review. The matcher was run for it once. Set L's
   retention is 6 of 24 with that call (5 of 20 without the review).

4. **Slice arm added, 2026-11-10, after the paging probe and before any further call.** Calls so far: 56, with
   777,888 input tokens. Probe result: 9 follow-up calls (59,504 input tokens) recovered 1 more stored finding, so
   the lean review with its follow-up kept 7 of 24 at 19.5% of A0's tokens. The follow-ups left 26 new open
   questions: each paged definition led to the next thing not shown.

   The arm, exploratory: A3 is the lean material plus numbered lines from the changed files: the definition of
   every name the changed lines use, and the lines around other uses of the names the change defines. These are
   the two rules of the trace study, fixed before the pilot. It runs on the L reviews where A2 kept no stored
   finding, smallest A3 material first, as many as fit under the 900,000-token ceiling with their matcher calls
   (two reviews, three stored findings). The question is only whether showing one hop of definitions and uses up
   front recovers what paging them in afterwards did not.

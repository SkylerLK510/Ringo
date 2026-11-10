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

None yet.

# Preregistration: reviewing a change in a very large file (control rerun, tool reading)

Registered 2026-11-10, before any model call of this experiment. It follows the pilot in
`2026-11-10-review-material-pilot.md`. The commit that adds this file fixes the reviews, the arms, the measures and
how they are read. Any change after it is a deviation, recorded at the end of this file with its reason.

## Why

The pilot showed the reviewer only the change with 20 lines of context. On changes inside two very large files it
kept 6 of 24 stored findings, at 15% of the tokens. Two things were left open:

- The pilot had no same-material control on those reviews, so it could not say how many stored findings a second
  whole-file review would itself reproduce.
- For 18 of the 20 lost findings every cited line was shown, and one round of paging in what the reviewer asked
  about recovered one finding. The pilot did not test a reviewer that reads the repository itself.

## Reviews

From the pilot's set L (stored `review --repo` runs on this repository's commits, backend `codex-chatgpt`): the
reviews with at least two stored findings, smallest stored input first, while the control's stored input stays under
700,000 tokens. That rule gives five reviews with 13 stored findings and 678,001 stored input tokens. Their heads
start `af0c0dff`, `4fccf819`, `7cec5a23`, `b05b06b4`, `eb3fb581`.

## Arms

- **A0.** The stored review (already paid): the diff, every changed file whole, the README and reference windows.
- **A1, control.** The stored material, byte for byte, reviewed again now by the active release (`496afe0b...`):
  `review --input MATERIAL --backend codex-chatgpt --timeout 180`.
- **A2, tool reading.** The same release, review plan and review prompt. The material is the stored PR description,
  `git diff -M -U20 base...head`, and a manifest that says what is shown and that the whole repository at the reviewed
  commit is in a named folder, to be read with the shell before a claim rests on code not shown, and that nothing from
  the repository is to be run. The backend is one Codex turn that keeps its agent prompt and its shell in the
  read-only sandbox. Everything else matches the `codex-chatgpt` preset: the ChatGPT sign-in is forced, the owner's
  configuration, rules, skills, plugins and project instructions are off, web search is off, nothing is stored, and
  the model and reasoning effort are the CLI's defaults. The folder is a plain export of the commit (`git archive`).

A1 and A2 differ in the material, in Codex's agent prompt and in the shell tool. The tool backend is first checked
with one call that carries no review material.

## Order, budget, stopping

One review at a time, A1 then A2, in the order above. At most 20 calls through the launcher (5 control, 5 tool
reading, 5 matcher, the rest for the release's own one-retry rule) and 1,200,000 uncached input tokens, counted from
every trace and from the tool backend's own record of each request. The run stops before a call that would pass
either limit, and when the plan's 5-hour window reaches 70% or its weekly window 75%. The plan's two windows are read
before and after every call (no model call).

## Measures

- **Primary: retention.** As in the pilot: an A0 finding is retained by a later review when that review has a
  finding about the same defect. One blind matcher call per review (the benchmark's matcher prompt, sha256
  `9f0bf7c80fef7b55...`, through `codex-chatgpt`), the A0 findings as the reference set, the A1 and A2 findings
  pooled in a seeded random order with the arms hidden. The rule without a model is reported beside it: same file,
  line ranges within 10 lines.
- **Cost.** Per review and arm: uncached input, cached input and output tokens; their sum (volume); the weighted
  figure uncached + 0.05 x cached + 5 x output (the published credit card's ratios); model requests and shell reads in
  the tool arm; seconds.
- **Secondary.** Findings and open questions per review. Findings that match no A0 finding, counted and kept with
  the private run records. They are not verified in this experiment.

## Analysis

- A1's retention with a Wilson 95% interval. This is the rerun baseline on large files.
- A1 against A2 per stored finding, exact McNemar test. With 13 findings this cannot show equivalence.
- The pilot's lean arm on the same five reviews against A1, per stored finding, exact McNemar test. Exploratory: the
  lean reviews were run a day earlier.
- How the plan's windows moved per call, against each call's uncached, cached and output tokens. Exploratory: the
  windows report whole percents.

How the result is read:

- A1 retains fewer than half of the stored findings: retention of stored findings is a weak measure on large files,
  and the pilot's 6 of 24 says less than it seemed to. A larger study then needs repeated controls or labelled
  defects.
- A2 retains at least A1's share minus 20 points, and its weighted cost is below A1's: the tool-reading reviewer goes
  to a confirmatory study (larger, sealed, a same-material control on every review). If its volume is above A1's,
  that is reported as a conflict with this repository's rule that an adopted change must not add token volume, and
  the owner decides.
- Otherwise the tool-reading reviewer is not pursued as it is, and each stored finding it lost is listed with what
  the reviewer read.

Nothing is adopted from this experiment. `review --repo` stays as it is until a confirmatory study and the owner's
review.

## Deviations

None yet.

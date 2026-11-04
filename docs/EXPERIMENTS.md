# Experiments: what we tried, what broke, what we measured

Every number here comes from our own runs. The task contents, hidden tests and raw traces stay private so the tasks
can be reused. This page records the designs, the results and every mistake we found.

**The current adoption rule:** a change is adopted only if it uses no more tokens than what it replaces. Cheaper
credits do not justify more tokens.

## How we measure

- **Tasks:** each is a brief with hidden acceptance tests. Every hidden test traces to a sentence in the brief.
  Mutants (plausible wrong answers that still pass the public checks) show that the hidden tests catch real
  mistakes.
- **Held-out discipline:** held-out tasks are frozen by hash before any model call, and development uses separate
  tasks.
- **Arms:**
  - the Codex CLI agent alone at high effort (AH) and at low effort (AL);
  - Claude Code alone (CA);
  - Ringo configurations.

  Every arm gets the same brief, public checks and instructions.
- **Quality:** the share of hidden tests that the delivered files pass.
- **Cost:**
  - Tokens are all input (uncached and cached) plus all output, for every provider.
  - Codex credits use the published rates for GPT-6.1 Sol (50 per million uncached input tokens, 2.5 cached, 250
    output) and GPT-6 Luna (2.5, 0.25, 12.5).
- **Decision rule, written before the runs:** a harness wins outright only if its paired quality interval lies
  above zero and it costs less.
- **Statistics:** two runs per arm per task; a 95% paired bootstrap with 10,000 resamples.
- **Audit:** an agent run that touched the hidden material would be excluded. None did.

## 1. The Design Council (2026-10-27)

The multi-judge Council cost about 7× the tokens of checks alone, with no significant quality gain. One review and
one revision delivered every candidate passing all acceptance tests, at a third of the Council's tokens. The Council
became opt-in (#15).

## 2. Where the tokens go: one code task with 26 hidden tests, one run per arm, all 26/26

| Arm | Tokens | Credits |
|---|---|---|
| Codex agent alone, high effort | 190,695 | 5.11 |
| One bare model call, high effort | 9,305 | 1.93 |
| Ringo, writer plus 6 Council judges | 75,229 | 7.04 |
| Ringo, check-first Council | 71,941 | 6.61 |
| Ringo, one review, high effort | 54,275 | 6.61 |
| Ringo, one review, low effort | 21,754 | 2.05 |

What we learned:
- **Output tokens are the cost.** They cost 5× uncached input and 100× cached input.
- **The agent's tokens were mostly re-reads.** 84% were cached re-reads of its own session.
- **High effort on every call bought nothing.**
- **No caching across calls.** A fresh `codex exec` call gets no cache hits from an earlier call.

## 3. Current Ringo against the agents (held-out tasks, phase 1)

Six tasks were built for this study:
- four code tasks: semantic-version ranges, a ledger, cron schedules, patch application;
- two analysis memos: a vendor decision, an incident postmortem.

Each has 27–40 hidden tests, and each arm ran twice.

| Arm | Hidden tests passed | Credits per task | Tokens per task | Seconds |
|---|---|---|---|---|
| Codex agent, high (AH) | 100% | 2.94 | 153k | 115 |
| Codex agent, low (AL) | 100% | 1.52 | 89k | 46 |
| Claude Code (CA) | 100% | (Claude: $0.32 API-equivalent) | 150k | 66 |
| Ringo, every call high (RH) | 100% | 2.38 | 26k | 129 |
| Ringo, every call low (RL) | 100% | 1.49 | 20k | 45 |

- **No quality win was possible.** Every arm passed every hidden test.
- **RH:** used 1.57× AL's credits, so it lost to AL. High effort everywhere was the bloat.
- **RL:** used 0.98× AL's credits and 0.22× its tokens.

## 4. Building v2: trials and errors

We tuned only on development tasks: a 26-test job-queue task, an ISO 8601 duration library (40 hidden tests) and an
inventory reconciliation memo (35).

| We tried | What happened | What we changed |
|---|---|---|
| The text protocol as the default | 28 existing tests built on JSON broke | Made it opt-in (`protocol: "text"`) |
| The writer's `@@@ FILE` blocks | It closed them with `@@@ END`, so every round was malformed | `END` is accepted after any block |
| GPT-6 Luna writing spec tests | It quoted the brief in four-quote docstrings, which do not compile, so the suite was lost | Brief sentences go in `# brief:` comments; four-quote docstrings are repaired |
| Luna's edits | It wrote a bare `@@@` where `@@@ WITH` belonged | A bare marker is read by position |
| Luna's edit anchors | An anchor matched twice, and the round was lost | `protocol_repair`: one immediate call that asks for whole files |
| Three Luna drafts (job queue) | 0.24–0.31 credits against 5.11 for the agent, but 25/26 in four of four runs. Every run returned `''` where the brief said `empty`, and Luna's tests shared that blind spot | Luna is not a writer |
| Sol writer, Luna for everything else (job queue) | 26/26 at 0.81 credits. Half of Luna's tests were wrong, and an unchanged reply ended the run before the review | An unchanged reply stands by the code and goes on to the review |
| Two Sol drafts (duration library) | Both drafts passed 40/40, but revisions driven by wrong Luna tests delivered 36/40 twice | Dual agreement (a test every draft fails stops gating) and the no-regression gate, after which it passed 40/40 |
| Luna's rationale-only replies | Counted as malformed | They stand by the code |
| Three Luna drafts (memo) | Luna sent only scripts, the run ended (a bug), and Sol escalation then wrote the memo at 3.0 credits | Fixed the bug; dropped Luna as a writer |

## 5. v2 on the held-out tasks (phase 2)

| Arm | Hidden tests passed | Credits per task | Tokens per task |
|---|---|---|---|
| V2S: Sol writer; Luna tests, revision and review | 99.4% (one memo run lost 2 of 28) | 1.13 | 58k |
| V2K: V2S with two Sol drafts | 100% | 1.93 | 61k |

- **V2S** had the lowest credits, but nearly 3× RL's tokens and twice its time.
- **V2K** used more credits than AL.

Current Ringo was outperforming v2, and the data showed why:
- **The first draft was already right.** In all 36 runs we checked (RH, RL, V2S), Ringo's first draft passed every
  hidden test, so everything after round 1 was overhead.
- **The reviews' findings were all false.** Luna reviews raised findings on 12 of 12 V2S runs, all false. Sol
  reviews raised findings on 3 of 12 RL runs, also false but harmless.
- **The one loss:** the Sol writer had the right answer (40 customers, SEV2) for three rounds. Then a Luna review
  claimed the count was wrong, and a Luna revision changed it to 56 and SEV1 with no successful script behind it.

## 6. V3: removing the kinks

**Fixes:**
- **The number-evidence gate:** with `scripts`, a document revision may not introduce a number that no successful
  script printed. Replayed on the real loss, the revision's only unbacked number was the wrong 56, so the revision
  would have been set aside.
- **Review material:** a rewritten file gets no diff. A stub-to-implementation diff repeated the whole file, which
  was 2–3k of a code review's 8k input tokens.
- **Scripts:** a script whose output already matches the document no longer forces a round. Script logs no longer
  overwrite the round's check logs.
- **Prompts:** when feedback is wrong, change nothing and say why; resend the script when changing a number.

**Three Ringo code reviews of these changes found seven more bugs, all fixed with tests:**
- a spec-runner timeout crashed the run;
- unittest class fixtures were skipped;
- module fixtures were skipped;
- teardown errors were dropped;
- fixture errors were matched to the wrong module;
- the blind test writer was shown fixtures;
- the number gate ignored sign changes.

**Dev check (one run each):**

| Task | Current Ringo (RL) | V3 without scripts | V3 with scripts |
|---|---|---|---|
| Memo, 35 hidden tests | 34/35, 2.14 credits, 28k tokens | 34/35, 2.08 credits, 25k tokens | 35/35, 2.55 credits, 29k tokens |
| Duration library, 40 hidden tests | 40/40, 1.47 credits, 18k tokens | 40/40, 0.89 credits, 10k tokens | n/a (code) |

**Phase 3** used fresh repetitions 3 and 4 of the six held-out tasks. V3 was designed after seeing phases 1 and 2,
so this is a confirmation, not a clean held-out test.

| Arm | Hidden tests passed | Credits per task | Tokens per task | Seconds |
|---|---|---|---|---|
| V3 with memo scripts (V3s) | 100% | 1.44 | 17.7k | 46 |
| Current Ringo (RL) | 100% | 1.52 | 20.3k | 46 |
| Codex agent, low (AL) | 100% | 1.42 | 88.2k | 45 |

- **Against RL:** at equal quality, V3s used 5% fewer credits and 13% fewer tokens.
- **Against AL:** about equal credits (1.01×) and 80% fewer tokens.
- **By task:** on code, V3s used fewer tokens than RL on three of four tasks (14k against 25k, 11k against 14k, 14k
  against 21k) and the same on the ledger task (15k each), where RL used slightly fewer credits (1.18 against 1.20).
  On the memos the scripts added tokens: 23k against RL's 17k on the vendor memo. Under the adoption rule, the
  recommended V3 therefore leaves `scripts` out.

## 7. HumanEval+: the review earns its place on code (2026-10-28)

The first study with hidden-test headroom: 163 HumanEval+ tasks (EvalPlus v0.1.10, Apache-2.0; HumanEval/32 left
out because its own reference fails the benchmark's root check). The original HumanEval tests are the public check;
EvalPlus's extra inputs, about 80 times as many, are the hidden tests. Every task ran once on V3 (Codex, Sol low).
Details and the method are in [AGCL](AGCL.md); task files and traces stay private.

**Against a standalone agent.** The same tasks also ran through the Codex CLI as a plain agent (Sol low, a fresh
workspace per task), in a fixed random order until an 80-credit ceiling stopped it after 154 tasks. On those 154:

| Arm | Hidden tests passed | Tokens per task | Credits per task |
|---|---|---|---|
| Codex agent | 148/154 | 50,325 | 0.513 |
| Ringo V3 | 147/154 | 3,500 | 0.235 |

Quality is a tie: the two disagree on 3 tasks (the agent alone passes 2, Ringo alone 1; exact McNemar p = 1.0), and 5
fail for both. Ringo uses 14.4 times fewer tokens and 2.2 times fewer credits. 87% of the agent's tokens are cached
input, its own context re-sent on each of about five steps. So Ringo does not beat the agent outright here: it
matches it at a fraction of the cost, and a quality win needs harder tasks.

| Idea | Measured | Decision |
|---|---|---|
| Stop after round 1 when the checks pass | Saves 53% of tokens. The review then caught one defect the public tests missed (HumanEval/137), so stopping harms 1 of 162 eligible runs: upper bound 2.9%, over the 2% bar set beforehand | Rejected; `stop_on_checks` removed |
| Keep the review on code | 24 planted defects that pass the public tests: the review flagged 24/24 and quoted the line on 23, with findings on 3/24 of the same drafts before seeding | Kept |
| A reviewer from another family (Claude Sonnet 5.5) | Same catch rate (24/24), no false alarms (0/24), but 51% more tokens per review (2,963 against 1,957) | Not adopted: adds tokens |
| Distinguishing inputs without a model: 200 inputs mutated from the public tests | Told 20 of 23 planted defects apart from the correct draft. Between Claude's and Codex's real drafts it flagged 1 of the 8 tasks Codex fails and 24 of the 150 it passes; it missed the one revision that mattered | Not adopted: misses the corners hidden tests reach |
| A second draft from another family | Either draft passes on 154 of 158 tasks, against 150 for Codex alone: real headroom, but no zero-token signal picks the right one | Open; nothing built on it |

**Mistakes found in the study itself:** HumanEval/38 and /50 test their function through a helper the prompt
defines, and the generated public check imported only the function, so it could never pass. Both were rerun with the
fix. 14 of 163 Claude drafts were replies Ringo could not apply; a `work` round answers that with one repair call, so
the drafts were redone with it.

## 8. MBPP+: four refinements from HumanEval+ failures, rejected (2026-10-29)

Four changes came from reading two of V3's HumanEval+ failures. HumanEval/47 hardcoded its docstring's wrong
example, and /34 sorted before deduplicating. Each change was written to add no tokens:

- a writer rule: follow the behaviour the brief names over its examples, and never special-case an input;
- a static note on code fitted to one literal example;
- a stop when a revision leaves code that fails a check unchanged, handing the dispute to a person;
- review wording that names edge values.

HumanEval+ cannot test changes built from its own failures. So the changes ran on 150 MBPP+ tasks (v0.2.0, a
seeded random sample; the other 228 stay sealed), paired with V3 on the same model, under a plan registered before
the first call ([preregistration](prereg/2026-10-29-v3-refine-mbpp.md)).

| Arm | Hidden tests passed | Tokens per task | Credits per task |
|---|---|---|---|
| V3 | 132/150 | 2,631 | 0.171 |
| V3 + the four changes | 126/150 | 2,666 | 0.175 |

Six tasks went from pass to fail and none the other way (exact McNemar p = 0.031), so the changes made Ringo worse.
They also used 35 more tokens per task (95% interval -43 to +118). The official EvalPlus evaluator agrees with the study's grader on all 300 graded files.
The study cost 51.9 credits.

**Why.** The study tests the four changes together, so these are per-task readings, not separate effects. MBPP's
prose is short, and its examples define the behaviour.
- The writer rule told the writer to follow the named behaviour over the examples. On 4 tasks it overrode correct
  examples.
- The stop rule then ended 5 runs as disputes. On 4 of those tasks, V3 delivered a correct answer by fixing its code
  in round 2.
- The edge-value wording led the review to flag float precision on huge integers (Mbpp/14). The revision broke the
  expected result. Overall, false alarms nearly doubled: 13 of 127 passing drafts against 7 of 131.
- The static note never fired.

All four were removed. A rule fitted to one benchmark's quirk, here a wrong docstring example, does harm where the
examples are the specification. On these tasks, deferring to the check worked better than trusting the prose.

## 9. Portability

- **`ringo.py wait`:** its liveness check used `os.kill(pid, 0)`, which terminates the process on Windows. On Windows
  it now opens the process and waits on it with a zero timeout instead.
- **Spec runner:** it used `SIGALRM`, which Windows lacks. It now runs without it there; the suite's timeout still
  bounds it.
- **Release bundles:** these left out `spec_runner.py`. It is now in the bundle.
- **CI:** it runs on Ubuntu, macOS and Windows, and the whole offline suite passes natively on Windows. Tests of
  POSIX-only properties (permission bits, `os.killpg`, FIFOs, the Linux ReviewBench image) are skipped there.
- **Processes on Windows:** the native port locks with `msvcrt`, ends a call's or check's process tree through a job
  object, and splits backend commands without treating `\` in Windows paths as an escape. Explicit `.cmd`/`.bat`
  launches are refused, including trusted backends and checks; call the underlying executable to avoid `cmd.exe`
  argument interpretation. POSIX permissions and process launching are unchanged.
- **Private folders on Windows** (traces, stores, release state, temporary prompts and private Codex homes) use a
  protected, verified DACL with one inheritable full-control ACE for the current user:
  - New roots receive it atomically at creation, and it is verified before sensitive files are written. Child files
    and folders inherit it.
  - Pre-existing roots are tightened and verified, but earlier open handles are not revoked.
  - Failure to set or verify the DACL stops writing, so a filesystem that cannot do it is refused.
  - Ringo checks the private roots, not every folder inside them. A file or folder already inside a root with an
    explicit ACL of its own keeps it, and so does what is written into it.
  - Administrators taking ownership, and other processes running under your account, are outside this guarantee.

## 10. Ideas we considered and did not adopt

A deep-research pass checked 25 published claims: 19 were confirmed and 6 refuted. Its verdicts are included below.

| Idea | Evidence | Decision |
|---|---|---|
| High effort on every call | RH: 2.38 credits against RL's 1.49, same quality | Rejected: bloat |
| The Council by default | About 7× tokens, no quality gain | Opt-in |
| Luna as the writer | Cheap, but missed rules and could not write a memo | Rejected |
| Luna reviews and revisions | False findings on 12 of 12 runs; caused the one loss | Rejected: weak models may advise, never change the result |
| Spec tests by default | Never improved a result; wrong tests drove regressions | Opt-in, with dual agreement |
| Two Sol drafts (V2K) | 1.27× AL's credits, with no quality to buy | Opt-in (`candidates`) |
| Shared-primer session forking: a seed session holds the brief, and later calls fork from it to read it from cache | `codex exec fork --ephemeral` reuses the parent's cache routing (Codex CLI 0.164.0); cached input costs 0.05× (estimated 27–34% fewer credits, not measured). A separate seed adds about 10k tokens per memo run. Using the writer as the seed adds the writer's transcript to the reviewer's context | Not adopted: it adds tokens |
| A cheap scout plus a router | The published saving came from a cheap fixer, not from the router | Rejected; Luna as writer already failed here |
| Learned per-step routers | No measurable end-to-end gain; needs a fine-tuned model | Rejected |
| A strong advisor beside a weak executor | Published gains are confounded and inside noise | Deferred |
| Single-pass claim verification | Worked only with a fine-tuned small model | At most a pilot |
| Batching similar checks into one call | About 4× fewer reasoning tokens, in one preprint | At most a pilot |
| Stop after round 1 when the checks pass (`stop_on_checks`) | 1 harm in 162 HumanEval+ runs; bound 2.9% over the 2% bar (section 7) | Removed |
| A reviewer from another model family | Same catches, fewer false alarms, 51% more tokens per review (section 7) | Not adopted |
| Distinguishing inputs from mutated public inputs | Flagged 1 of 8 real hidden failures, with 24 false flags in 150 (section 7) | Not adopted |
| Letting a model rewrite an ambiguous brief directly | Below the original brief in 11 of 12 model-benchmark cells (SpecFix, arXiv 2505.07270) | Not tried: published as harmful |
| Regenerating when the model seems uncertain | Lower pass@1 in 5 of 6 configurations (arXiv 2608.14659; small models) | Not tried: published as harmful |
| Fixed multi-round repair | Repair with a noisy verifier turned correct answers wrong (arXiv 2607.17641); at a matched budget, more first drafts beat more repairs (Olausson et al., ICLR 2024) | Not tried; `max_rounds` stays bounded by the checks |
| Context compaction (shrinking history or material) | Vendors compact with a model call over the whole context, which adds tokens; deterministic masking matched LLM summaries at half the cost on SWE-bench Verified (arXiv 2508.21433). Ringo's pipelines keep no history. Offline, windowing changed files to ±15 lines would cut review material 39%, with untested recall ([details](CONTEXT-COMPACTION.md)) | Not adopted now; not tested |
| Follow the named behaviour over the examples (writer rule) | 4 round-1 losses and no gains on MBPP+ (section 8) | Removed |
| Stop when unchanged code fails a check, and hand the dispute to a person | Ended 5 MBPP+ runs; V3 delivered 4 of those tasks correctly (section 8) | Removed |
| A static note on code fitted to one literal example | Fired on 1 of 645 earlier drafts and on none of 300 MBPP+ runs | Removed: inert |
| Edge values named in the review prompt | False alarms 13/127 against 7/131; one revision broke a passing draft (section 8) | Removed |

## 11. What would move it next

- **Tasks with headroom, measured against the agent.** On HumanEval+ the two tie on quality and Ringo costs far less
  (section 7). V3 passes 132 of 150 MBPP+ tasks at 2,631 tokens each (section 8). The agent on those same tasks, and
  harder sets such as LiveCodeBench, come next.
- **Refinements need unseen tasks.** The four changes built from HumanEval+ failures, tested together, did harm on
  MBPP+ (section 8). A candidate is now built on one set and judged on a sealed one.
- **Choosing between two drafts.** Two drafts from different families pass more hidden tests than either alone, but
  the cheap signals tried so far cannot tell which one is right. A published e-process release rule
  (arXiv 2605.12947) and execution-based selection (DiffCodeGen, arXiv 2605.20473) are the starting points; both
  spend far more samples than Ringo, so the question is a gate that pays only when drafts disagree.
- **The IDE.** Two of the largest gains need a person in the loop: asking at the fork (a one-click choice where the
  spec has two readings) and remembering the user's conventions. Both belong in the IDE (see the [Roadmap](../README.md#roadmap)).


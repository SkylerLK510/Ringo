# Roadmap

[Project home](README.md) · [Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

Ringo has a working shell and pipeline runtime, and plenty left to prove. This page
groups the next work by the outcome it should deliver. It is a direction, not a release
schedule; new evidence can change the order.

## Make the everyday experience dependable

Colleagues should be able to install Ringo, understand what an agent is doing, and recover
their work without learning the runtime internals.

| Open work | What would count as progress |
| --- | --- |
| First-run setup and onboarding | A new user can install, sign in and complete a small task using the published guide |
| Input, selection, copying and terminal rendering | Manual checks cover narrow/wide terminals, multiline input, Unicode and clipboard behavior |
| Restart, resume and handoffs | Conversations retain their context and readable formatting across quit, restart and upgrade |
| Live usage and job visibility | Provider-reported usage remains traceable through running turns, subagents and compaction; missing counts stay visible |
| Windows and WSL | Real agent calls are exercised and documented, in addition to offline CI |

Recent fixes establish pieces of this behavior. Broader terminal and platform coverage
still needs real use. Useful bug reports include a minimal reproduction and a redacted
screenshot when the issue is visual.

## Measure more than small coding tasks

Existing studies found token savings on specific workloads. Some reached a quality ceiling;
some refinements failed on a different benchmark. The next studies need room to detect
both gains and regressions.

- Cover repository-level coding, design judgment and architectural decisions with independently
  checked outcomes for each.
- Compare the complete task against the same standalone agent: include retries, reviews,
  cached input and failed attempts in the accounting.
- Keep development, validation and holdout families separate; publish methods and aggregate
  outcomes while protecting task contents and hidden checks.
- Validate research workloads for evidence quality, reproducibility and numerical correctness.
  The current source-verification pipeline has no measured quality advantage.

The [experiments](docs/EXPERIMENTS.md), [benchmark gaps](docs/BENCHMARK-GAPS.md) and
[research requirements](docs/RESEARCH-REQUIREMENTS.md) provide the background.

## Test recursive improvement honestly

The constrained controller and live adapter are implemented. That establishes machinery
for an experiment, not a successful recursive improvement result.

The next live stage must:

1. Freeze fresh scenarios and disjoint development, validation and holdout splits.
2. Certify grader discrimination and the exact process profile used by the run.
3. Obtain explicit numeric authorization for each live phase and run outside a Ringo child session.
4. Demonstrate policy sensitivity with controls before searching for improvements.
5. Compare candidates with the incumbent and manual arm, then require validation and sealed confirmation.
6. Retain failures and complete usage accounting, and require a human decision before adoption.

An inconclusive control stops advancement. A token reduction without the required quality
evidence does not establish success. The [live-pilot reference](docs/LIVE-RRSI-PILOT.md)
defines the execution gates. This roadmap authorizes no inference budget.

## Keep the engine reusable

- Maintain the Python runtime as a reference and test native Rust behavior against it.
- Keep model execution and accounting in Ringo; keep optional Council scoring in its own project.
- Develop the [Ringo IDE](https://github.com/SkylerLK510/Ringo-IDE) as a separate client.
  Its proposed context support, local model and background auditing remain future work.

## Help shape the next step

Start with an [issue](https://github.com/SkylerLK510/Ringo/issues/new/choose) describing
the task you were trying to finish, the friction you hit, and how you would tell a fix worked.
For a larger change, agree on scope and acceptance evidence before building it.

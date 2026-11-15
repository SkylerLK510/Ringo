# Documentation

[Project home](../README.md) · [Roadmap](../ROADMAP.md) · [Contributing](../CONTRIBUTING.md)

Start with a task you want to do. The references and research notes can wait until you need them.

## Start using Ringo

| Document | What you will find |
| --- | --- |
| [Getting started](GETTING-STARTED.md) | Install the shell or Python CLI, sign in, and run an example |
| [User guide](USER-GUIDE.md) | Everyday terminal use: agents, permissions, input, images, usage and undo |
| [Shell reference](SHELL.md) | Commands, configuration, platform behavior and session handling |
| [Pipeline reference](CLI.md) | Review, design, work, research, backends, time limits and accounting |
| [Research](RESEARCH.md) | Source gathering, verification and report rules |
| [Token usage](TOKEN-USAGE.md) | Which tokens are counted and how unknown usage is represented |

## Understand the runtime

| Document | What you will find |
| --- | --- |
| [Native runtime](NATIVE-RUNTIME.md) | Rust/Python parity, measurements and release gates |
| [Harness reference](HARNESS.md) | Detailed command behavior, plans, evaluation and improvement specs |
| [Work specs and Council integration](COUNCIL-WORKFLOW.md) | Declared files, checks, judging and trust boundaries |
| [Context compaction](CONTEXT-COMPACTION.md) | Context handling and its implementation limits |
| [Releases](RELEASES.md) | Bundles, activation, rollback and latency checks |
| [Latency measurement](LATENCY.md) | Offline timing methodology |
| [Optimization regression cases](OPTIMIZE-CASES.md) | Release integrity and timing regression coverage |

## Read the evidence

| Document | What you will find |
| --- | --- |
| [Experiments](EXPERIMENTS.md) | Study methods, outcomes, failed ideas and limitations |
| [Improvement log](IMPROVEMENT-LOG.md) | Generated results backed by committed, pinned evidence |
| [HarBenchmark](../HarBenchmark.md) | Public development cases and an offline scorer |
| [Benchmark gaps](BENCHMARK-GAPS.md) | Where present evaluation falls short |
| [Overfitting guardrails](OVERFITTING-GUARDRAILS.md) | Held-out data rules and remaining limits |

The experiment summaries describe the configurations that were measured. They do not
establish that every current shell feature or backend has the same performance.

## Evaluation and improvement

RRSI means *Regularized Recursive Self-Improvement*. Here it is an experimental program
for improving harness policy while keeping provider model weights fixed. Live improvement
has yet to be demonstrated; offline checks and transport proofs answer narrower questions.

Read these in order:

1. [Engineering objectives](ENGINEERING-OBJECTIVES.md): what a useful improvement should achieve.
2. [Turn runner](TURN-RUNNER.md): offline shell-turn measurement and scenarios.
3. [Recursive campaigns](RRSI-CAMPAIGN.md): policy scope, splits, controls and advancement gates.
4. [Native broker](NATIVE-RRSI-BROKER.md): isolated provider execution and process certification.
5. [Live pilot](LIVE-RRSI-PILOT.md): preparation, phase authorization and standalone execution.

These are references, not authorization to launch a campaign. Live runs require fresh
inputs and explicit numeric budgets; preparation and offline replay supply no live quality claim.

## Research directions and earlier investigations

These documents explain proposals or narrower studies. Their presence is not a claim that
the full workflow has shipped:

- [Audit-Gated Continual Learning](AGCL.md) and the [preregistered MBPP study](prereg/2026-10-29-v3-refine-mbpp.md).
- [Research requirements](RESEARCH-REQUIREMENTS.md) and [research role](RESEARCH-ROLE.md).
- [Latency optimization](LATENCY-OPTIMIZATION.md), [results](LATENCY-RESULTS.md) and [verifier optimization](VERIFIER-OPTIMIZATION.md).
- [Security and CI bot research](SECURITY-CI-BOT-RESEARCH.md).

For a source-code map and development checks, use [Contributing](../CONTRIBUTING.md).

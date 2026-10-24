# Ringo

A local harness for code review, design proposals, bounded council-guided
artifact work, and guarded improvement of its own prompts and pipelines. Ringo
runs model CLIs and assembles their results; it uses frozen provider models and
does not train model weights.

This repository owns the harness, its model calls and their budget. The original
[design council](https://github.com/SkylerLK510/DukChulBOT) owns proposal judging,
rubric scoring, decision gates and journals. A future GUI will be a separate
client; no GUI is implemented here.

## Run

Python 3.11+ and the standard library. From this checkout:

```sh
python3 ringo.py --help
python3 ringo.py review --input change.txt --backend codex-chatgpt --max-calls 1
python3 ringo.py design --input brief.md --out proposal.md
python3 ringo.py work --spec task.json --out /ABSOLUTE/PATH/TO/new-run
python3 -m unittest discover -s tests -q
```

For reviews, supply the change description, unified diff, complete changed files and
project README as one material file. For designs, supply the decision brief and hard
constraints. Review/design default to one strong model call before retries; custom
plans are JSON files under `plans/`, with instructions under `prompts/`.

`work` revises a declared set of text files: a writer model returns complete
files, fixed executable checks run on them, and a separate council assessment
decides whether to revise or stop. A result that passes every check and is
recommended is written to the run's `final/` folder for human review; nothing is
applied, adopted or published. The task names the council bridge and rubric
explicitly. See [the council workflow guide](docs/COUNCIL-WORKFLOW.md) for the
task format, outputs, budgets and trust limits.

Backends include Claude Code, Codex, local Ollama and commands that read stdin.
`codex-chatgpt` forces native ChatGPT authentication, strips API-key and endpoint
overrides, disables nested tools/skill discovery and records reported tokens from
the CLI's JSON events. Its dollar cost remains unknown. The existing `codex`
preset remains available for the caller's configured authentication.
Authenticate with the provider's supported CLI. Usage consumes the selected account's
allowance or API billing according to authentication and plan settings. Unknown
cost is not zero. `--trace` uses a new folder outside this checkout; it contains
private material. Opening the CLI help or running tests makes no provider calls.

## Evaluation and improvement

See [the harness guide](docs/HARNESS.md) and [the agent skill](skills/ringo-harness/SKILL.md).

```sh
python3 evaluate.py --help
python3 improve.py --help
```

Evaluation freezes inputs, separates repository groups, grades blind, records an
integrity ledger, and controls validation/test reuse. Improvement campaigns search
review prompts/plans against dev evidence. A finalist requires a sealed-test pass
and human adoption. These gates stay advisory; no automatic adoption occurs.
`work` repairs one task's files and does not change Ringo's prompts or plans.
A dollar-budgeted run requires an explicitly declared fallback for unpriced attempts.
The final admitted call can exceed a remaining budget.

Backend calls start in empty directories, which are not filesystem boundaries.
Codex and custom commands may retain file-access tools; use an external boundary
when answer keys must be inaccessible. Evaluation locking currently uses POSIX
`fcntl`.

## Research credit

**Research credit.** We credit **Peng Xia and coauthors at Google Cloud AI Research**, with collaborators at UNC-Chapel Hill, Stanford University and Washington University in St. Louis, for *RRSI: Regularized Recursive Self-Improvement of Agent Harnesses* (2026). Their work inspired this portion of Ringo's improvement loop. Ringo adapts selected ideas from the paper. See the [original paper](https://arxiv.org/abs/2609.24972) and the [authors' project page](https://regularized-rsi.com/).

The proposal loop also draws on [Meta-Harness](https://arxiv.org/abs/2603.28052).
Ringo implements selected ideas; it is not a full reproduction of either paper.
See the guide for the implemented limits and required confirmation.

## Repository separation

The runtime has no import or hardcoded path dependency on the council checkout.
`backends.py` contains the minimal extracted subprocess, command-preset and JSON
parsing helpers. Council judging remains in its own repository and is not bundled
into this CLI. `work` requires an explicitly supplied external council bridge
command and rubric: the council scores the replies it is given and makes no model
calls, while Ringo makes the writer and judge calls and owns their budget.
Existing evaluation stores must be re-frozen for this source layout; do not reuse
old digests or copy sealed data into this repository.

A publication license has not yet been selected. No performance or quality gain
is claimed from this repository extraction.

## Use from an agent

The [Ringo workflow skill](skills/ringo-harness/SKILL.md) connects ordinary coding
and design work to actual bounded harness calls. Link that folder into the agent's
user skills directory and set its local instructions to use it for substantive
engineering work. For Codex, the user skill location is `~/.agents/skills/`; for
Claude Code it is `~/.claude/skills/`. Use `RINGO_HOME` to identify this checkout
when needed. The CLI remains explicitly invoked; installing a skill is guidance,
not a background daemon. Start a new agent session after changing global instructions.

## Intended research use

The target includes intensive ML and finance research. See the
[research requirements](docs/RESEARCH-REQUIREMENTS.md) for evidence, reproducibility,
numerical testing and domain validation goals. The selected direction uses Design
Council for structured assessment, with Ringo providing execution and validation.
Basic integration exists: `work` combines generated files, fixed checks and council
assessment. The research workflow those requirements describe is future work, and
the current harness has not been validated for that workload.

## Research documents

The [research index](docs/RESEARCH.md) links HarBenchmark, benchmark coverage gaps,
security/CI-bot research, release architecture, local latency measurements and
optimization options. The standalone HarBenchmark scorer and fixture tests are
included; guides clearly label runtime prototype changes pending separate publication.

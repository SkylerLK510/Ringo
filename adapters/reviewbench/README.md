# Ringo on ReviewBench

> Publication status (2026-10-24): this documents a local adapter prototype.
> Its runtime, container files and adapter tests are pending separate publication;
> this documentation upload does not make the image available or claim a score.

An image that runs one Ringo review per pull request under the
[ReviewBench agent contract](https://github.com/review-bench/ReviewBench/blob/main/AGENT_CONTRACT.md).
`entrypoint.sh` reads the contract's inputs, writes the PR text (title and body
from `pr.json`) and runs:

```sh
python3 ringo.py review --repo /work/repo --base "$RB_BASE" --head "$RB_HEAD" --pr-text PR.txt \
  --findings-json "$RB_OUT" --rb-pr-json "$RB_PR_JSON" --agent "$RB_AGENT" --backend claude
```

Ringo builds the material from git objects (the same three-dot diff as
`RB_DIFF`, plus the changed files and the unchanged code they depend on). It
splits the review into findings, one entry per finding and file, and checks
each file against `RB_HEAD`, clamping its lines. A finding whose file is not
in the repository is dropped and counted in the summary Ringo prints to
stderr. A failed review exits non-zero and leaves no findings file.

## Build

From the root of the Ringo checkout, for the platform the benchmark runs:

```sh
docker build --platform linux/amd64 -f adapters/reviewbench/Dockerfile \
  --build-arg CLAUDE_CODE_VERSION=<version> -t ringo-reviewer:dev .
```

Pin `CLAUDE_CODE_VERSION`, and register the image by digest, so a run can be
reproduced. Without the build argument, the image installs the latest Claude CLI.

## Credentials and settings

- `ANTHROPIC_API_KEY`: the Claude CLI's credential inside the container (the
  manifest's `secrets.env`). Egress: `api.anthropic.com`. A subscription login
  on your machine does not reach the container.
- `RB_CONFIG_BACKEND`: a Ringo backend spec, default `claude`.
- `RB_CONFIG_MODEL`: a model for the `claude` backend, for example
  `claude-sonnet-5`. With another backend, put the model in its
  `claude:`/`codex:` command instead; the adapter refuses the pair.
- `RB_CONFIG_PIPELINE`: a plan path inside the image, for example
  `plans/review.json` (the default plan: one call, at most two).
- `RB_CONFIG_TIMEOUT`: seconds per call, default 780, so two attempts fit
  the default 30-minute limit per pull request.

The adapter prints the settings it applied, as the contract asks, for example
`ringo settings: backend=claude model=claude-sonnet-5 pipeline=built-in timeout=780 agent=ringo`.

## Try it locally

From a ReviewBench checkout (needs docker, git and jq):

```sh
scripts/try-agent.sh ringo-reviewer:dev --pr 0 -e ANTHROPIC_API_KEY -e RB_CONFIG_MODEL=claude-sonnet-5
```

It runs the image the way the benchmark does and checks each findings file.
`--set test` (the default) covers the 25 test pull requests; `--set full`
covers all of them.

## Score locally

```sh
npm ci
npm run judge -- --candidate "$run/findings" --provider anthropic --model claude-sonnet-5 \
  --output "$run/scoring/results.json" --concurrency 4
```

The leaderboard is judged by Claude Sonnet 5. Another judge model changes
the scores. A pull request that failed has no findings file and drops out of
the offline score, so check that `try-agent.sh` reported `failed 0`. Offline
numbers are for development only. No leaderboard result is claimed for Ringo.

The entrypoint is covered offline by `tests/test_findings.py`, which uses a
fake `claude` CLI. No image build or model call happens in the tests.

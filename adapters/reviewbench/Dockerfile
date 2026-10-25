# Ringo as a ReviewBench reviewer image (agent contract: AGENT_CONTRACT.md in github.com/review-bench/ReviewBench).
# Build from the root of the Ringo checkout, for linux/amd64:
#   docker build --platform linux/amd64 -f adapters/reviewbench/Dockerfile -t ringo-reviewer:dev .
#
# Credential (manifest secrets.env): ANTHROPIC_API_KEY, for the Claude CLI. It is never baked into the image.
# Egress (manifest egress): api.anthropic.com
# Configuration (RB_CONFIG_*): BACKEND (default claude), MODEL, PIPELINE (a plan path in the image), TIMEOUT.
FROM python:3.13-slim-trixie

# Pin the Claude CLI for a reproducible image: --build-arg CLAUDE_CODE_VERSION=<version>.
ARG CLAUDE_CODE_VERSION=latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates nodejs npm \
    && npm install -g --no-audit --no-fund "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && npm cache clean --force \
    && rm -rf /var/lib/apt/lists/*

# No auto-update, telemetry or error reporting: only the model API is reachable during a run.
ENV DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 PYTHONDONTWRITEBYTECODE=1

WORKDIR /ringo
COPY *.py ./
COPY plans ./plans
COPY prompts ./prompts
COPY adapters/reviewbench/entrypoint.sh ./entrypoint.sh
RUN chmod 0755 entrypoint.sh

ENTRYPOINT ["/ringo/entrypoint.sh"]

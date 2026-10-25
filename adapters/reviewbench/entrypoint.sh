#!/usr/bin/env bash
# ReviewBench agent contract for Ringo: one review of the pull request mounted at /work, findings to RB_OUT.
# Ringo builds the review material itself from git objects (base...head, the same three-dot diff as RB_DIFF),
# makes the plan's calls (the built-in plan: one call, at most two) and writes the contract JSON with every
# finding's file checked against RB_HEAD. Any failure exits non-zero, which the benchmark counts as a failed PR.
set -euo pipefail

for name in RB_BASE RB_HEAD RB_OUT RB_PR_JSON; do
  if [ -z "${!name:-}" ]; then echo "ringo-adapter: missing $name" >&2; exit 2; fi
done
ringo="${RINGO_HOME:-/ringo}"  # the Ringo checkout in the image
repo="${RB_REPO:-/work/repo}"
agent="${RB_AGENT:-ringo}"
backend="${RB_CONFIG_BACKEND:-claude}"
model="${RB_CONFIG_MODEL:-}"
pipeline="${RB_CONFIG_PIPELINE:-}"
timeout="${RB_CONFIG_TIMEOUT:-780}"  # seconds per call; two attempts fit the default 30-minute PR limit

spec="$backend"
if [ -n "$model" ]; then
  if [ "$backend" != claude ]; then
    echo "ringo-adapter: RB_CONFIG_MODEL works only with RB_CONFIG_BACKEND=claude; for $backend set the model in a claude:/codex: command" >&2
    exit 2
  fi
  spec="claude:$(PYTHONPATH="$ringo" python3 -c 'import shlex, sys, backends, harness
print(shlex.join(harness.with_model(shlex.split(backends.JUDGE_PRESETS["claude"]), sys.argv[1])))' "$model")"
fi
# The benchmark names any configuration label whose value never appears in the agent's output.
echo "ringo settings: backend=$backend model=${model:-backend-default} pipeline=${pipeline:-built-in} timeout=$timeout agent=$agent"

# Without the benchmark's GIT_CONFIG_* variables, git would refuse the mounted checkout as unsafe.
git config --global --add safe.directory "$repo"

work="$(mktemp -d)"
python3 - "$RB_PR_JSON" > "$work/pr.txt" <<'PY'
import json, sys
pr = json.load(open(sys.argv[1], encoding='utf-8'))
print(f"# {pr.get('title') or ''}\n\n{pr.get('body') or ''}")
PY

args=(review --repo "$repo" --base "$RB_BASE" --head "$RB_HEAD" --pr-text "$work/pr.txt"
      --findings-json "$RB_OUT" --rb-pr-json "$RB_PR_JSON" --agent "$agent"
      --backend "$spec" --timeout "$timeout" --out "$work/review.md")
if [ -n "$pipeline" ]; then args+=(--pipeline "$pipeline"); fi

status=0
python3 "$ringo/ringo.py" "${args[@]}" || status=$?
if [ "$status" -ne 0 ]; then
  rm -f "$RB_OUT"  # no partial or empty findings file after a failure
  echo "ringo-adapter: review failed (exit $status)" >&2
  exit "$status"
fi
cat "$work/review.md" >&2

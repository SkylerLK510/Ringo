# Council-guided work

`python3 ringo.py work` revises a fixed set of text files until fixed executable
checks pass and a separate Design Council assessment recommends the result, or
until a bound is reached. It writes every candidate to a new run folder for human
review. It never edits a source checkout, adopts a result or publishes anything.

Ringo owns the model calls and their budget. The council checkout owns rubric
scoring and the decision gate. Ringo imports nothing from the council and has no
built-in path to it: each task names the council bridge command and rubric file.

## How a run works

Before inference, Ringo parses both backend commands and performs an offline bridge
`prepare` preflight to validate the bridge, brief, rubric and policy. Invalid setup
therefore stops before a writer call. The initial files are not scored or adopted.

Each round:

1. **Write.** The writer model receives the brief, the source material, the
   current files, the fixtures and earlier feedback. It must reply with one JSON
   object, without Markdown fences: `{"files": {...}, "rationale": "..."}`, giving
   the complete text of exactly the declared editable files. A reply with fences,
   extra or missing paths, unsafe paths, duplicate keys or `NaN` is a `malformed`
   round: the problem becomes feedback for the next round.
2. **Stop on repeats.** A candidate identical to an earlier one ends the run as
   `repeated_candidate` before any check or judge call.
3. **Check.** The candidate files and the read-only fixtures are written to
   `round-N/candidate/`, and each check command runs there in order. A check
   passes when it exits with 0. Afterwards every submitted file and fixture must
   be unchanged and not a symlink; otherwise the run stops with `error`.
4. **Assess.** Ringo asks the bridge to `prepare` judge prompts, makes one judge
   call per sample, and asks the bridge to `score` the replies. The judges see
   the source material, the candidate files, the rationale and the observed check
   results with log excerpts. They do not see the fixture source. The council
   assesses every well-formed new candidate, including one whose checks failed.
5. **Decide.** A round is `ready_for_review` only when every check passed, the
   council report is complete and its decision is `recommend`. Ringo then copies
   the editable files, not the fixtures, to `final/` and stops. Otherwise the check
   results, the decision and the scored rows become feedback for the next round.

## Task spec

A task is one JSON file. This complete example asks for a small numerical
function. Replace the two placeholder paths with the absolute paths of a council
checkout:

```json
{
  "version": 1,
  "brief": "Implement mean(values) in stats.py.\n\nHard constraints:\n- Return the arithmetic mean of a nonempty sequence of finite numbers as a float.\n- Raise ValueError for an empty sequence.\n- Use only the Python standard library.\n",
  "material": "Reference cases: mean([1, 2, 3]) is 2.0; mean([-1.5, 1.5]) is 0.0; mean([]) raises ValueError.",
  "files": {"stats.py": ""},
  "fixtures": {
    "check_stats.py": "import math, runpy, sys\nmean = runpy.run_path('stats.py')['mean']\nfailures = []\nfor values, expected in (([1, 2, 3], 2.0), ([-1.5, 1.5], 0.0), ([0.1] * 10, 0.1)):\n    got = mean(values)\n    if not isinstance(got, float) or not math.isclose(got, expected, rel_tol=1e-12, abs_tol=1e-12):\n        failures.append(f'mean({values!r}) returned {got!r}, expected {expected!r}')\ntry:\n    mean([])\nexcept ValueError:\n    pass\nelse:\n    failures.append('mean([]) did not raise ValueError')\nprint('\\n'.join(failures) or 'all reference cases passed')\nsys.exit(1 if failures else 0)\n"
  },
  "checks": [{"name": "reference cases", "argv": ["python3", "-I", "-B", "check_stats.py"], "timeout_s": 30}],
  "allow_execution": true,
  "council": ["python3", "/ABSOLUTE/PATH/TO/design-council/bridge.py"],
  "rubric": "/ABSOLUTE/PATH/TO/design-council/rubrics/code_change.json",
  "writer": "claude",
  "judge": "claude",
  "allow_tool_backends": false,
  "samples": 2,
  "max_rounds": 2,
  "max_calls": 6,
  "max_seconds": 600,
  "timeout_s": 180
}
```

The check reports failures by its exit status, not with `assert`, which
`python -O` removes. It loads `stats.py` by path because `-I` keeps the script's
folder off `sys.path`.

| Field | Default | Meaning |
|---|---|---|
| `version` | required | Must be `1`. Unknown fields are refused. |
| `brief` | required | The task. The council needs a `Hard constraints:` bullet list in it; every bullet becomes a mandatory check in the decision. |
| `material` | `""` | Fixed source material. The writer and every judge receive it as evidence, not as instructions. |
| `files` | required | Editable files: relative path to initial complete UTF-8 text (`""` for a new file). The writer must return exactly these paths. |
| `fixtures` | `{}` | Read-only files written beside each candidate, such as checks and reference data. Disjoint from `files`. |
| `checks` | required | One or more `{"name", "argv", "timeout_s"}` commands. `timeout_s` defaults to 30. They run in the candidate folder. |
| `allow_execution` | `false` | Must be `true`: checks execute code, including model-written code. |
| `council` | required | Bridge command as an argv list. Use absolute paths: it runs from an empty temporary folder. |
| `rubric` | required | Rubric file, absolute or relative to the spec's folder. |
| `writer`, `judge` | required | Backends: `claude`, `codex`, `codex-chatgpt`, `ollama:MODEL`, `claude:<command>` or any command that reads stdin. |
| `allow_tool_backends` | `false` | Must be `true` for any backend except `claude`, `codex-chatgpt` and `ollama:MODEL`, since the others may read local files. |
| `samples` | 2 | Judge calls per assessed round, 2 to 25. |
| `max_rounds` | 3 | Writer rounds, including malformed ones. |
| `max_calls` | 9 | Writer and judge calls together. |
| `max_seconds` | 600 | Wall-clock bound for the whole run. |
| `timeout_s` | 120 | Bound for each model call, capped by the time left. |
| `max_cost`, `unpriced_call_usd` | none | Optional dollar cap and fallback price for calls reporting none. A dollar cap requires a declared fallback. |

Paths in `files` and `fixtures` must be normalized relative POSIX paths without
backslashes or components that start with `.`, so `.github/` is refused. No path
may also be a parent folder of another. Each map holds at most 2 MB of text.

## Run

From the Ringo checkout:

```sh
python3 ringo.py work --help
python3 ringo.py work --spec /ABSOLUTE/PATH/TO/task.json --out /ABSOLUTE/PATH/TO/new-run
```

`--out` must be a new or empty folder outside the Ringo checkout. It is created
with mode 0700 because it holds the brief, material and every reply. This
command calls the selected models and uses their accounts.

## Results

| Exit | Status in `report.json` | Meaning |
|---|---|---|
| 0 | `ready_for_review` | All checks passed and the council recommended. `final/` holds the editable files. |
| 1 | `needs_revision` | Every round ran without a reviewable candidate. |
| 1 | `repeated_candidate` | The writer returned an earlier candidate again. |
| 1 | `budget_exhausted` | The call cap, cost cap, admission refusal or overall wall-clock bound stopped the run, including a model/bridge timeout at that deadline. |
| 1 | `error` | For example a mutated file, bridge failure, unreadable judge reply, changed council policy, or a per-operation timeout before the overall deadline. |
| 2 | none | The spec or output folder was refused before the run started. |

Deeply nested writer JSON is treated as malformed protocol output and can be
revised. An interrupted run, or one stopped by an exception the workflow does not
handle, writes `report.json` with status `interrupted` and then re-raises the
exception, so no report is printed. Each round records `malformed`,
`repeated_candidate`, `revise` or `ready_for_review`. A round stopped by a bound,
an error or an interruption has no status; the top-level `status` and `error`
say why.

The run folder holds `spec.json` (the loaded spec, with defaults filled in for
the bounds, switches and check timeouts), `source.json`
(the workflow's SHA-256), `trace.jsonl`, the successful replies under `steps/`, and
per round `proposal.json`, `candidate/`, the full `check-K.out` and `check-K.err`
logs and `council.json`. `report.json` keeps the first 16,000 bytes of each log.
`shortcomings.json` holds the final feedback for development use.

`final/` exists only after all checks passed and a complete council report
recommended the candidate. Review it and copy what you accept yourself. A pass
shows that the supplied checks passed and the council recommended the files. It
does not show that the files are correct on unseen cases.

## Evidence and council policy

The writer prompt treats the source material and feedback as evidence rather
than instructions. The judge system prompt does the same for the source material,
candidate files, rationale and check logs, and the proposal labels the material as
fixed task evidence. Judges are told to assess claims against that material and
not to infer unobserved checks. Labels reduce prompt injection; they do not
prevent it.

The bridge's fingerprint covers the judge prompts and the council policy: rubric
content, `council.py`, `bridge.py` and the judge skill. `score` refuses replies for
a different fingerprint. Ringo records the first round's policy hash as
`council_policy_sha256` and stops with `error` if a later round reports a
different one, so the policy stays fixed across rounds. An unreadable judge reply
ends the run with `error`; the bridge does not retry it.

## Budgets and accounting

Writer and judge calls share one `harness.Run`. A call is reserved before it
starts and counted from then on. A call that fails or times out stays counted,
and its cost is unknown unless the backend reported it. Unknown cost is not zero.
Without `unpriced_call_usd`, `cost_usd` excludes it and `unpriced_calls` counts it;
with `unpriced_call_usd`, that price is added to `cost_usd` instead. Setting
`max_cost` without a fallback is rejected before the run starts.

A Python adapter that calls `workflow.run(..., call_fn=...)` can raise
`harness.AdmissionRefused` only when it refused before starting any provider call.
Ringo then releases the reservation and counts it under `admission_refusals`, not
as a call. Every other exception, including a generic `BudgetExceeded`, is treated
as a potentially started call. The built-in backends never raise it. Reports
written before this distinction existed may count a refused admission as an
unpriced call; they have not been rewritten.

For native ChatGPT authentication, select `codex-chatgpt` for writer/judge. It
forces the CLI's ChatGPT login method, removes API-auth/endpoint overrides and
records reported input/cache/output tokens from JSON events. Its price remains
unknown; it consumes the selected account's subscription allowance.

Caps stop new calls. The last admitted call can overshoot a cost or token
threshold. Ringo has no built-in token cap; an adapter must enforce one.

## Trust boundaries

The bridge, check commands and custom backends are trusted code chosen by the task
author. Candidate and check folders are not sandboxes. Checks inherit your
environment, including any credentials in it, and run model-written code with your
user's permissions. Use an external boundary, such as a container or a separate
account, when that is unacceptable.

Model and bridge calls run from empty temporary folders without the TypeSafe key;
those folders are not filesystem boundaries. Full check logs are spooled to disk
without a quota. Timeouts bound waiting and admission, not CPU, GPU, memory or
disk use. After each check, on success, timeout or interruption, Ringo kills the
check's process group on POSIX. Processes that left the group are not stopped. On
Windows only the direct process is killed.

## Task repair is not workflow improvement

A `ready_for_review` run repairs one task's artifacts. It is not evidence that
Ringo's workflow improved on unseen tasks. `improve.py` still searches only review
prompts and plans, with held-out controls and human adoption. Reliability for ML
or finance research has not been established; see the
[research requirements](RESEARCH-REQUIREMENTS.md).

## Tests

The offline suite makes no provider calls. Workflow tests use a fake bridge, so they
run without a council checkout. Set `RINGO_COUNCIL_ROOT` to also run a smoke test
against the real bridge with deterministic judge replies:

```sh
python3 -m unittest discover -s tests -q
RINGO_COUNCIL_ROOT=/ABSOLUTE/PATH/TO/design-council python3 -m unittest tests.test_workflow -q
```

Passing tests show that the plumbing and gates behave as written. They are not
evidence of model judgment quality or comparative quality.

# Council-guided work

> Publication status (2026-10-24): this guide includes tested local prototype
> capabilities whose runtime changes are pending separate publication. This
> documentation upload does not activate a release or change the default runtime.

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
therefore stops before a writer call. The initial files are not scored or adopted:
a round-1 reply that returns them unchanged ends the run as `repeated_candidate`,
as an unchanged reply does in any later round.

Each round:

1. **Write.** The writer model receives the brief, the source material, the
   current files, the fixtures and earlier feedback. It must reply with one JSON
   object, without Markdown fences, in one of two forms, never both:
   - `{"files": {...}, "rationale": "..."}` gives the complete text of exactly the
     declared editable files. Use it for a new file or a rewrite of most of a file.
   - `{"edits": [{"path": "...", "old": "...", "new": "..."}], "rationale": "..."}`
     changes only what it names; files it does not edit stay as they are. Use it
     for targeted changes to a long file, where restating the whole file costs
     output tokens and risks unrequested drift in text that should not change.
     Each `old` must occur exactly once in the current text the writer was shown.
     Every `old` is matched against that same text, not after earlier edits, so
     the result does not depend on edit order, and edits must not overlap.
     Matching is exact: an anchor that matches no place or several places fails,
     and the error names the edit and, for no match, the line where the longest
     matching prefix of `old` occurs. One failed edit means none is applied.

   Ringo turns edits into complete files before anything else, so the repeat
   check, the checks, the mutation and symlink check and the judges all work on
   complete text, never on edits. A reply with fences, both forms, extra or
   missing paths in `files`, unsafe or undeclared paths, a failed edit, a result
   over the size limit, duplicate keys or `NaN` is a `malformed` round: it makes no
   check or judge calls, the current files stay unchanged, and the next round gets
   the problem together with the earlier check and council feedback, not instead.
2. **Stop on repeats.** A candidate identical to the files the writer was shown
   or to an earlier candidate ends the run as `repeated_candidate` before any
   check or judge call.
3. **Check.** The candidate files and the read-only fixtures are written to
   `round-N/candidate/`, and each check command runs there in order. A check
   passes when it exits with 0. Afterwards every submitted file and fixture must
   be unchanged and not a symlink; otherwise the run stops with `error`.
   If any check fails or times out, the round is `revise` with
   `assessment_skipped: checks_failed`. Ringo sends the observed check results
   straight to the next writer round without making Council model calls. Earlier
   Council feedback, if any, is retained as `previous_council`, identified by the
   hash of the candidate it assessed. An exhausted round limit leaves the task
   `needs_revision`; failed checks never authorize a final artifact.
4. **Assess.** Ringo asks the bridge to `prepare` judge prompts, makes one judge
   call per sample, and asks the bridge to `score` the replies. The judges see
   the source material, the candidate files, the rationale and the observed check
   results with log excerpts. They do not see the fixture source. The council
   assesses new candidates only after every executable check passes. Passing
   checks alone does not authorize a final artifact: the full Council gate remains.
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
  "samples": 6,
  "max_rounds": 2,
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
| `files` | required | Editable files: relative path to initial complete UTF-8 text (`""` for a new file). A `files` reply must give exactly these paths; an `edits` reply may change any of them. |
| `fixtures` | `{}` | Read-only files written beside each candidate, such as checks and reference data. Disjoint from `files`. |
| `checks` | required | One or more `{"name", "argv", "timeout_s"}` commands. `timeout_s` defaults to 30. They run in the candidate folder. |
| `allow_execution` | `false` | Must be `true`: checks execute code, including model-written code. |
| `council` | required | Bridge command as an argv list. Use absolute paths: it runs from an empty temporary folder. |
| `rubric` | required | Rubric file, absolute or relative to the spec's folder. See "Choosing a rubric" below. |
| `writer`, `judge` | required | Backends: `claude`, `codex`, `codex-chatgpt`, `ollama:MODEL`, `claude:<command>`, `codex:<command>` or any command that reads stdin. |
| `allow_tool_backends` | `false` | Must be `true` for any backend except `claude`, `codex-chatgpt` and `ollama:MODEL`, since the others may read local files. |
| `samples` | 6 | The most judge calls per assessed round, 2 to 25. 6 is the council's default; 2 is a smoke-test value only (see below). |
| `first_judges` | half of `samples`, at least 3 | Judges the Council decides on first; the remaining samples are called only if those do not give a complete `recommend`. Set it to `samples` to always run every sample. |
| `max_rounds` | 3 | Writer rounds, including malformed ones and check failures. Allow at least 2 for a correction after the first answer. With `samples` it bounds the calls (see below). |
| `max_seconds` | 600 | Wall-clock bound for the whole run. |
| `timeout_s` | 120 | Bound for each model call, capped by the time left. |
| `max_cost`, `unpriced_call_usd` | none | Optional dollar cap and fallback price for calls reporting none. A dollar cap requires a declared fallback. |

**Calls.** There is no call cap: the task's rounds and samples decide the calls.
Each round makes one writer call; a new candidate that passes every executable
check then gets `first_judges` judge calls. If the Council's decision on those is
a complete `recommend`, the round stops there; otherwise the remaining samples are
called and the Council decides on all of them. Judge k's prompt depends only on k,
so the first stage's judges are the full gate's first judges, and Ringo refuses a
council that gives different first requests at the smaller sample count. A
first-stage decision is the Council's own decision on that many samples: its
probabilities and confidence come from those samples, not from `samples`. Each
assessed round records `judges` and `council_stages`, and a ready report records
`council_judges` and `council_samples`. Failed or timed-out checks
and malformed replies skip judges for that round; a repeated candidate ends the run. Calls are not
retried. So a run makes at most `max_rounds x (1 + samples)` calls: the example
above makes at most 2 x (1 + 6) = 14. If the first candidate fails checks and the
second passes and the first three judges recommend, it uses 2 writer + 3 judge
calls = 5; if they do not, 2 + 6 = 8. If both fail checks, it uses
only 2 writer calls and remains `needs_revision`. No correction or judge calls
are added beyond the declared round/time bounds. A spec that still sets the removed
`max_calls` field is refused ("max_calls was removed: calls are set by max_rounds
and samples"). `max_seconds`, `timeout_s` and `max_cost` bound time and money.

**Samples.** Use the default of 6 for any run whose verdict should count. Set
`samples` to 2 only for a smoke test of the plumbing: with two samples, any split
between them on a scored question gives that question a confidence of at most
0.33, below the council's `min_confidence` of 0.5, so the round escalates even
when both answers are good; and a veto that one of the two samples answers yes
gets p = 0.5, below the 0.7 threshold and outside the shadow gate's veto band, so
it passes silently. Six is the smallest even count at which one dissenting sample
on any question type still clears the gate, and a 5-of-6 veto still fires. Keep
the count even: the council alternates level order across samples, and an odd
count splits it unevenly.

**Choosing a rubric.** The rubrics live in the council checkout under
`rubrics/`. Use `code_change.json` for code, `system_design.json` for design
proposals, `report.json` for documents and reports judged against supplied
material (research write-ups, reviews, guides), and `artifact_design.json` for
visual artifacts such as pages and profiles. `artifact_design.json` asks about
visual hierarchy, link destinations and screen sizes, which do not fit a
Markdown report.

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
| 1 | `repeated_candidate` | The writer returned the files it was shown, or an earlier candidate, again. |
| 1 | `budget_exhausted` | The cost cap, admission refusal or overall wall-clock bound stopped the run, including a model/bridge timeout at that deadline. |
| 1 | `error` | For example a mutated file, bridge failure, unreadable judge reply, changed council policy, or a per-operation timeout before the overall deadline. |
| 2 | none | The spec or output folder was refused before the run started. |

Deeply nested writer JSON is treated as malformed protocol output and can be
revised. An interrupted run, or one stopped by an exception the workflow does not
handle, writes `report.json` with status `interrupted` and then re-raises the
exception, so no report is printed. Each round records `malformed`,
`repeated_candidate`, `revise` or `ready_for_review`. A round stopped by a bound,
an error or an interruption has no status; the top-level `status` and `error`
say why.

Rounds with failed checks record `assessment_skipped: checks_failed`, their
candidate and full check logs, but no Council decision or `council.json` for
that round. Feedback distinguishes these observed failures from any earlier
Council assessment; a skipped assessment is never represented as a recommendation.

The run folder holds `spec.json` (the loaded spec, with defaults filled in for
the bounds, switches and check timeouts), `source.json`
(the workflow's SHA-256), `trace.jsonl`, the successful replies and the first 1 MB of each failed call's raw output under `steps/`, and
per round `proposal.json` (the complete applied files, the rationale and, when
the writer sent edits, those edits), `candidate/`, the full `check-K.out` and `check-K.err`
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

Writer and judge calls share one `harness.Run`, which counts calls but does not
cap them. A call is reserved before it starts and counted from then on. A call that fails or times out stays counted,
and its cost is unknown unless the backend reported it. Unknown cost is not zero.
Without `unpriced_call_usd`, `cost_usd` excludes it and `unpriced_calls` counts it;
with `unpriced_call_usd`, that price is added to `cost_usd` instead. A call that
reported no token usage counts under `untokened_calls`; the token sums leave it out. Setting
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
records reported input/cache/output tokens from JSON events. Like every Codex
backend it runs with a private CODEX_HOME and HOME (see the harness guide). Its price remains
unknown; it consumes the selected account's subscription allowance.

The cost cap and the wall-clock bound stop new calls; there is no call cap. The
last admitted call can overshoot a cost or token threshold. Ringo has no built-in
token cap; an adapter must enforce one.

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

# Work: checks, one review, opt-in Council

> Publication status (2026-10-24): this guide includes tested local prototype
> capabilities whose runtime changes are pending separate publication. This
> documentation upload does not activate a release or change the default runtime.

`python3 ringo.py work` revises a fixed set of text files until fixed executable
checks pass and one review's findings have been answered, or until a bound is
reached. It writes every candidate to a new run folder for human review. It never
edits a source checkout, adopts a result or publishes anything.

The review is Ringo's own review plan (`plans/review.json`), run once on the first
candidate that passes every check. A task can opt into the separate Design Council
instead by naming a council bridge; see "Why the Council is opt-in" below. Ringo
owns the model calls and their budget. The council checkout owns rubric scoring and
the decision gate. Ringo imports nothing from the council and has no built-in path
to it: a task that opts in names the council bridge command and rubric file.

## How a run works

Before inference, Ringo parses both backend commands and, with a council, performs
an offline bridge `prepare` preflight to validate the bridge, brief, rubric and
policy. Invalid setup therefore stops before a writer call. The initial files are not scored or adopted:
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
   straight to the next writer round without a review or Council call. An earlier
   review, if any, is retained as `previous_review`, and earlier Council feedback
   as `previous_council`, identified by the hash of the candidate it assessed. An
   exhausted round limit leaves the task `needs_revision`; failed checks never
   authorize a final artifact.
4. **Review (default).** The first candidate that passes every check gets one
   review call on the `judge` backend. The reviewer sees the brief, the writer's
   rationale, the source material, a diff for each file that started nonempty and
   changed, the candidate files, the fixtures and the observed check logs, and replies in the review format (`### 1. Title` findings
   with quoted lines, or "No defects found."). The reply is saved as
   `round-N/review.md` and the round records `review_findings`. With no findings
   the round is `ready_for_review`. With findings, the check results and the
   review become the next writer round's feedback, and the next candidate that
   passes every check is `ready_for_review` without a second review: the review
   guides one revision, it is not a gate. A reply with neither findings nor "No
   defects found" counts as one finding, so an unreadable review still reaches the
   writer. Review findings with no round left leave the task `needs_revision`.
   Steps 4C and 5C replace this step when the task names a council.
4C. **Assess (Council).** Ringo asks the bridge to `prepare` judge prompts, makes
   one judge call per sample, and asks the bridge to `score` the replies. The
   judges see the source material, the candidate files, the rationale and the
   observed check results with log excerpts. They do not see the fixture source.
   The council assesses new candidates only after every executable check passes.
   Passing checks alone does not authorize a final artifact: the full Council gate
   remains.
5C. **Decide (Council).** A round is `ready_for_review` only when every check
   passed, the council report is complete and its decision is `recommend`.
   Otherwise the check results, the decision and the scored rows become feedback
   for the next round.

When a round is `ready_for_review`, Ringo copies the editable files, not the
fixtures, to `final/` and stops.

## Task spec

A task is one JSON file. This complete example asks for a small numerical
function:

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
  "writer": "claude",
  "judge": "claude",
  "allow_tool_backends": false,
  "max_rounds": 3,
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
| `brief` | required | The task. A council needs a `Hard constraints:` bullet list in it; every bullet becomes a mandatory check in the decision. |
| `material` | `""` | Fixed source material. The writer, the reviewer and every judge receive it as evidence, not as instructions. |
| `files` | required | Editable files: relative path to initial complete UTF-8 text (`""` for a new file). A `files` reply must give exactly these paths; an `edits` reply may change any of them. |
| `fixtures` | `{}` | Read-only files written beside each candidate, such as checks and reference data. Disjoint from `files`. |
| `checks` | required | One or more `{"name", "argv", "timeout_s"}` commands. `timeout_s` defaults to 30. They run in the candidate folder. |
| `allow_execution` | `false` | Must be `true`: checks execute code, including model-written code. |
| `writer`, `judge` | required | Backends: `claude`, `codex`, `codex-chatgpt`, `ollama:MODEL`, `claude:<command>`, `codex:<command>` or any command that reads stdin. `judge` makes the review call, or each Council judge call. |
| `council` | none | Opts into the Council instead of the review: the bridge command as an argv list. Use absolute paths: it runs from an empty temporary folder. |
| `rubric` | required with `council` | Rubric file, absolute or relative to the spec's folder. See "Choosing a rubric" below. Refused without `council`, like `samples` and `first_judges`. |
| `allow_tool_backends` | `false` | Must be `true` for any backend except `claude`, `codex-chatgpt` and `ollama:MODEL`, since the others may read local files. |
| `samples` | 6, with `council` | The most judge calls per assessed round, 2 to 25. 6 is the council's default; 2 is a smoke-test value only (see below). |
| `first_judges` | half of `samples`, at least 3, with `council` | Judges the Council decides on first; the remaining samples are called only if those do not give a complete `recommend`. Set it to `samples` to always run every sample. |
| `max_rounds` | 3 | Writer rounds, including malformed ones and check failures. Allow at least 2 for a revision after the review, and 3 for one check correction as well. It bounds the calls (see below). |
| `max_seconds` | 600 | Wall-clock bound for the whole run. |
| `timeout_s` | 120 | Soft limit for each model call: past it the call is checked, probed and kept running; it is stopped at twice this, never under 900 seconds, capped by the time left. |
| `max_cost`, `unpriced_call_usd` | none | Optional dollar cap and fallback price for calls reporting none. A dollar cap requires a declared fallback. |
| `protocol` | `json` | `text` replaces the JSON writer protocol with `@@@` blocks (`prompts/work-code.md`, or `work-doc.md` when every editable file is a document): code is not JSON-escaped, and revisions send `REPLACE` edits. A JSON reply is still read. |
| `roles` | `{}` | Model and reasoning effort per call role: `writer` (round 1), `revision`, `escalate` (a round after failed checks; falls back to `revision`), `tests` and `review`, each `{"model", "effort"}`. Codex takes both; a Claude backend takes a model only. |
| `spec_tests` | none | `{"python", "timeout_s"}`: one call (role `tests`) writes a unittest module from the brief and the starting files alone, never a candidate. It runs after the checks pass; its failures drive one revision before the review, and the writer may `DISPUTE` a test with the brief sentence it contradicts. A suite that cannot load is dropped. Code tasks only; not with `council`. |
| `candidates` | 1 | 1 to 4. Round 1 makes this many writer calls on the same input (in parallel unless a cost cap is set); each distinct candidate runs the checks and the spec tests, and the one passing the checks with the most spec tests continues. Not with `council`. |
| `protocol_repair` | `false` | When no reply of a round can be applied (a bad anchor, a missing block), make one immediate repair call to the same role with the exact error, asking for complete FILE blocks, instead of spending the round. At most one per round. |
| `no_regression` | `false` | Once a candidate passes the checks, a later revision that fails a check or newly fails a spec test it passed is set aside (`round-N/rejected`), and the writer gets the earlier code back with what broke. With `scripts`, a document revision that introduces a number no successful script printed (and the brief does not give) is set aside too, so a review's claim alone cannot overturn a computed value. With `candidates`, a spec test that every candidate fails stops gating whatever this says (dual agreement). |
| `stop_on_checks` | none | `{"retain": r}` with r from 0.05 to 1: a round-1 candidate that passes every check (with no failing spec tests) is ready for review without the review call, unless a random draw below r keeps it for the full run. The draw is logged in `report.json` as `stop`, so the kept runs, the retention cohort, can be weighted in an audit. `retain: 1` records eligibility and changes nothing. It only removes calls. Not with `council`. See [AGCL](AGCL.md). |
| `scripts` | none | `{"python", "timeout_s"}` (text protocol; default 60 s): a document writer adds one `@@@ SCRIPT` that computes its numbers from the fixtures (`prompts/work-doc-script.md`). Ringo runs it in a scratch copy and shows the output to the review, and to the writer in the next round (one round, while the review still fits) unless every number the document states is already given or printed. The script is never delivered. |

**Calls.** With `candidates`, round 1 makes that many writer calls, so a run makes at most
`max_rounds - 1 + candidates` writer calls (plus at most one repair call per round with `protocol_repair`), plus
one `spec_tests` call when set; scripts and checks are not model calls. There is no call cap: the task's rounds decide the calls. Each round
makes one writer call, and one review call follows the first candidate that passes
every check. The review call is retried once if it fails or comes back empty, like
any harness pass. So a run makes at most `max_rounds + 2` calls: the example above
makes 2 when the review finds nothing, 3 when its findings drive a revision, and
4 when a check failure comes first.

With a council, each new candidate that passes every executable check gets
`first_judges` judge calls instead. If the Council's decision on those is
a complete `recommend`, the round stops there; otherwise the remaining samples are
called and the Council decides on all of them. Judge k's prompt depends only on k,
so the first stage's judges are the full gate's first judges, and Ringo refuses a
council that gives different first requests at the smaller sample count. A
first-stage decision is the Council's own decision on that many samples: its
probabilities and confidence come from those samples, not from `samples`. Each
assessed round records `judges` and `council_stages`, and a ready report records
`council_judges` and `council_samples`. Failed or timed-out checks
and malformed replies skip judges for that round; a repeated candidate ends the run.
A call is retried only when the person stops it after its soft limit, once. So a council
run makes at most `max_rounds x (1 + samples)` calls when nothing is stopped: with
`max_rounds` 2 and `samples` 6, at most 2 x (1 + 6) = 14. If the first candidate fails checks and the
second passes and the first three judges recommend, it uses 2 writer + 3 judge
calls = 5; if they do not, 2 + 6 = 8. If both fail checks, it uses
only 2 writer calls and remains `needs_revision`. Each stopped call adds one retry,
and each attempt that passes its soft limit adds at most one probe, so the bound is
four times that. A spec that still sets the removed
`max_calls` field is refused ("max_calls was removed: calls are set by max_rounds
and samples"). `max_seconds`, `timeout_s` and `max_cost` bound time and money.

**Samples (council only).** Use the default of 6 for any run whose verdict should count. Set
`samples` to 2 only for a smoke test of the plumbing: with two samples, any split
between them on a scored question gives that question a confidence of at most
0.33, below the council's `min_confidence` of 0.5, so the round escalates even
when both answers are good; and a veto that one of the two samples answers yes
gets p = 0.5, below the 0.7 threshold and outside the shadow gate's veto band, so
it passes silently. Six is the smallest even count at which one dissenting sample
on any question type still clears the gate, and a 5-of-6 veto still fires. Keep
the count even: the council alternates level order across samples, and an odd
count splits it unevenly.

**Choosing a rubric (council only).** The rubrics live in the council checkout under
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
| 0 | `ready_for_review` | All checks passed, after the review's one revision if it had findings (with a council: and the council recommended). `final/` holds the editable files. |
| 1 | `needs_revision` | Every round ran without a reviewable candidate, including review findings left with no round to revise. |
| 1 | `repeated_candidate` | The writer returned the files it was shown, or an earlier candidate, again. |
| 1 | `budget_exhausted` | The cost cap, admission refusal or overall wall-clock bound stopped the run, including a model/bridge timeout at that deadline. |
| 1 | `error` | For example a mutated file, a review call that failed twice, bridge failure, unreadable judge reply, changed council policy, or a per-operation timeout before the overall deadline. |
| 2 | none | The spec or output folder was refused before the run started. |

The command prints one compact JSON line: status, `final`, the `report.json` path,
rounds, usage, elapsed time and any error; everything else is in `report.json`.
The `--out` folder also holds `state.json`, updated after every call, so a caller
can block on `python3 ringo.py wait OUT` instead of polling (see `docs/HARNESS.md`,
**Run state**).

Deeply nested writer JSON is treated as malformed protocol output and can be
revised. An interrupted run, or one stopped by an exception the workflow does not
handle, writes `report.json` with status `interrupted` and then re-raises the
exception, so no report is printed. Each round records `malformed`,
`repeated_candidate`, `revise` or `ready_for_review`. A round stopped by a bound,
an error or an interruption has no status; the top-level `status` and `error`
say why.

Rounds with failed checks record `assessment_skipped: checks_failed`, their
candidate and full check logs, but no review, Council decision or `council.json`
for that round. Feedback distinguishes these observed failures from any earlier
Council assessment; a skipped assessment is never represented as a recommendation.

The run folder holds `spec.json` (the loaded spec, with defaults filled in for
the bounds, switches and check timeouts), `source.json`
(the workflow's SHA-256), `trace.jsonl`, the successful replies and the first 1 MB of each failed call's raw output under `steps/`, and
per round `proposal.json` (the complete applied files, the rationale and, when
the writer sent edits, those edits), `candidate/`, the full `check-K.out` and `check-K.err`
logs and `review.md` (or, with a council, `council.json`). `report.json` keeps the
first 16,000 bytes of each log, records `assessment` (`review` or `council`) and,
after a review, its path as `review`. `shortcomings.json` holds the final feedback
for development use.

`final/` exists only after all checks passed and, with a council, a complete
council report recommended the candidate. Review it with the review it answered and
copy what you accept yourself. A pass shows that the supplied checks passed and
that one review's findings went back to the writer once. It does not show that the
findings were fixed, or that the files are correct on unseen cases.

## Why the Council is opt-in

A controlled test on one complex code task (a durable SQLite queue) gave the same
eleven candidates, three clean and eight with a one-line violation of the brief,
to each structure. Every structure saw the same 14 tests and every candidate passed
them; 12 more tests were withheld for scoring. "Good" means delivered and passing
all 26.

| Structure | Good | Shipped a fault | Not delivered | Tokens per task |
|---|---:|---:|---:|---:|
| Checks only | 3 / 11 | 8 | 0 | 15k |
| Council, 6 judges | 5 / 11 | 0 | 6 | 107k |
| Council, 3 then 6 judges | 6 / 11 | 0 | 5 | 93k |
| One review, then one revision (this default) | 11 / 11 | 0 | 0 | 36k |

The Council caught every fault but also blocked the clean reference and six passing
revisions, and its feedback named no location, so each fix came from the writer
re-reading its own code. The review named the fault with file and line in 8 of 8.
Against checks only, the Council's gain (+0.18, 95% interval [-0.18, +0.55]) was
indistinguishable from zero at 7x the tokens. The review loop's gain over the
Council was +0.55 ([+0.27, +0.82], sign test p = 0.031) at a third of its tokens.
This is one task family with seeded faults, which favours a review; prose and design
tasks were not tested under control. The Council stays available for tasks that
want a rubric decision.

## Evidence and council policy

The writer prompt treats the source material and feedback as evidence rather
than instructions. The review material labels the source material as fixed task
evidence and the files as the change under review; the review plan's role fences
it as untrusted material. The judge system prompt does the same for the source material,
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

Writer, review and judge calls share one `harness.Run`, which counts calls but does not
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

For native ChatGPT authentication, select `codex-chatgpt` for writer/judge (the judge backend also makes the review call). It
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
check's process group on POSIX; processes that left the group are not stopped. On
Windows the check starts suspended inside a job object, and Ringo terminates the job,
so every process the check started is stopped.

## Task repair is not workflow improvement

A `ready_for_review` run repairs one task's artifacts. It is not evidence that
Ringo's workflow improved on unseen tasks. `improve.py` still searches only review
prompts and plans, with held-out controls and human adoption. Reliability for ML
or finance research has not been established; see the
[research requirements](RESEARCH-REQUIREMENTS.md).

## Tests

The offline suite makes no provider calls. Workflow tests use fake writer, review
and judge replies and a fake bridge, so they run without a council checkout. Set `RINGO_COUNCIL_ROOT` to also run a smoke test
against the real bridge with deterministic judge replies:

```sh
python3 -m unittest discover -s tests -q
RINGO_COUNCIL_ROOT=/ABSOLUTE/PATH/TO/design-council python3 -m unittest tests.test_workflow -q
```

Passing tests show that the plumbing and gates behave as written. They are not
evidence of model judgment quality or comparative quality.

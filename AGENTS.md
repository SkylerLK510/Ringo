# Ringo development

Ringo owns model execution, accounting, bounded artifact work, evaluation and
improvement. The separate Design Council owns scoring and decision gates.

Use `skills/ringo-harness/SKILL.md` for substantive coding/design work. Run the
actual harness for the final code review when inference is authorized; retain
reports outside this checkout and report the command and call count. Calls made
inside the harness must not recursively launch another harness.

Ringo has no call cap: a task makes the calls its plan needs (one per pass or
checker batch, at most `ceil(readers x 50 / batch)` batches, one retry each, one checker repair) or, for `work`, at most
`max_rounds x (1 + samples)`. Keep every new call path bounded by that structure
or by a cost or time limit, and pin the exact count in a test.

Use Python 3.11+ and the standard library for the runtime. Run
`python3 -m unittest discover -s tests -q` after implementation changes. Set
`RINGO_COUNCIL_ROOT` to a council checkout to enable the real offline bridge smoke.
Protected checks, evaluation rules and sealed data are outside improvement scope.

Record a measured improvement only in `docs/improvements/log.json`, with its raw
evidence beside it (redacted of private paths and message text), then run
`python3 improvement_log.py --write`. Never type a result into the log page or the
entry prose: the numbers are recomputed from the evidence, and an unsupported
claim fails `tests/test_improvement_log.py`. Measured non-improvements stay out of
the log and in their own reports.

Keep credentials, private research, comparison results and run traces outside this
repository; published improvement evidence is the exception above. Preserve human review of proposed artifacts and policy adoption.

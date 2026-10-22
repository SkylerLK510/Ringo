# Ringo development

Ringo owns model execution, accounting, bounded artifact work, evaluation and
improvement. The separate Design Council owns scoring and decision gates.

Use `skills/ringo-harness/SKILL.md` for substantive coding/design work. Run the
actual harness for the final code review when inference is authorized; retain
reports outside this checkout and report the command and call count. Calls made
inside the harness must not recursively launch another harness.

Use Python 3.11+ and the standard library for the runtime. Run
`python3 -m unittest discover -s tests -q` after implementation changes. Set
`RINGO_COUNCIL_ROOT` to a council checkout to enable the real offline bridge smoke.
Protected checks, evaluation rules and sealed data are outside improvement scope.

Keep credentials, private research, comparison results and run traces outside this
repository. Preserve human review of proposed artifacts and policy adoption.

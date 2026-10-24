# ML and finance research requirements

Recorded 2026-10-21. Intended use: intensive machine-learning and finance research,
with very high accuracy, efficiency and strong coding principles. The initial
finance specialization remains to be selected.

This document defines a target, not implemented capabilities or a quality claim.
Ringo currently runs review/design pipelines, searches review prompts/plans, and
runs bounded council-guided artifact work (`ringo.py work`, described in the
[council workflow guide](COUNCIL-WORKFLOW.md)). It has not demonstrated
end-to-end scientific or financial research reliability.

## Selected direction

Use the project's own Design Council as the foundation for structured assessment
of research plans and architecture. Ringo should provide bounded execution,
evidence collection and improvement around that assessment. External skill
frameworks and agent runtimes are not selected as the foundation.

Keep the council's rubric scoring and decision gates in its separate repository.
Its judgments assess the supplied evidence; numerical calculations, source claims
and experimental outcomes still require independent checks. Distinguish proposing
a method, executing it and evaluating its results.

Basic integration is implemented. `work` has a writer model return complete text
files or exact edits that Ringo applies to complete files, runs fixed executable checks on them, sends the fixed source material,
files, rationale and check results to a council rubric through an offline bridge,
and revises from the feedback within its round count and shared cost and time bounds. A
candidate reaches `final/` only after every check passes and the council
recommends it; a person decides what to adopt. Ringo makes the model calls and
owns their budget; the council scores the replies and owns the decision gate.
The research execution loop described below remains future work: inspecting and
editing an existing workspace, running experiments with data provenance, and
enforceable workspace permissions.

Evaluate this direction on fresh domain tasks before making comparative quality
claims. Prefer a single focused assessment by default; add independent review only
where its measured value justifies the additional inference cost.

## Priorities

Correctness and research validity are acceptance conditions. Among workflows that
meet those conditions, measure elapsed time, inference usage, compute cost and
repeated work. Short answers, fewer lines and model agreement are insufficient
evidence of quality. A well-supported negative or inconclusive result can be a
successful research outcome.

## Required evidence

Each substantive conclusion should link to its source or executable experiment.
Separate reported findings, assumptions, calculations and new hypotheses. Record
the source version and retrieval date where they affect interpretation. Verify
that references exist and support the specific claim; an attached citation alone
is not verification. Expose missing evidence and conflicting results.

Numerical results should have a reproducible command, code/configuration identity,
data provenance and snapshot identity, dependency versions, random seeds, hardware
details where relevant, and retained raw outputs. Record tolerances and known
nondeterminism rather than promising identical results on every machine.

## Domain validation

For ML: define the estimand or prediction target, metric and simple baseline;
separate training, tuning and final assessment; select splits that respect groups,
time and dependencies; prevent preprocessing and label leakage. Use ablations,
repeated trials and uncertainty estimates appropriate to the experiment. Record
unsuccessful runs as well as selected results.

Preprocessing learned from held-out data can invalidate evaluation. See
[scikit-learn's leakage examples](https://scikit-learn.org/stable/common_pitfalls.html#data-leakage)
and [cross-validation guidance](https://scikit-learn.org/stable/modules/cross_validation.html).

For finance: make units, currencies, valuation dates, data availability dates,
revision policy and modeling assumptions explicit. Choose validation appropriate
to the research question. If studying trading strategies, check look-ahead and
survivorship effects, chronological execution, relevant transaction/financing
costs and liquidity assumptions. Record the full strategy/model search history
and assess selection effects rather than reporting only the best backtest.
Repeated searches can manufacture apparently strong results from noise; see
[Bailey et al., The Probability of Backtest Overfitting](https://www.davidhbailey.com/dhbpapers/backtest-prob.pdf).

## Coding and execution

Use small, explicit modules and established numerical libraries. The harness's
standard-library implementation preference does not prohibit scientific
dependencies in research projects. Keep pure calculations separate from data
access and expensive jobs; make shapes, dtypes, units and time conventions clear.
Test calculations against independent reference cases, invariants and boundary
conditions. Check NaNs, infinities, numerical stability, alignment and indexing.
Passing implementation tests does not prove the scientific hypothesis.

An execution workflow needs bounded inspect/edit/test/diagnose/repair steps,
retained failures, cancellation and enforceable workspace permissions. `work`
covers part of this for declared text files: bounded rounds, retained candidates
and check logs, and process-group cleanup of checks on POSIX. Its run folders are
not sandboxes, and checks run with the user's permissions and environment. Start with
small diagnostic experiments before full training or large data jobs. Reuse cached
artifacts only when relevant code, inputs, parameters and environment still match.
Measure inference, external data and CPU/GPU costs separately.

## Improvement and evaluation

Evaluate on domain tasks with independently specified acceptance criteria and
reference calculations, including deliberately invalid analyses and genuinely
clean controls. Add review of evidence and experimental design where executable
checks cannot settle validity. Agreement between models is supplementary evidence.

Keep two validation problems separate: whether a research model generalizes to
new data, and whether Ringo's workflow generalizes to new research tasks. A valid
data split inside one experiment does not establish the latter.

Future RRSI-inspired search should optimize validated research outcomes within
resource budgets. Keep evaluation rules and protected checks outside its edit
scope. Do not reward impressive returns, higher model scores or confident prose
without validating their basis. Preserve untouched task assessments and account
for repeated search. Extending beyond review prompts requires implementation and
offline verification before any live campaign. A `work` run repairs one task's artifacts;
it neither searches nor changes workflow policy.

Generic engineering skills may supply useful starting guidance. Their suitability
for this research workload must be measured; no external framework is adopted by
this requirements document.

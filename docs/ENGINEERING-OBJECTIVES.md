# Engineering optimization objectives

Recorded 2026-10-26 from the owner's direction: optimize the Ringo executable
harness for **coding ability, design judgment, and architectural insight**, while
preserving correctness and whole-task efficiency. These are requirements and a
proposed measurement plan, not measured improvements or an active campaign.

Ringo orchestrates frozen provider models; this objective concerns its context,
execution, verification and assessment workflow, not training model weights.

## Three capabilities to improve

| Capability | Observable success | Evidence to collect |
|---|---|---|
| Coding ability | Produces working fixes/features, diagnoses causes, preserves interfaces, and evolves existing code without needless complexity | Executable patch, independently fixed acceptance/regression/failure checks, later requirement change, human review |
| Design judgment ("intuition") | Chooses a coherent interaction or API suited to the user/task, prioritizes the right details, and handles important states with appropriate simplicity | Implemented artifact or API, task walkthrough, relevant rendered/keyboard/accessibility checks, change-of-requirement exercise, independent critique |
| Architectural insight | Finds the consequential boundaries, dependencies and bottlenecks; explains tradeoffs, failure behavior, migration and rollback | Committed decision with assumptions, load arithmetic when inputs exist, interface/integration evidence, fault scenarios, later constraint change |

Report these outcomes separately. A higher average cannot hide a failed hard
constraint or a material regression in one capability. Maintainability and
verification are part of coding success; design quality includes usability and
implementation; architecture quality must survive concrete consequences.

Do not manufacture traffic, hardware, deadlines or capacity numbers for an
underspecified brief. Identify missing inputs and make assumptions visible.
Prefer the simplest design that meets the supplied constraints; earn added
components with evidence of a need. Preserve useful alternatives and switch or
rollback triggers where a decision is uncertain.

## Task families for a future development pilot

The following are proposed task shapes, not a selected corpus or launched run.
Each needs a frozen brief, starting artifact, allowed edits/tools, independent
checks and a declared resource budget before execution.
Keep the later requirement out of the initial candidate's material and reveal
it at the same stage in every comparison arm.

| Family | Required artifact | Checks fixed independently of the candidate | Evolution challenge |
|---|---|---|---|
| Repository implementation | Patch to a real repository with a reproducible trigger or feature request | Behavior, compatibility, relevant boundary/failure cases; the candidate cannot replace acceptance tests | Add a realistic requirement that should reuse the implementation |
| Interaction/API design | Working interaction slice or callable API, with rationale for the chosen behavior | Complete core user task; empty/loading/error/cancelled states as relevant; render/keyboard checks for UI or contract examples for API | Change a user need without rebuilding the entire design |
| System architecture | Decision record plus the minimum prototype/contracts needed to test its riskiest claim | Capacity/failure calculations from supplied inputs; fault/integration evidence; migration/rollback walkthrough | Change a load, dependency or reliability constraint and assess adaptation cost |

Examples could include a cancellation-safe completion worker, its suggestion
interaction, and its crash/restart boundaries. Ringo-IDE is a useful development
case, not sufficient evidence of generalization. Use unrelated repositories and
different task authors for later untouched assessment. Protect hidden checks and
answer keys from candidate writers; declaring a folder private is not isolation.

Distinguish failure types: incorrect behavior, incomplete task, poor fit to user
needs, unsupported architecture assumptions, unnecessary complexity and lost
verification. Retain failed and inconclusive attempts and their resource usage.
After an evolved requirement, examine regressions, changed interfaces, duplicated
logic and the actual work needed, not just the elegance of the initial rationale.

## Evidence and the separate Design Council

Ringo owns model execution, checks, traces and accounting. The separate Council
owns rubric scoring and decision gates. Its existing starting rubrics are
`code_change.json`, `system_design.json` and `artifact_design.json`; choose one
that fits the actual deliverable and supply the relevant implementation/checks.
Use `report.json` for a report rather than treating prose as a working design.

Executable checks establish only the behavior they test. Council assessments
address the supplied evidence and need calibration against independently observed
outcomes and human judgment. Model agreement or a rising rubric score alone
does not establish improvement. Keep protected checks/rubrics outside candidate
edits, preserve human adoption, and record disagreements and uncertainty.

## Compare quality and efficiency together

For a later comparison, preregister the same task/model versions, tool access,
starting state, acceptance criteria and resource envelope for:

- The same agent working alone.
- The current approved Ringo workflow.
- One bounded candidate workflow change.

Separate the effect of adding Ringo from the effect of changing Ringo. Match
quality criteria and task scope before comparing resources. Randomize or balance
run order and repeat enough to support the declared uncertainty method; choose
sample counts and thresholds before final assessment, not after observing gains.

Report completion and hard-constraint pass rates per capability; regressions;
independent quality observations and Council assessments; elapsed time; and
whole-task tokens including the outer agent, Ringo/Council calls, retries,
failures, corrections and setup overhead. Missing usage is unknown, not zero.
Include any model-based scoring of the standalone arm in the same accounting
boundary; free assessment overhead would make the comparison misleading.
Keep failures in the denominator and report their resource use. Token savings
are useful only while required quality and completion hold. See
[token accounting](TOKEN-USAGE.md).

## Current limits and implementation sequence

The protected [evaluator](../evaluate.py) accepts review tasks only. Existing
guarded improvement searches review prompts/plans. [HarBenchmark](../HarBenchmark.md)
has narrow repair-expression and design-arithmetic development fixtures; its
scores do not establish repository implementation, interaction design or
architecture quality. Do not relabel those results as coverage of this objective.

1. Specify candidate-independent task contracts and evidence fields for all three
   capabilities, using the existing executable checks/Council boundary.
2. Implement and offline-test any missing runners/adapters in an isolated change;
   demonstrate that invalid, incomplete and falsely claimed outcomes are rejected.
3. Select a development pilot and declare its allowed edits, budget, stopping
   conditions and comparison protocol. Keep protected evaluation untouched.
4. Only after validated support exists, preregister an appropriate held-out
   evaluation. Do not force non-review tasks into the review-only evaluator or
   weaken its existing gates to accommodate them.
5. Consider one workflow change at a time. Adopt only with the required sealed
   evidence, no material per-capability regression, and an explicit human decision.

Stop or withhold adoption on failed acceptance checks, missing evidence, protected
data exposure, exhausted budget, unresolved quality regressions or inconclusive
comparisons. No training, prompt search, evaluation/improvement campaign, runtime
activation or expansion of candidate permissions is authorized by this document.

Before a live pilot, still choose the task corpus, languages/domains, allowed
candidate changes, per-capability acceptance/regression thresholds, sample size,
uncertainty method and finite token/time/compute limits. Existing protected
monetary-budget requirements remain in force where those tools require them;
this objective invents no monetary cap. Ordinary task reviews/design proposals
remain distinct from a campaign. There is no new measured gain to report.

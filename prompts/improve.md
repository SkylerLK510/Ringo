You improve a code-review pipeline. The material holds the incumbent pipeline (its plan and prompt files), the log
of earlier proposals in this campaign with why each was rejected, and evidence from development cases only: what
the incumbent's reviews found, which known defects they missed (category, difficulty, description), and which of
their findings a grader judged wrong.

Propose exactly one change that you expect to help on cases you have not seen:
- edit one prompt file, or change the plan (add, remove or reorder a pass, change a pass's inputs, batch size or
  model); a plan edit may introduce the prompts needed by its new passes, including a named library prompt;
- write general rules only: never mention a case, a repository, a file path, a known defect, or anything that only
  makes sense for these cases;
- cost counts: an extra call or a longer prompt must buy more defects found or fewer wrong findings;
- do not repeat a rejected hypothesis unless you say what is different this time.

Reply with one JSON object and nothing else:
{"hypothesis": "what you expect to improve and why, in one or two sentences",
 "plan": {"...": "the whole new plan; omit this key if the plan is unchanged"},
 "prompts": {"<file>.md": "the whole new text of the one prompt you changed; omit this key if none"}}

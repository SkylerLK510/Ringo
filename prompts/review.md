Review the change in the material as the engineer who will be paged when it breaks.

Report the real defects it introduces: wrong results, crashes, security holes, lost data or work, a mechanism that
fails at what the code, its docs or the change description say it is for, a message or document that tells the user
something false, and failures at the load the project expects. Read beyond the diff: callers, readers, twins of what
changed, and every promise the docs and comments make about it.

Show each defect, or leave it out:
- a concrete input or situation the declared types and docs allow, edge values included (empty, duplicate,
  unhashable, unorderable, very large); any caller, subclass or configuration the public API allows counts, shown
  or not;
- what happens, and what should happen;
- the exact line(s) it rests on, quoted.

Before you report a finding, re-read those lines for anything in the code shown that already prevents it. A claim
that depends on code you cannot see is a question, not a finding. One mechanism per finding; no unchecked side
claims. No style comments.

Format, most severe first:

What this change does: one sentence.

### 1. Short title (`file:line`)
- Code: the exact line(s), quoted.
- Problem: the input or situation, what happens, what should happen.
- Fix: the smallest fix.

### 2. ...

### Open questions
- What the code shown cannot settle.

### Not checked
- What you did not check.

If you find no defects, write the line "No defects found." after the summary instead of findings.

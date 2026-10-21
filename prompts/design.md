Write a design proposal that answers the decision brief in the material.

Work it out before you write:
1. List each hard constraint and the measurable condition that shows it is met. Mark what the brief states as
   known and what you assume.
2. Pull every number from the brief (today's load, the load it says is coming, sizes, rates, limits, deadlines)
   and compute demand against capacity, with units, for every shared resource the design would use (a disk, a
   link, a lock, a single process, a queue, a pool, a rate limit), today and at the stated future load. Name the
   first bottleneck and the load at which it breaks a constraint.
3. Sketch two or three candidates (the smallest that works today, one sized for the stated load, the easiest to
   undo), check each against every constraint at the stated future load, drop the ones that fail unless they state
   their ceiling and a switch trigger well before the growth arrives, and choose the simplest of the rest.

Then write 250 to 450 words of plain prose, no headings (a few bold labels are fine):
- the decision, committed, and what in it meets each hard constraint;
- the load arithmetic and the design's ceiling;
- the failure cases and what the system does in each;
- pass/fail checks, including one at the stated future load and one failure test;
- known versus assumed, and what would change the decision.

Before you finish, re-read the brief and make sure every constraint is met by a sentence in the proposal and every
number comes from the brief or is marked as assumed. Do not sign it or say who or what you are. Reply with the
proposal only.

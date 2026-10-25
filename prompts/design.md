Write a design proposal that answers the decision brief in the material.

Work it out before you write:
0. Copy every item from the brief's Deliverable section and every hard constraint into a checklist, splitting
   lists (e.g. "data, models, metric, threshold, kill criterion") into separate items. If the brief names options
   and criteria to compare them on, add one item per option and criterion.
1. List each hard constraint and the measurable condition that shows it is met. Mark what the brief states as
   known and what you assume.
2. If the brief gives loads, sizes, rates, capacities or deadlines, pull every number (today's load, the load it
   says is coming, sizes, rates, limits, deadlines) and compute demand against capacity, with units, for every
   shared resource the design would use (a disk, a link, a lock, a single process, a queue, a pool, a rate limit),
   today and at the stated future load. Name the first bottleneck and the load at which it breaks a constraint.
   If it gives no loads, rates or capacities, say so in one sentence and skip that arithmetic; resource limits
   that bear on a constraint (hardware, time, quota) still get computed.
3. If the brief names options, compare every named option on every listed criterion, and add any stronger option
   you find or say you found none. Otherwise sketch two or three candidates (the smallest that works today, one
   sized for the stated load, the easiest to undo), check each against every constraint at the stated future
   load, drop the ones that fail unless they state their ceiling and a switch trigger well before the growth
   arrives, and choose the simplest of the rest.

Then write the proposal. A brief with one decision gets 250 to 450 words of plain prose (a few bold labels are
fine). A brief that lists deliverables, or options to compare on criteria, gets as many words as it takes to
answer every item, with one short heading per deliverable and a table when options are compared on criteria.
Never drop a required item to save words, and do not pad. Cover:
- the decision, committed, and what in it meets each hard constraint;
- where the brief gives loads, the load arithmetic and the design's ceiling;
- the failure cases and what the system does in each;
- pass/fail checks, including one failure test and, where the brief gives a future load, one at that load;
- known versus assumed, and what would change the decision.

Before you finish, re-read the brief and your checklist. Make sure every hard constraint, every deliverable item,
and every named option on every named criterion is answered in the body, and every number comes from the brief or
is marked as assumed. Do not sign it or say who or what you are. Reply with the proposal only.

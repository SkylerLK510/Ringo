Write a Python unittest module that checks an implementation of the brief. You see the brief and the current
files only to learn the public interface (names, signatures, modules). The implementation will change, so do not
test its internals.

Test only behaviour that the brief states. Put the brief sentence each test checks in its docstring, quoted.
Concentrate on the rules a careful implementer is most likely to miss: boundaries, error cases with their stated
types and messages, ordering and tie rules, precision and rounding, encoding, round trips, and interactions
between two rules. Write one rule per test, with 25 to 45 tests in total. Tests must be independent,
deterministic and fast. Use no network, and write files only under tempfile directories. Import the code under
test at module level, by the import path the brief and files give.

Reply with only the module, in one ```python fenced block.

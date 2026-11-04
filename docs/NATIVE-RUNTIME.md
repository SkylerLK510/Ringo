# The native runtime, and why it moved to Rust

`ringo review`, `ringo design` and `ringo work` now run inside the `ringo` binary. The runtime in `cli/src/rt/` is
ported module by module from the Python files of the same names (`context`, `pipelines`, `findings`, `backends`,
`harness`, `workflow`). The Python runtime stays, unchanged, as the reference: `python3 ringo.py ...` works as before.

**Speed was not the reason.** Python's own cost is about 70 ms per run, against 18-56 s of model time, so 0.1-0.4%.
The one large overhead the profile found was about 1 s per Codex call: a network fetch of Codex's model list. That
was fixed in Python, by giving each call's private Codex home the owner's cached list (design overhead fell from
806 ms to 72 ms).

**The reasons:**
- **One engine.** The shell, the planned IDE and its local code model all need to start, follow and stop pipelines
  and agents. The shell had re-implemented in Rust what `backends.py` does in Python: environment scrubbing,
  process groups and event parsing. Now they share one library.
- **Less per job.** We measured a review with a fake model that answers in 0.30 s, median of 7 runs:
  - native: 8.1 MB peak memory, 0.34 s wall time;
  - Python: 24.5 MB, 0.39 s.

  The shell runs up to three pipelines at once and idles at 2.5 MB.
- **One binary to install.** `review` and `design` need no Python, with one exception: reviewing a change to `.py`
  files runs `python3` once to read their symbols (see "What stayed Python"). `work` runs the task's own checks,
  which need Python when the task does.

**The published results still describe it, because it sends the same requests.** The benchmark numbers in the
[README](../README.md) were measured on the Python runtime. The port counts only where it sends the model the same bytes:
- **Module by module.** Each module was ported against its Python original with differential tests on generated,
  corpus and git-history inputs: over a million cases in all, most of them generated JSON inputs. No mismatch was
  accepted. The deviations left (a JSON nesting limit near 1,000 levels instead of about 116,000, 128-bit integers)
  are listed in the test files.
- **End to end.** `tests/native_gate.py` runs 16 review, design and work tasks through both runtimes, with fake
  `codex`, `claude` and plain programs. It compares every request and every record each run writes. A request here
  means the argv, stdin, the system prompt, the private Codex home, and the environment variables that decide
  auth. The run uses no model calls; today all 16 tasks are identical.
- **Real use.** Through the binary with real Codex calls, the same commands worked:
  - a design (1 call);
  - a review of a commit (1 call);
  - the slugify `work` example (2 calls, ready for review in round 1, passing the example's checks);
  - `/design` from the shell (1 call).

**The release gate still decides what runs.**
- A release can carry the native binary only with a passing gate report for that exact binary and that exact Python
  runtime (`ringo.py release stage --native BIN --native-gate REPORT`).
- Activation still needs the latency evidence and your approval.
- The binary checks the pinned release itself before running it: the bundle, its hashes, the active pointer and
  the approval record. `cli/tests/gate_release.rs` stages a real native release and checks the genuine chain and
  seven forgeries.
- With no release state, the binary runs as checkout code, as `ringo.py` runs its own checkout.
- With release state, `ringo.py` decides which runtime runs.
- Windows keeps the Python runtime.

**What stayed Python:**
- **Python's `ast` parser.** rustpython-parser disagreed with CPython in 90 of 11,442 symbol cases, and
  tree-sitter-python in 24, on the standard library and this repository. So the native runtime asks `python3` for
  Python symbols, with an embedded copy of the same function, which disagreed in none.
- **Research tooling and release management.** `evaluate.py`, `improve.py`, `evalplus_study.py`, `shadow_labels.py`
  and `releases.py` stay Python.

**Bugs the port found and fixed, in both runtimes:**
- The `claude` preset now removes API keys and endpoint overrides, so its calls stay on your subscription login
  (`claude:<command>` keeps your environment).
- A refreshed Codex login is copied back only from a regular file: never through a symlink, never from a FIFO.
- A review of a submodule bump to a commit the repository lacks no longer fails.
- Trace folders are private from the moment they are created.
- Codex's setup call is killed with everything it started.

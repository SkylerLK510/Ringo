# Contributing to Ringo

[Project home](README.md) · [Documentation](docs/README.md) · [Roadmap](ROADMAP.md)

Bug reports, documentation improvements, small fixes and carefully measured experiments
are welcome. Start with the problem a person encounters and the behavior you want to improve.

## Report a problem or propose an idea

Use the [issue chooser](https://github.com/SkylerLK510/Ringo/issues/new/choose).
For a bug, include reproduction steps, expected/actual behavior, operating system, terminal,
Ringo commit/version and agent CLI version. For a feature, explain the workflow and how
you would judge the result. A larger architectural change benefits from an issue before a PR.

Redact credentials, personal paths, prompts and private code from logs and screenshots.
If a report contains a security-sensitive detail, use the repository's private vulnerability
reporting option if available; otherwise describe the category without publishing an exploit
or secret and ask for a private reporting route.

## Repository map

| Location | Responsibility |
| --- | --- |
| `cli/src/` | Rust terminal UI, agent sessions and native pipeline runtime |
| `cli/tests/` | Engine, shell, runtime parity and scenario integration tests |
| `ringo.py`, `harness.py`, `workflow.py`, `backends.py` | Python entrypoint, orchestration, file work and model execution |
| `research.py`, `context.py`, `findings.py` | Source verification, review context and findings output |
| `evaluate.py`, `improve.py`, `releases.py` | Evaluation, improvement search and release gates |
| `turnrun*.py`, `cli/src/turn_policy.rs` | Experimental turn measurement, isolation and policy campaigns |
| `plans/`, `prompts/` | Pipeline configuration and instructions |
| `examples/`, `tests/` | Starter task specs and Python tests |
| `docs/` | User documentation, technical references and experiment records |
| `adapters/` | External benchmark integration |

Root-level Python modules are part of the runtime layout. Keep structural refactors
separate from presentation cleanup so imports, release inventories and entrypoints can
be reviewed and tested together.

## Work on a change

Use a branch from current `main` and keep changes focused. The Python runtime requires
Python 3.11+ and uses only the standard library. The Rust crate uses the 2024 edition;
build it with a current stable toolchain. Runtime instructions for coding agents live in
[AGENTS.md](AGENTS.md).

Run the checks that cover the change from the repository root:

```sh
# Python runtime changes: offline suite, no provider inference
python3 -m unittest discover -s tests -q

# Rust changes: engine, shell and parity/scenario gates
cargo test --manifest-path cli/Cargo.toml --locked --all-features --no-fail-fast
```

The Rust parity gates compare the Rust runtime with Python's own behaviour, which changes between patch releases
(for example, Python 3.14.4's json scanner and argparse error texts differ from 3.14.7 and later). They run the
interpreter named by `RINGO_PYTHON` (an absolute path to one program), else `python3` (`python` on Windows); CI's
Rust jobs use the latest Python 3.14. If your `python3` is
older, point `RINGO_PYTHON` at a current 3.14 (for example one installed with `uv python install 3.14`) for the cargo
run.

CI runs Python on macOS, Linux and Windows with Python 3.11/3.14; Rust on all three
platforms; and a separate Linux containment job. Docker-dependent tests can skip when
their pinned images are absent. Native replay and scenario coverage also need the
corresponding images and binary; see [live-pilot offline acceptance](docs/LIVE-RRSI-PILOT.md#offline-acceptance).
A local pass with skips does not replace those CI checks.

For documentation-only changes, check relative links, heading anchors, code examples
and the rendered Markdown. Runtime suites are appropriate when behavior or executable
examples change. For terminal UI changes, also inspect the running shell and record
the terminal, interactions and visual result.

## Keep evidence trustworthy

- Keep credentials, private tasks, hidden tests and raw run traces outside the checkout.
- Live evaluation and improvement campaigns need declared scope and numeric budgets.
  Running ordinary offline tests does not authorize inference.
- Preserve protected checks, held-out splits, release integrity and human adoption gates.
- Record measured improvements in `docs/improvements/log.json` with redacted, pinned
  evidence, then run `python3 improvement_log.py --write`. The generated log must agree
  with that evidence. Put non-improvements in their own experiment reports.

## Open a pull request

Explain the problem, resulting behavior and validation. Include a concrete before/after
example when it helps. State skipped checks or platform limits plainly. For measured
performance claims, include the comparison, task scope and complete accounting.

Keep private run logs outside the repository. Link public evidence or attach redacted
screenshots as appropriate. Let CI finish and resolve findings before merge.

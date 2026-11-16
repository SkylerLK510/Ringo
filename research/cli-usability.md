# Research: Custom CLI Usability, Readability, and Ergonomics

*Recorded November 2026 for the Ringo project and command-line harness architectures.*

See also: [`docs/RESEARCH-CLI-USABILITY.md`](../docs/RESEARCH-CLI-USABILITY.md)

---

## 1. Executive Summary & The Modern CLI Paradigm

Command-line interfaces (CLIs) have undergone a significant design renaissance over the past decade. Historically influenced by 1970s UNIX constraints—where telegraphic terseness, silent execution, and arcane single-letter flags were necessary optimizations for slow teletype terminals—modern CLIs are evaluated against high **Developer Experience (DevEx)** standards established by tools like `git`, GitHub CLI (`gh`), Docker, Stripe CLI, and the *Command Line Interface Guidelines* ([clig.dev](https://clig.dev/)).

Unlike Graphical User Interfaces (GUIs), which provide visible visual affordances (buttons, menus, breadcrumbs), a CLI has only text, stream discipline, and layout structure to guide the user. Consequently, **every character, space, color code, and error message in a CLI is user-interface design**.

### The Dual-Persona Imperative
Every professional CLI serves two fundamentally divergent user personas:
1. **The Interactive Human:** Needs discoverability, visual hierarchy, progressive disclosure, informative feedback, and forgiving error recovery.
2. **The Automated Pipeline (Script / CI / Agent):** Needs deterministic exit codes, parseable machine-readable data streams, zero interactive stalls, and silent efficiency.

High usability and readability are achieved by honoring both personas through strict stream separation, automatic terminal detection (`TTY`), and consistent command architecture.

---

## 2. Command Architecture & Grammar (Information Architecture)

Usability begins with cognitive predictability. A user who understands one command in your CLI should instinctively guess how other commands behave.

### 2.1 Command Hierarchies: The Noun-Verb Pattern
For non-trivial CLIs with multiple domains (such as Ringo's reviews, designs, work loops, evaluations, and campaigns), adopt the structured **`tool [noun/topic] [verb/action]`** grammar over sprawling ad-hoc flags or arbitrary verbs.

* **Pattern:** `ringo <resource> <action> [arguments] [flags]`
* **Examples from industry standards:**
  * `git remote add <name> <url>` (Noun: `remote`, Verb: `add`)
  * `docker container ls` (Noun: `container`, Verb: `ls`)
  * `gh pr checkout <number>` (Noun: `pr`, Verb: `checkout`)
* **When to use Single-Verb:** For primary workflows that represent the core identity of the tool, top-level verbs (`ringo review`, `ringo design`, `ringo work`) provide speed and brevity. When commands scale past 4–5 actions, sub-grouping under nouns (`ringo eval run`, `ringo eval split`, `ringo eval grade`) prevents cognitive overload.

### 2.2 Arguments vs. Options (Flags)
Confusing positional arguments with options creates fragile CLIs. Follow these strict boundaries:

| Entity | Role | When to Use | Example |
|---|---|---|---|
| **Positional Arguments** | The primary target or subject of the command | When the parameter is essential, unambiguous, and there are 1 (at most 2) targets. | `ringo review change.txt` |
| **Named Options (`--flags`)** | Modifiers, configurations, filters, or overrides | For all optional values, tunables, secondary inputs, and ambiguous types. | `--backend codex-chatgpt --max-calls 1` |

*Guideline:* Never use more than two positional arguments. If a command requires three or more inputs, use named options with descriptive keys.

### 2.3 Flag Naming Standards
Adhere to POSIX and GNU conventions:
* **Kebab-Case:** Use lowercase words separated by hyphens (e.g., `--max-cost`, `--dry-run`, `--unpriced-call-usd`). Avoid `camelCase`, `snake_case`, or joined words.
* **Short Option Parity:** Provide single-character shortcuts for high-frequency flags (`-h` for `--help`, `-v` for `--version`, `-o` for `--out`, `-q` for `--quiet`, `-f` for `--force`, `-y` for `--yes`).
* **Symmetry and Negation:** For boolean flags that default to `True`, provide an explicit negation flag rather than requiring an argument:
  * `--color` vs `--no-color`
  * `--confirm` vs `--no-confirm`
  * `--execution` vs `--no-execution`

---

## 3. Readability & Microcopy Design

Terminal typography is constrained to monospaced grids. Readability depends on structured layout, visual contrast, and actionable microcopy.

### 3.1 Visual Hierarchy & Chunking
Dense walls of text cause cognitive fatigue and cause users to miss critical warnings.
* **Section Chunking:** Separate logical phases with single empty blank lines and bolded section headers (e.g., `==> Phase 1: Preparation`).
* **Visual Indentation:** Indent child steps, log excerpts, or secondary items by 2 or 4 spaces to visually subordinate them to their parent task.
* **Key-Value Alignment:** In status summaries and reports, align colons or values into vertical columns for quick vertical eye scanning:
  ```text
  Status:      ready_for_review
  Rounds:      2 / 3
  Total Calls: 5 (writer: 2, judge: 3)
  Cost:        $0.042 USD
  Output:      /path/to/run/final/
  ```

### 3.2 Color, Contrast, and Accessibility
Color enhances readability when applied semantically, but degrades it when overused.

#### Semantic Palette Standard:
* **Green:** Successful completions, additions, passing checks (`✔`, `[PASS]`).
* **Red:** Errors, fatal halts, rejected candidates (`✖`, `[FAIL]`).
* **Yellow / Amber:** Non-fatal warnings, spending limits approaching, pending gates (`⚠`, `[WARN]`).
* **Cyan / Blue:** Informational highlights, URLs, resource identifiers.
* **Dim / Gray (`\x1b[2m`):** Secondary metadata, timestamps, hashes, debug notes, guidance hints.

#### Accessibility Rules:
1. **Never convey state through color alone.** Always pair color with text markers or Unicode glyphs (`✔ Passed`, `✖ Failed`, `⚠ Warning`). This ensures full comprehension for color-blind users and monochrome terminals.
2. **Respect the `NO_COLOR` Standard:** Check for the presence of `os.environ.get("NO_COLOR")`. If set (regardless of value), immediately suppress all ANSI escape styling (see [no-color.org](https://no-color.org/)).
3. **Check `TERM=dumb` and TTY:** If `TERM` is `dumb` or the standard output stream is not attached to a terminal (`not sys.stdout.isatty()`), disable color codes and dynamic cursor controls.

### 3.3 Dynamic Terminal Width & Text Wrapping
* **Width Awareness:** Query the live terminal size (`shutil.get_terminal_size((80, 24)).columns`).
* **Never Overflow Columns:** Avoid lines hard-clipping past the terminal width, which produces jagged, broken lines. Wrap running text cleanly with standard word wrapping (`textwrap.fill`).
* **Middle-Truncation for Paths and Hashes:** When paths or cryptographic hashes must fit in a constrained column or table, truncate them in the middle rather than clipping the end:
  * *Bad:* `/Users/skylerkendrick/project/very/long/nested/...` (omits the actual file name)
  * *Good:* `/Users/.../nested/stats.py` or `a1b2c3...f9e0`

### 3.4 Microcopy: The 3-Part Error Pattern
Poor CLI error: `Error: failed at step 2 (code 1)`.
Usable CLI error follows the **What, Why, Fix** structure:
```text
Error: Spec validation failed for task.json.
Reason: Field 'writer' specifies unknown backend 'codex-preview'.
Suggestion: Use one of the supported presets: 'claude', 'codex', 'codex-chatgpt', or 'ollama:MODEL'.
```

---

## 4. Usability & Interaction Ergonomics

### 4.1 Discoverability & Progressive Disclosure
Users do not read external man pages before trying a CLI; they explore from within the terminal.

* **Top-Level `--help` Overview:** The root help must present the tool's purpose, the primary commands grouped by category, and 2–3 copy-pasteable quickstart examples.
* **Subcommand Help (`tool <command> --help`):** Each subcommand must document:
  1. One-line purpose description.
  2. Synopsis showing required vs optional arguments using standard notation:
     - `<required_arg>` (angle brackets)
     - `[optional_flag]` (square brackets)
     - `choice_a | choice_b` (pipe for alternatives)
  3. Table of options with default values displayed.
  4. At least one realistic example invocation.
* **Fuzzy Command Matching (Levenshtein Distance):** If a user mistypes `ringo revirew`, the CLI should not simply print a usage error. It should suggest:
  `ringo: 'revirew' is not a ringo command. Did you mean 'review'?`

### 4.2 Safe Defaults & Guarded Actions
* **Non-Destructive by Default:** Operations that write files, spend money, or alter state should dry-run or target safe, isolated locations unless explicitly instructed.
* **Confirmation Prompts with Non-Interactive Overrides:** For irreversible operations (e.g., clearing an evaluation ledger, retiring test cases):
  * In interactive TTY: Prompt `Are you sure? [y/N]: ` (defaulting to No on Enter).
  * In non-interactive pipes: Fail immediately unless `--yes` / `-y` / `--force` was passed, preventing hung CI runs.

### 4.3 Real-Time Feedback & Progress Indicators
A command running for more than 1 second without output causes users to assume the program has frozen.

* **Spinners vs. Progress Bars:**
  * **Spinners (indeterminate):** Use for operations where total duration cannot be predicted (e.g., waiting for backend LLM inference, remote network roundtrips).
  * **Progress Bars (determinate):** Use when total items or steps are known (e.g., evaluating 40 benchmark cases: `[████████░░] 80% (32/40) • ETA: 14s`).
* **In-Place Updates (`\r`):** Update the status line in-place using carriage returns while active.
* **Static Resolution Line:** Once an operation finishes, replace the dynamic spinner with a clean, permanent static log line:
  ```text
  ✔ Completed review across 3 passes (2.4s, $0.012 USD)
  ```
* **Graceful Signal Handling (`Ctrl+C`):** Intercepting `SIGINT` should immediately clear active spinners, restore cursor visibility (`\x1b[?25h`), flush state/traces, and exit cleanly with code 130 without vomiting a multi-line Python traceback.

---

## 5. Stream Separation & The Unix Philosophy

To be both a comfortable interactive tool and an automated pipeline component, the CLI must adhere strictly to standard stream contracts.

### 5.1 Standard Streams Contract
```
+--------------------------------------------------------+
|                      CLI PROCESS                       |
+---------------------------+----------------------------+
                            |
           stdout           |           stderr
      (Primary Payload)     |     (Diagnostics & UX)
                            |
   - Command result text    |   - Spinners & progress bars
   - Machine JSON / YAML    |   - Informational status logs
   - Tables requested       |   - Warning banners
   - Generated files/code   |   - Error explanations
                            |
                            v
                    Piped downstream or
                 inspected by human operator
```

* **Data to `stdout`:** The actual answer, synthesized proposal, review findings, or `--json` payload belongs on standard output so users can pipe it into `jq`, `grep`, or file redirection (`ringo review ... > report.md`).
* **Noise to `stderr`:** Progress meters, step announcements, token counters, and status updates belong on standard error. This prevents diagnostic messages from corrupting downstream data parsers.

### 5.2 Structured Machine Output
Every diagnostic, evaluation, and review command should support `--json` (or `--ndjson`). When `--json` is selected:
1. Emit pure, valid JSON to `stdout`.
2. Omit all decorative ANSI styling from `stdout`.
3. Route any human warnings or status messages to `stderr`.

### 5.3 Standard Exit Codes

| Exit Code | Semantic Meaning | Usage in Ringo |
|---|---|---|
| `0` | Success / Clean Pass | Checks passed, council recommended, eval passed. |
| `1` | Operational Failure / Rejection | Checks failed, council rejected, budget exhausted, inconclusive. |
| `2` | Command-Line Usage / Syntax Error | Invalid flag, missing argument, bad spec syntax. |
| `130` | Terminated by `SIGINT` | User pressed `Ctrl+C`. |

---

## 6. Python Standard-Library Implementation Patterns

Ringo's harness maintains a strict, zero-external-dependency standard-library philosophy. High usability, color styling, and terminal formatting do not require heavy third-party packages (like `click` or `rich`). They can be cleanly achieved with standard library modules (`argparse`, `shutil`, `textwrap`, `sys`, `os`).

### 6.1 Minimalist ANSI Color & Styling Helper
```python
import os
import sys

class Term:
    """Zero-dependency ANSI styling with NO_COLOR and TTY detection."""
    _enabled = (
        sys.stderr.isatty()
        and "NO_COLOR" not in os.environ
        and os.environ.get("TERM") != "dumb"
    )

    RESET = "\033[0m" if _enabled else ""
    BOLD = "\033[1m" if _enabled else ""
    DIM = "\033[2m" if _enabled else ""
    GREEN = "\033[32m" if _enabled else ""
    RED = "\033[31m" if _enabled else ""
    YELLOW = "\033[33m" if _enabled else ""
    CYAN = "\033[36m" if _enabled else ""

    @classmethod
    def success(cls, text: str) -> str:
        return f"{cls.GREEN}✔ {text}{cls.RESET}"

    @classmethod
    def error(cls, text: str) -> str:
        return f"{cls.RED}✖ {text}{cls.RESET}"

    @classmethod
    def warn(cls, text: str) -> str:
        return f"{cls.YELLOW}⚠ {text}{cls.RESET}"
```

### 6.2 Responsive Key-Value Output
```python
import shutil

def format_summary(title: str, pairs: list[tuple[str, str]]) -> str:
    """Format an aligned key-value summary box bounded by terminal width."""
    width = min(shutil.get_terminal_size((80, 24)).columns, 90)
    out = [f"{Term.BOLD}{title}{Term.RESET}", "─" * width]
    max_k = max(len(k) for k, _ in pairs) + 2
    for k, v in pairs:
        out.append(f"  {Term.DIM}{k.ljust(max_k)}{Term.RESET} {v}")
    out.append("─" * width)
    return "\n".join(out)
```

### 6.3 Graceful Signal Handling
```python
import signal
import sys

def setup_clean_sigint():
    def _handler(sig, frame):
        # Restore terminal cursor if hidden
        sys.stderr.write("\033[?25h\n\nAborted by user (Ctrl+C).\n")
        sys.stderr.flush()
        sys.exit(130)
    signal.signal(signal.SIGINT, _handler)
```

---

## 7. Actionable Recommendations for the Ringo Repository

Applying these principles to Ringo's existing architecture (`ringo.py`, `workflow.py`, `evaluate.py`, `improve.py`):

1. **Unify Subcommand Help & Listing (`ringo.py`):**
   * Expand the root `ringo.py --help` from minimal one-line descriptions to a structured help screen grouping commands into Core Workflows (`review`, `design`, `work`) and Verification (`evaluate`, `improve`), complete with copy-pasteable example commands.
2. **Context-Aware Error Reporting:**
   * In `workflow.load_spec` and `evaluate.py`, replace raw `ValueError` exceptions with structured error microcopy highlighting the offending file, invalid key, allowed keys, and recommended corrections.
3. **Stream Discipline in `ringo.py review` and `ringo.py design`:**
   * Direct model findings and generated proposals directly to `stdout` (or `--out`), while directing pass announcements, token counts, and costs to `stderr`.
4. **Standardize Exit Codes:**
   * Maintain the clean contract: `0` for verified/recommended completion, `1` for rejected/needs-revision/budget-exhausted, and `2` for argument/spec parsing failures.
5. **Interactive Summary Banners:**
   * Format the final completion of `python3 ringo.py work` using aligned key-value summary blocks with clear status badges (`✔ ready_for_review` or `✖ needs_revision`).

---

## References & Further Reading
* **Command Line Interface Guidelines:** [clig.dev](https://clig.dev/)
* **NO_COLOR Standard Specification:** [no-color.org](https://no-color.org/)
* **Heroku CLI Style Guide:** [devcenter.heroku.com/articles/cli-style-guide](https://devcenter.heroku.com/articles/cli-style-guide)
* **POSIX Utility Conventions (IEEE Std 1003.1-2017):** Section 12.2, *Utility Syntax Guidelines*
* **GNU Coding Standards:** Section 4.7, *Standards for Command Line Interfaces*

# Terminal feedback and throughput

Review, design and work show a live status line on stderr when attached to a
terminal. It lists the active step, retry attempt, number of active calls and
elapsed time. Finished attempts show their duration and reported output tok/s.
The final terminal summary shows call count, elapsed time, total tokens with the
uncached input/cache read/cache write/output breakdown, throughput and reported
cost. Cached reads count toward total token consumption. Totals are marked
partial when usage is missing or a call is unfinished; unreported prices stay
marked unknown. A higher output tok/s does not establish lower task token usage.
Ctrl+C clears the status line and exits with status 130. Once accounting has
started, review/design preserve a summary in `--trace`; work saves its report
under `--out`.

Both controls are available on review, design and work:

- `--progress auto` (default): live status in a terminal; quiet when redirected.
- `--progress always`: also print step events when redirected, without animation.
- `--progress never`: disable step feedback.
- `--summary-format auto` (default): readable text in a terminal; JSON on stderr
  when redirected. Review/design keep the existing `harness: ` JSON prefix.
- `--summary-format text` or `json`: select the summary explicitly.

The result stays on stdout (or the chosen result file); progress uses stderr.
Work continues to print its compact JSON outcome on stdout; the full report is
in `report.json`. Trace records and
summaries retain machine-readable timing and usage regardless of display flags.

`output_tokens_per_second` means reported output tokens divided by elapsed wall
time. Per-attempt rates include backend startup, reasoning and reply processing.
The run rate uses total task wall time, including setup, retries, parallel waits
and, for work, checks and council processing. Overlapping call durations are not
added together. This is effective task throughput, not a provider decoding-speed
measurement. Providers determine whether reasoning tokens are counted as output.
Rates appear after usage arrives; status during a silent call shows elapsed time.

A missing output count produces a null attempt rate. The run rate is null when
any call's token accounting is incomplete, a call is unfinished, no calls ran,
or elapsed time is zero. An explicitly reported zero output count produces zero.
Known tokens from failed or malformed attempts count toward the run totals.

The existing subprocess runner blocks on child completion in a waiter thread,
avoiding POSIX timed-wait polling, and retains its soft-limit/probe controls. Output recovery, descendant
cleanup and login refresh persistence apply on timeout as well as normal exit.
Prompt hashes are shared across passes and reused on retries.

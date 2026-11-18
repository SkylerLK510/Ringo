# Structured stream invariants (Claude / Codex adapters)

Scope: the stdout of `claude` / `codex app-server` children as read by `cli/src/lib.rs` (`Session::spawn`),
`cli/src/framing.rs` and `cli/src/parse.rs`. This is an interface contract with tests, not a proof of isolation,
safety or lower token use. Client versions exercised by the fixtures: Claude Code 2.1.316 shapes, Codex app-server
shapes recorded in `parse.rs` tests. Live behavior of other versions is an assumption.

| # | Invariant | Kind | Enforced / tested by |
|---|-----------|------|----------------------|
| F1 | A frame is at most `MAX_FRAME` (16 MiB); the reader never holds more than that | guarantee | `framing::tests::newline_free_input_never_holds_more_than_the_cap` |
| F2 | An oversized frame is reported when the cap is crossed (not when it ends), its remainder dropped unseen, the pipe kept draining, the child group stopped, the turn failed | guarantee | `an_oversized_frame_fails_the_turn_closed`, framing tests |
| F3 | Invalid UTF-8 is never replaced; it fails the turn | guarantee | `invalid_utf8_is_refused_not_replaced` |
| F4 | Frames split at any byte boundary, or coalesced, decode identically | guarantee | `frames_survive_every_split_and_coalescing` |
| F5 | A final frame without newline is passed on; non-JSON or cut JSON fails in the parser, so it is never success | guarantee | `a_frame_cut_at_the_end_of_the_stream_is_flagged_partial`, `Parser::line` |
| F7 | Free-text (`Plain`) output is capped but decoded lossily, as before | guarantee | `lossy_mode_keeps_free_text_readable_but_still_caps_it` |
| F6 | Raw `events.jsonl` log stops growing at `MAX_EVENTS_LOG` (256 MiB, a record that would pass it is not written); the turn is unaffected | guarantee | `lib.rs` reader |
| C1 | Codex: requests 1/2/3 are each answered once, the thread names a thread, the turn is answered only after it was asked for; a repeat or an answer to an unmade request fails the turn | guarantee | `codex_replayed_or_unrequested_answers_fail_closed` |
| C2 | Codex: events of another thread, and a `turn/completed` before the turn's acknowledgement or naming no/another turn, do not complete, fail or count this turn | guarantee | `codex_another_turns_or_threads_events_...` |
| C3 | Codex: exactly one own `turn/completed`; a missing or duplicate one fails; text without completion is not success | guarantee | `codex_duplicate_completion_fails`, `codex_server_requests_...` |
| C4 | Codex: any server-initiated request is refused with -32601, never served; no tool authority comes from the stream | guarantee | `codex_server_requests_are_refused...` |
| C5 | Counters: a total counts only if input and output are whole numbers not below the last accepted; otherwise ignored with a log note, last good count kept, absent counts stay unknown | documented rule | `codex_counters_that_are_malformed_or_go_down...` |
| C6 | A framing violation keeps usage already known | guarantee | `a_framing_violation_fails_the_turn_and_keeps_known_usage` |
| A1 | Claude events are validated only as JSON objects with the existing completion/`result` checks; request/turn correlation is not stronger than the client provides | assumption / gap | not hardened in this PR |
| A2 | Cancellation, stdout/stderr flood and blocked-stdin behavior beyond the stop-group path, queue saturation (`mpsc` channels are unbounded) | gap | not hardened in this PR |
| A3 | Descendants that leave the process group are not reached by stop | limitation | existing |
| A4 | Delivery reconciliation after disconnect (PR 1 item 4) | gap | not addressed here |

Gaps A1-A4 remain open; do not call the stream airtight.

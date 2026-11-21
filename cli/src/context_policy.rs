//! The boundary context policy: at the start of an owner's read turn, whether an agent's resumable thread goes on
//! (`Resume`) or starts fresh from Ringo's record (`Fresh`). Pure: no I/O and no model call; `Session::handoff` gathers
//! the signals and acts on the verdict. The reasoning and the worked examples are in the adaptive-compaction proposal.

use super::fmt_k;

/// How the owner asked for context to be managed (`RINGO_CONTEXT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The default: the size test `fills > threshold`; the adaptive verdict is only logged when it differs.
    Static,
    /// The cost model decides.
    Adaptive,
    /// An existing thread is never started fresh automatically.
    Off,
}

impl Mode {
    /// The mode named by the variable's text, and the text itself when it names none (the default mode is used).
    pub fn parse(text: Option<&str>) -> (Mode, Option<String>) {
        match text.map(str::trim) {
            None | Some("") | Some("static") => (Mode::Static, None),
            Some("adaptive") => (Mode::Adaptive, None),
            Some("off") => (Mode::Off, None),
            Some(other) => (Mode::Static, Some(other.to_string())),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Resume,
    Fresh,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Resume => "resume",
            Action::Fresh => "fresh",
        }
    }
}

/// Assumed: the share of the carried context (fills minus the fixed base) that a fresh agent reads back in
/// (PROPOSAL.md, not yet calibrated).
const RHO_PERCENT_KEPT: u64 = 75; // 1 - RHO, with RHO = 0.25
/// Assumed: the projected saving over a turn's calls must reach this many tokens while the cache is warm
/// (PROPOSAL.md; a cold cache uses 0).
pub const MARGIN: u64 = 32_000;
/// Calls per turn assumed when the agent has no finished read turn here yet (the measured median in PROPOSAL.md).
pub const DEFAULT_CALLS: u64 = 5;

/// Whether the cost model could say fresh at all, with an empty record (its best case): when not, the record need
/// not be built to know the answer is resume. Guards and the no-base fallback do not need the record either.
pub fn could_start_fresh(fills: u64, base: Option<u64>, calls: Option<u64>, cold: bool) -> bool {
    let Some(base) = base else { return false };
    let best = fills.saturating_sub(base) * RHO_PERCENT_KEPT / 100;
    best > 0 && (cold || calls.unwrap_or(DEFAULT_CALLS).max(1) * best >= MARGIN)
}
/// Assumed prompt-cache lifetime of a Claude subscription turn, in seconds.
pub const CLAUDE_TTL_S: u64 = 3600;
/// Assumed prompt-cache lifetime of a Codex turn, in seconds (not documented by the provider).
pub const CODEX_TTL_S: u64 = 1800;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signals {
    /// The last `Note::Context` of the agent's previous read turn.
    pub fills: u64,
    /// The agent's fixed base (median of the last 3 measurements); None = unmeasured.
    pub base: Option<u64>,
    /// About = record text bytes / 4: the exact text the fresh start would send.
    /// None when the record was not built because even an empty one could not pay (`could_start_fresh`).
    pub record: Option<u64>,
    /// Median `calls` of the agent's last <= 5 finished read turns in this lane.
    pub calls: Option<u64>,
    /// Seconds since the agent's previous read turn ended.
    pub idle_s: Option<u64>,
    pub ttl_s: u64,
    /// The previous read turn Failed or was Cancelled.
    pub previous_ended_badly: bool,
    /// The owner's message is a bare continuation (`is_continuation`).
    pub continuation: bool,
    /// The previous read turn of this agent was itself a fresh start.
    pub fresh_last_turn: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub action: Action,
    /// Text for the log line after the action: a guard, or the inputs.
    pub reason: String,
    /// The projected saving per call, in tokens, when the cost model ran.
    pub saving: Option<i64>,
}

fn fmt_signed_k(n: i64) -> String {
    if n < 0 { format!("-{}", fmt_k(n.unsigned_abs())) } else { fmt_k(n as u64) }
}

pub fn decide(s: &Signals, fallback_threshold: u64) -> Verdict {
    let guard = |why: &str| Verdict { action: Action::Resume, reason: format!("guard: {why}"), saving: None };
    if s.previous_ended_badly {
        return guard("previous turn stopped or failed");
    }
    if s.continuation {
        return guard("owner continues");
    }
    if s.fresh_last_turn {
        return guard("fresh start last turn");
    }
    let Some(base) = s.base else {
        let fresh = s.fills > fallback_threshold;
        return Verdict {
            action: if fresh { Action::Fresh } else { Action::Resume },
            reason: format!("no base yet: static {}k", fallback_threshold / 1000),
            saving: None,
        };
    };
    let idle_note = |cold: bool| match s.idle_s {
        Some(i) => format!(" idle {}m ({})", i / 60, if cold { "cold" } else { "warm" }),
        None => String::new(),
    };
    let n = s.calls.unwrap_or(DEFAULT_CALLS).max(1);
    let cold = s.idle_s.is_some_and(|i| i > s.ttl_s);
    let Some(record) = s.record else {
        // not built: a fresh start could not pay even with an empty record, so no record size or saving is claimed
        return Verdict { action: Action::Resume, saving: None,
                         reason: format!("fills {} base {} calls~{n}: cannot pay even with an empty record{}",
                                         fmt_k(s.fills), fmt_k(base), idle_note(cold)) };
    };
    // s = (1 - RHO) * (fills - base) - record, in whole tokens
    let saving = (s.fills.saturating_sub(base) * RHO_PERCENT_KEPT / 100) as i64 - record as i64;
    let margin = if cold { 0 } else { MARGIN };
    let pays = saving > 0 && (n as i128) * (saving as i128) >= margin as i128;
    // The record must cost at most half of the gross saving (`saving` is net of it): a start that barely pays,
    // with a record as large as what it saves, throws a conversation away for nothing.
    let covers = saving >= record as i64;
    let fresh = pays && covers;
    let idle = idle_note(cold);
    let why = if pays && !covers { ": the saving does not cover twice the record" } else { "" };
    Verdict {
        action: if fresh { Action::Fresh } else { Action::Resume },
        reason: format!("fills {} base {} record {} calls~{n} save~{}{idle}{why}", fmt_k(s.fills), fmt_k(base), fmt_k(record),
                        fmt_signed_k(saving.saturating_mul(n as i64))),
        saving: Some(saving),
    }
}

const CONTINUATIONS: [&str; 11] = ["continue", "go on", "keep going", "yes", "ok", "okay", "go ahead", "proceed",
                                   "carry on", "please continue", "continue please"];

/// Whether the owner's message only says to go on: an exact phrase, or at most 3 words (image markers are not words).
pub fn is_continuation(prompt: &str) -> bool {
    let mut text = String::new();
    let mut rest = prompt;
    while let Some(at) = rest.find("[image:") {
        text.push_str(&rest[..at]);
        text.push(' ');
        match rest[at..].find(']') {
            Some(end) => rest = &rest[at + end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    text.push_str(rest);
    let lower = text.trim().to_lowercase();
    let lower = lower.trim_end_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
    CONTINUATIONS.contains(&lower) || lower.split_whitespace().count() <= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warm(fills: u64, calls: u64) -> Signals {
        Signals { fills, base: Some(13_000), record: Some(10_000), calls: Some(calls), idle_s: Some(60), ttl_s: CODEX_TTL_S,
                  previous_ended_badly: false, continuation: false, fresh_last_turn: false }
    }

    #[test]
    fn the_worked_examples_decide_as_the_proposal_says() {
        let v = decide(&warm(56_000, 5), 48_000);
        assert_eq!((v.action, v.saving), (Action::Fresh, Some(22_250)));
        assert!(v.reason.contains("calls~5") && v.reason.contains("save~111.2k") && v.reason.contains("(warm)"), "{}", v.reason);
        // f=30k, N=5, cold-or-warm: saves 12.75k - 10k = 2.75k, which does not cover the 10k record
        assert_eq!(decide(&warm(30_000, 5), 48_000).action, Action::Resume);
        assert_eq!(decide(&warm(60_000, 1), 48_000).action, Action::Resume);
        let cold = Signals { idle_s: Some(41 * 60), ..warm(30_000, 5) };
        let v = decide(&cold, 48_000);
        assert_eq!((v.action, v.saving), (Action::Resume, Some(2_750)), "2.75k < the 10k record");
        assert!(v.reason.contains("idle 41m (cold)"), "{}", v.reason);
        let cold = Signals { idle_s: Some(41 * 60), ..warm(56_000, 1) };
        let v = decide(&cold, 48_000);
        assert_eq!(v.action, Action::Fresh);
        assert!(v.reason.contains("idle 41m (cold)"), "{}", v.reason);
    }

    #[test]
    fn the_saving_must_cover_twice_the_record() {
        // f=40k N=2 cold: 20.25k gross - 10k = 10.25k >= 10k (margin 0, cold) -> fresh
        let cold = |fills, calls| Signals { idle_s: Some(41 * 60), ..warm(fills, calls) };
        let v = decide(&cold(40_000, 2), 48_000);
        assert_eq!((v.action, v.saving), (Action::Fresh, Some(10_250)));
        assert!(!v.reason.contains("does not cover"), "{}", v.reason);
        // the same warm: 2 x 10.25k < 32k -> resume, for the old reason (the margin), not the new one
        let v = decide(&warm(40_000, 2), 48_000);
        assert_eq!(v.action, Action::Resume);
        assert!(!v.reason.contains("does not cover"), "{}", v.reason);
        // f=30k N=5 cold: the saving is positive and pays cold, but is 2.75k against a 10k record
        let v = decide(&cold(30_000, 5), 48_000);
        assert_eq!(v.action, Action::Resume);
        assert!(v.reason.contains("fills 30.0k base 13.0k record 10.0k calls~5 save~13.8k"), "{}", v.reason);
        assert!(v.reason.ends_with(": the saving does not cover twice the record"), "{}", v.reason);
        // exactly twice is enough; a byte less is not
        let at = |record| Signals { record: Some(record), ..cold(40_000, 1) };
        assert_eq!(decide(&at(10_125), 48_000).action, Action::Fresh); // 20_250 - 10_125 = 10_125
        assert_eq!(decide(&at(10_126), 48_000).action, Action::Resume);
        // an empty record always covers itself
        assert_eq!(decide(&Signals { record: Some(0), ..cold(40_000, 1) }, 48_000).action, Action::Fresh);
    }

    #[test]
    fn a_saving_that_is_not_positive_never_starts_fresh() {
        let cold = Signals { idle_s: Some(99_999), ..warm(20_000, 40) };
        let v = decide(&cold, 48_000); // (7k * 3 / 4) - 10k < 0
        assert_eq!(v.action, Action::Resume);
        assert!(v.saving.unwrap() < 0);
        assert!(v.reason.contains("save~-"), "{}", v.reason);
        assert_eq!(decide(&Signals { fills: 5_000, ..warm(0, 5) }, 48_000).action, Action::Resume); // below the base
    }

    #[test]
    fn missing_calls_assume_the_default_and_idle_unknown_is_warm() {
        let s = Signals { calls: None, idle_s: None, ..warm(56_000, 5) };
        let v = decide(&s, 48_000);
        assert_eq!(v.action, Action::Fresh);
        assert!(v.reason.contains("calls~5") && !v.reason.contains("idle"), "{}", v.reason);
        assert_eq!(decide(&Signals { calls: Some(0), ..warm(60_000, 5) }, 48_000).action, Action::Resume); // 0 counts as 1
    }

    #[test]
    fn each_guard_resumes_with_its_reason() {
        let big = warm(120_000, 5);
        for (s, why) in [
            (Signals { previous_ended_badly: true, ..big.clone() }, "guard: previous turn stopped or failed"),
            (Signals { continuation: true, ..big.clone() }, "guard: owner continues"),
            (Signals { fresh_last_turn: true, ..big.clone() }, "guard: fresh start last turn"),
        ] {
            let v = decide(&s, 48_000);
            assert_eq!((v.action, v.reason.as_str(), v.saving), (Action::Resume, why, None));
        }
        assert_eq!(decide(&big, 48_000).action, Action::Fresh);
    }

    #[test]
    fn an_unmeasured_base_falls_back_to_the_static_threshold() {
        let s = Signals { base: None, ..warm(50_000, 5) };
        let v = decide(&s, 48_000);
        assert_eq!((v.action, v.reason.as_str()), (Action::Fresh, "no base yet: static 48k"));
        assert_eq!(decide(&Signals { fills: 48_000, ..s.clone() }, 48_000).action, Action::Resume);
        assert_eq!(decide(&s, 64_000).reason, "no base yet: static 64k");
    }

    #[test]
    fn continuations_are_phrases_or_at_most_three_words() {
        for yes in ["continue", "Continue.", "  go on! ", "keep going", "yes", "OK", "okay", "go ahead", "proceed", "carry on",
                    "please continue", "continue please", "do it now", "[image:/tmp/a.png] [image:/tmp/b.png] yes please",
                    "[image:/tmp/a.png] look"] {
            assert!(is_continuation(yes), "{yes}");
        }
        for no in ["fix the failing tests", "please continue with the parser rewrite", "what does this do?",
                   "[image:/tmp/a.png] what is this picture"] {
            assert!(!is_continuation(no), "{no}");
        }
    }

    #[test]
    fn the_variable_names_a_mode_and_a_bad_value_is_reported() {
        assert_eq!(Mode::parse(None), (Mode::Static, None));
        assert_eq!(Mode::parse(Some("static")), (Mode::Static, None));
        assert_eq!(Mode::parse(Some(" adaptive ")), (Mode::Adaptive, None));
        assert_eq!(Mode::parse(Some("off")), (Mode::Off, None));
        assert_eq!(Mode::parse(Some("sometimes")), (Mode::Static, Some("sometimes".into())));
    }

    #[test]
    fn the_record_is_needed_only_when_the_cost_model_could_say_fresh() {
        assert!(!could_start_fresh(60_000, None, Some(5), false), "no base: the size test, no record needed");
        assert!(!could_start_fresh(13_000, Some(13_000), Some(50), true), "nothing to save");
        assert!(!could_start_fresh(20_000, Some(13_000), Some(5), false), "5 x 5.25k < 32k even with no record");
        assert!(could_start_fresh(20_000, Some(13_000), Some(5), true), "cold: any positive saving");
        assert!(could_start_fresh(56_000, Some(13_000), None, false), "the default 5 calls");
    }


    #[test]
    fn an_unbuilt_record_claims_no_size_or_saving() {
        let s = Signals { fills: 30_000, base: Some(13_000), record: None, calls: Some(1), idle_s: Some(60),
                          ttl_s: CODEX_TTL_S, previous_ended_badly: false, continuation: false, fresh_last_turn: false };
        let v = decide(&s, 48_000);
        assert_eq!((v.action, v.saving), (Action::Resume, None));
        assert!(v.reason.contains("cannot pay even with an empty record") && !v.reason.contains("record 0"), "{}", v.reason);
    }

}

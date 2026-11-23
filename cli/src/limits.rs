//! Usage limits as the agents report them: what was hit (the plan's usage, a rate limit, a busy provider), and when it
//! starts over. Each agent says it in its own way (Codex's `codexErrorInfo` and rate-limit snapshot, Claude's
//! `rate_limit_event`, agy's error text), so the parsers turn what they see into one `Limit`, and the shell says it
//! plainly instead of a generic failure. Nothing here reroutes a turn: it only names what happened.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long Codex may keep reporting `willRetry` errors with no other progress before the turn is stopped. Codex
/// retries a dropped or refused request by itself and says so each time; with the network down or a provider refusing,
/// it can do that for a long while, and the job sits at "retrying" the whole time. T3 Code stops at the same 10
/// minutes (`provider-core/src/server/failure.ts`).
pub const CODEX_RETRY_STALL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    /// The plan's own usage is used up (a window at 100%, a quota): it starts over at a known time.
    Usage,
    /// Too many requests too fast: a short wait.
    Rate,
    /// The provider is busy (Claude's 529): retryable at once.
    Overloaded,
}

impl LimitKind {
    pub fn name(self) -> &'static str {
        match self {
            LimitKind::Usage => "usage",
            LimitKind::Rate => "rate",
            LimitKind::Overloaded => "overloaded",
        }
    }
}

/// A limit an agent's turn ran into.
#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    /// codex, claude or agy: the program that reported it (not the shell's name for the agent).
    pub agent: &'static str,
    pub kind: LimitKind,
    /// When it starts over, in unix seconds, when the agent said.
    pub resets_at: Option<u64>,
    /// What the agent said, cut short, for the log.
    pub message: String,
}

/// A moment, as the monotonic clock and the wall clock read it. The parser takes one with each line, so a test can
/// say what time it is (a stall is ten minutes of monotonic time; a reset is a wall-clock time).
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub mono: Instant,
    pub unix: u64,
}

impl Clock {
    pub fn now() -> Clock {
        Clock { mono: Instant::now(), unix: unix_now() }
    }

    /// The moment `after` later than this one.
    pub fn plus(self, after: Duration) -> Clock {
        Clock { mono: self.mono + after, unix: self.unix + after.as_secs() }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Limit {
    /// What the job's failure says, plainly, with the reset time in the owner's local time.
    pub fn describe(&self) -> String {
        let now = unix_now();
        self.describe_at(now, local_offset(now))
    }

    /// `describe` at a given time and zone (seconds east of UTC; None when the zone is unknown, then UTC is said).
    pub fn describe_at(&self, now: u64, offset: Option<i64>) -> String {
        match (self.kind, self.resets_at) {
            (LimitKind::Usage, Some(at)) if at > now => format!("usage limit reached — resets at {}", fmt_reset(at, now, offset)),
            (LimitKind::Usage, Some(_)) => "usage limit reached — its reset time has already passed, so try again".into(),
            (LimitKind::Usage, None) => "usage limit reached — the reset time was not reported".into(),
            (LimitKind::Rate, Some(at)) if at > now => format!("rate limited — try again at {}", fmt_reset(at, now, offset)),
            (LimitKind::Rate, _) => "rate limited — try again in a few minutes".into(),
            (LimitKind::Overloaded, _) => "overloaded — the provider is busy; try again shortly".into(),
        }
    }

    /// The same limit when more is known of it: what `self` knew stays where `later` says nothing.
    pub fn refined_by(&self, later: Limit) -> Limit {
        Limit { resets_at: later.resets_at.or(self.resets_at), ..later }
    }
}

/// Seconds east of UTC the machine's local time is at `at`; None where the machine cannot say.
#[cfg(unix)]
pub fn local_offset(at: u64) -> Option<i64> {
    let t = at as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` it is given, which is valid and not shared.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let done = unsafe { libc::localtime_r(&t, &mut tm) };
    (!done.is_null()).then_some(tm.tm_gmtoff as i64)
}

#[cfg(not(unix))]
pub fn local_offset(_at: u64) -> Option<i64> {
    None
}

/// Days from 1970-01-01 to a date (H. Hinnant's days_from_civil, as `iso_secs` has it).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day count from 1970-01-01 (the inverse).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// A reset time as the owner reads it: `15:38 (local)` today, `Nov 25 15:38 (local)` on another day. With no zone
/// known it says UTC.
pub fn fmt_reset(at: u64, now: u64, offset: Option<i64>) -> String {
    let off = offset.unwrap_or(0);
    let (local, here) = (at as i64 + off, now as i64 + off);
    let (day, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let clock = format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60);
    let zone = if offset.is_some() { "(local)" } else { "(UTC)" };
    if day == here.div_euclid(86_400) {
        return format!("{clock} {zone}");
    }
    let (_, month, date) = civil_from_days(day);
    format!("{} {date} {clock} {zone}", MONTHS[(month - 1) as usize])
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a fixed pattern"))
}

/// A usage limit's reset as Codex writes it into its error text, in its own local time with the seconds dropped:
/// "Try again at 3:37 PM." (today) or "try again at Nov 11th, 2026 3:37 PM.", and "try again in 20 minutes". It is
/// the only reset left when a proxy strips the rate-limit snapshot (T3 Code's `codexMessageResetAt`). The minute
/// after the one named is the first that is past the reset. None when the text names no time, or one already past.
pub fn codex_reset_from_text(message: &str, now: u64, offset: Option<i64>) -> Option<u64> {
    static AT: OnceLock<Regex> = OnceLock::new();
    static IN: OnceLock<Regex> = OnceLock::new();
    static PART: OnceLock<Regex> = OnceLock::new();
    let at = regex(&AT, r"(?i-u)try again at (?:(?P<mon>[a-z]{3})[a-z]* (?P<day>\d{1,2})(?:st|nd|rd|th)?,? (?P<year>\d{4}),? )?(?P<h>\d{1,2}):(?P<m>\d{2}) ?(?P<ap>[ap])\.?m");
    if let Some(c) = at.captures(message) {
        let offset = offset?;
        let hour = c["h"].parse::<i64>().ok().filter(|h| (1..=12).contains(h))? % 12 + if c["ap"].eq_ignore_ascii_case("p") { 12 } else { 0 };
        let minute = c["m"].parse::<i64>().ok().filter(|m| *m < 60)?;
        let day = match c.name("mon") {
            Some(mon) => {
                let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(mon.as_str()))? as i64 + 1;
                days_from_civil(c["year"].parse().ok()?, month, c["day"].parse().ok()?)
            }
            None => (now as i64 + offset).div_euclid(86_400), // today's form: the day it was said
        };
        let reset = day * 86_400 + hour * 3600 + minute * 60 + 60 - offset;
        return u64::try_from(reset).ok().filter(|r| *r > now);
    }
    let after = regex(&IN, r"(?i-u)try again in ((?:\d+\s*[a-z]+[\s,]*(?:and\s+)?)+)").captures(message)?;
    let part = regex(&PART, r"(?i-u)(\d+)\s*(days?|d|hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)\b");
    let secs: u64 = part.captures_iter(&after[1]).map(|p| {
        let n: u64 = p[1].parse().unwrap_or(0);
        n * match p[2].to_ascii_lowercase().chars().next() { Some('d') => 86_400, Some('h') => 3600, Some('m') => 60, _ => 1 }
    }).sum();
    (secs > 0).then_some(now + secs)
}

/// When a Codex rate-limit snapshot says the turn can go on: the latest reset among its windows that are used up
/// (both when both are), and one that is still in the future. None when no window is full.
pub fn codex_reset_from_snapshot(snapshot: &Value, now: u64) -> Option<u64> {
    ["primary", "secondary"].iter().filter_map(|key| snapshot.get(*key))
        .filter(|w| w["usedPercent"].as_f64().is_some_and(|p| p >= 100.0))
        .filter_map(|w| w["resetsAt"].as_u64()).filter(|r| *r > now).max()
}

/// A `rateLimits` snapshot's update merged into the one before it. An update is sparse: what it leaves out or sends
/// as null keeps its earlier value (Codex 0.170.0's schema: "Nullable account metadata may be unavailable in a
/// rolling update and does not clear a previously observed value"). A snapshot of another limit than Codex's own
/// (`limitId` other than "codex": a model-specific bucket) is not the plan's and changes nothing. True when it did.
pub fn merge_codex_snapshot(into: &mut Value, update: &Value) -> bool {
    let Some(new) = update.as_object() else { return false };
    if new.get("limitId").and_then(Value::as_str).is_some_and(|id| id != "codex") {
        return false;
    }
    if !into.is_object() {
        *into = Value::Object(Default::default());
    }
    let old = into.as_object_mut().expect("made an object above");
    let mut changed = false;
    for (key, value) in new.iter().filter(|(_, v)| !v.is_null()) {
        changed |= old.get(key) != Some(value);
        old.insert(key.clone(), value.clone());
    }
    changed
}

/// A Go duration as agy prints it ("3600s", "1h2m3.5s", "42s"), in seconds; None when it is not one.
fn go_duration_secs(text: &str) -> Option<f64> {
    static PART: OnceLock<Regex> = OnceLock::new();
    let part = regex(&PART, r"(\d+(?:\.\d+)?)(ns|us|µs|μs|ms|h|m|s)");
    let total: f64 = part.captures_iter(text).map(|c| {
        let n: f64 = c[1].parse().unwrap_or(0.0);
        n * match &c[2] { "ns" => 1e-9, "us" | "µs" | "μs" => 1e-6, "ms" => 1e-3, "h" => 3600.0, "m" => 60.0, _ => 1.0 }
    }).sum();
    (total > 0.0).then_some(total)
}

/// A limit in agy's own error text, which is Google's API error as agy's harness renders it: `QUOTA_EXHAUSTED` is
/// the plan's quota, `RESOURCE_EXHAUSTED` or a 429 without it is a rate limit, and a refusal is how agy's SDK stops a
/// turn on a quota (T3 Code's `AntigravityTurnOutcome.ts`). The reset is `quotaResetTimeStamp` when it is a time,
/// else `quotaResetDelay` or the plain rate limit's `retryDelay` from now. Only what agy printed itself is read: no
/// quota endpoint is called.
pub fn agy_limit(text: &str, now: u64) -> Option<Limit> {
    static STAMP: OnceLock<Regex> = OnceLock::new();
    static DELAY: OnceLock<Regex> = OnceLock::new();
    static RETRY: OnceLock<Regex> = OnceLock::new();
    static CODE: OnceLock<Regex> = OnceLock::new();
    let quota = text.contains("QUOTA_EXHAUSTED");
    let rate = text.contains("RESOURCE_EXHAUSTED") || regex(&CODE, r"Error 429\b|\(code 429\)").is_match(text);
    let refused = text.to_ascii_lowercase().contains("refusal");
    if !(quota || rate || refused) {
        return None;
    }
    let stamp = regex(&STAMP, r"quotaResetTimeStamp:([^\s\]]+)").captures(text).and_then(|c| crate::iso_secs(&c[1]));
    let delay = |pattern: &'static OnceLock<Regex>, key: &str| {
        regex(pattern, &format!(r"\b{key}:((?:\d+(?:\.\d+)?(?:ns|us|µs|μs|ms|h|m|s))+)")).captures(text).and_then(|c| go_duration_secs(&c[1]))
    };
    let after = delay(&DELAY, "quotaResetDelay").or_else(|| delay(&RETRY, "retryDelay"));
    let resets_at = stamp.or(after.map(|s| now + s.ceil() as u64));
    // a refusal or a quota is the plan's; a rate limit that carries no quota is the short kind
    let kind = if quota || refused || stamp.is_some() { LimitKind::Usage } else { LimitKind::Rate };
    Some(Limit { agent: "agy", kind, resets_at, message: crate::parse::short(text, 200) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 2026-11-23 12:00:00 UTC; the owner's zone in these tests is UTC+6 (Dhaka), so 18:00 local.
    const NOW: u64 = 1_795_435_200;
    const DHAKA: Option<i64> = Some(6 * 3600);

    #[test]
    fn reset_times_read_in_local_time() {
        assert_eq!(civil_from_days(days_from_civil(2026, 11, 23)), (2026, 11, 23));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // today's form reads in the zone of the machine: 18:37 local is 12:37 UTC, and the minute after it counts
        assert_eq!(codex_reset_from_text("You've hit your usage limit. Try again at 6:37 PM.", NOW, DHAKA), Some(NOW + 37 * 60 + 60));
        // a dated form, and 12 AM/12 PM
        let dated = codex_reset_from_text("Upgrade, or try again at Dec 1st, 2026 12:05 AM.", NOW, DHAKA).unwrap();
        assert_eq!(fmt_reset(dated, NOW, DHAKA), "Dec 1 00:06 (local)");
        assert_eq!(codex_reset_from_text("Try again at 12:30 PM.", NOW, DHAKA), None, "12:30 local is before the 18:00 it is now");
        // a time already past today, no time in the text, a zone the machine cannot name
        assert_eq!(codex_reset_from_text("Try again at 8:15 AM.", NOW, DHAKA), None);
        assert_eq!(codex_reset_from_text("You've hit your usage limit. Try again later.", NOW, DHAKA), None);
        assert_eq!(codex_reset_from_text("Try again at 6:37 PM.", NOW, None), None);
        // a wait
        assert_eq!(codex_reset_from_text("rate limited, try again in 20 minutes.", NOW, None), Some(NOW + 1200));
        assert_eq!(codex_reset_from_text("Try again in 1 hour 5 minutes", NOW, None), Some(NOW + 3900));
        assert_eq!(codex_reset_from_text("try again in 2 days", NOW, None), Some(NOW + 172_800));
        // how it reads
        assert_eq!(fmt_reset(NOW + 3600, NOW, DHAKA), "19:00 (local)");
        assert_eq!(fmt_reset(NOW + 3600, NOW, None), "13:00 (UTC)");
        assert_eq!(fmt_reset(NOW + 86_400, NOW, DHAKA), "Nov 24 18:00 (local)");
    }

    #[test]
    fn a_limit_is_described_plainly() {
        let limit = |kind, resets_at| Limit { agent: "codex", kind, resets_at, message: String::new() };
        let said = |l: Limit| l.describe_at(NOW, DHAKA);
        assert_eq!(said(limit(LimitKind::Usage, Some(NOW + 3600))), "usage limit reached — resets at 19:00 (local)");
        assert_eq!(said(limit(LimitKind::Usage, None)), "usage limit reached — the reset time was not reported");
        assert!(said(limit(LimitKind::Usage, Some(NOW - 5))).contains("already passed"));
        assert_eq!(said(limit(LimitKind::Rate, Some(NOW + 60))), "rate limited — try again at 18:01 (local)");
        assert_eq!(said(limit(LimitKind::Rate, None)), "rate limited — try again in a few minutes");
        assert_eq!(said(limit(LimitKind::Overloaded, None)), "overloaded — the provider is busy; try again shortly");
    }

    #[test]
    fn snapshots_merge_sparsely_and_only_for_codexs_own_limit() {
        let mut snapshot = Value::Null;
        let first = json!({"limitId": "codex", "planType": "plus", "primary": {"usedPercent": 100, "resetsAt": NOW + 900},
                           "secondary": {"usedPercent": 40, "resetsAt": NOW + 90_000}});
        assert!(merge_codex_snapshot(&mut snapshot, &first));
        // a sparse update: the plan is not told again and the secondary is null, so both stay
        assert!(merge_codex_snapshot(&mut snapshot, &json!({"limitId": "codex", "planType": null, "secondary": null,
                                                            "primary": {"usedPercent": 100, "resetsAt": NOW + 1200}})));
        assert_eq!((snapshot["planType"].as_str(), snapshot["secondary"]["usedPercent"].as_u64()), (Some("plus"), Some(40)));
        assert_eq!(codex_reset_from_snapshot(&snapshot, NOW), Some(NOW + 1200));
        // another bucket (a model's own) is not the plan
        assert!(!merge_codex_snapshot(&mut snapshot, &json!({"limitId": "codex_other", "primary": {"usedPercent": 1}})));
        assert_eq!(snapshot["primary"]["usedPercent"], 100);
        // both windows full: the later reset; none full: none; one already over: none
        assert_eq!(codex_reset_from_snapshot(&json!({"primary": {"usedPercent": 100, "resetsAt": NOW + 10},
                                                     "secondary": {"usedPercent": 100, "resetsAt": NOW + 99}}), NOW), Some(NOW + 99));
        assert_eq!(codex_reset_from_snapshot(&json!({"primary": {"usedPercent": 99, "resetsAt": NOW + 10}}), NOW), None);
        assert_eq!(codex_reset_from_snapshot(&json!({"primary": {"usedPercent": 100, "resetsAt": NOW - 10}}), NOW), None);
    }

    #[test]
    fn agy_says_its_quota_in_its_own_error_text() {
        // the shapes of T3 Code's AntigravityTurnOutcome tests: Google's API error as agy's harness prints it
        let quota = "Error 429, Message: Quota exhausted for test model., Status: RESOURCE_EXHAUSTED, Details: [map[@type:type.googleapis.com/google.rpc.RetryInfo retryDelay:42s] map[@type:type.googleapis.com/google.rpc.ErrorInfo domain:googleapis.com metadata:map[quotaResetDelay:3600s quotaResetTimeStamp:2026-11-24T01:05:00Z] reason:QUOTA_EXHAUSTED]]";
        let l = agy_limit(quota, NOW).unwrap();
        assert_eq!((l.agent, l.kind, l.resets_at), ("agy", LimitKind::Usage, crate::iso_secs("2026-11-24T01:05:00Z")));
        // a timestamp that is not a time falls back to the delay
        let soon = quota.replace("2026-11-24T01:05:00Z", "soon");
        assert_eq!(agy_limit(&soon, NOW).unwrap().resets_at, Some(NOW + 3600));
        // a plain rate limit carries a retry delay only, and is the short kind
        let rate = "Error 429, Message: Resource has been exhausted, Status: RESOURCE_EXHAUSTED, Details: [map[@type:type.googleapis.com/google.rpc.RetryInfo retryDelay:1m30.5s]]";
        let l = agy_limit(rate, NOW).unwrap();
        assert_eq!((l.kind, l.resets_at), (LimitKind::Rate, Some(NOW + 91)));
        // a refusal is the SDK's way of stopping on a quota; other errors are not limits
        assert_eq!(agy_limit("the model refused: refusal", NOW).unwrap().kind, LimitKind::Usage);
        assert_eq!(agy_limit("Error 500, Message: boom, Status: INTERNAL", NOW), None);
        assert_eq!(go_duration_secs("1h2m3.5s"), Some(3723.5));
        assert_eq!(go_duration_secs("soon"), None);
    }
}

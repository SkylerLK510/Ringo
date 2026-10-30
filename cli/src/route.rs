//! Route suggestions for a plain line (`/route on`): a few word rules, no model call. A line that reads like a
//! design, review or edit task gets the matching command put in the input line for the owner to send, change or
//! turn back into a plain question; anything else is asked as it is. Nothing is sent without a key press, and an
//! agent turn is only ever suggested, never started.

#[derive(Debug, PartialEq)]
pub enum Route {
    Ask,
    Design,
    Review(Option<usize>),
    Edit,
}

const DESIGN: [&str; 8] = ["design", "architect", "architecture", "propose", "proposal", "tradeoff", "tradeoffs", "trade-off"];
const DESIGN_PHRASES: [&str; 6] = ["how should we", "how should i", "should we use", "should i use", "pros and cons", "approach for"];
const REVIEW: [&str; 3] = ["review", "critique", "audit"];
const EDIT: [&str; 14] = ["implement", "fix", "add", "refactor", "rename", "remove", "delete", "update", "change", "write",
                          "create", "migrate", "port", "make"];

/// The route a plain line reads as. `git`: the shell is in a git project (reviews and edits need one).
pub fn classify(line: &str, git: bool) -> Route {
    let lower = line.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '#')).filter(|w| !w.is_empty()).collect();
    let job = words.iter().find_map(|w| w.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()));
    let asks_review = words.first().is_some_and(|w| REVIEW.contains(w)) || words.windows(2).any(|p| p == ["code", "review"]);
    if git && asks_review {
        return Route::Review(job);
    }
    if words.iter().any(|w| DESIGN.contains(w)) || DESIGN_PHRASES.iter().any(|p| lower.contains(p)) {
        return Route::Design;
    }
    let first = words.first().copied().unwrap_or("");
    if git && (EDIT.contains(&first) || (first == "please" && words.get(1).is_some_and(|w| EDIT.contains(w)))) {
        return Route::Edit;
    }
    Route::Ask
}

/// The command to put in the input line for a route, and what it costs, for the suggestion line.
pub fn suggestion(route: &Route, line: &str, agent: &str) -> Option<(String, &'static str)> {
    match route {
        Route::Ask => None,
        Route::Design => Some((format!("/design {line}"), "one design call, about 2-4k tokens")),
        Route::Review(Some(n)) => Some((format!("/review #{n}"), "one review call of that job's change")),
        Route::Review(None) => Some(("/review HEAD~1".into(), "one review call of the last commit; edit BASE to review more")),
        Route::Edit => Some((format!("@{agent}! {line}"), "a full agent turn that edits in a worktree, 17k tokens and up")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_questions_stay_questions() {
        for q in ["what is LangGraph for?", "why is the sky blue", "explain the release gate", "is this design pattern common?"] {
            let r = classify(q, true);
            assert!(r == Route::Ask || r == Route::Design, "{q}: {r:?}");
        }
        assert_eq!(classify("what does this repo do?", true), Route::Ask);
        assert_eq!(classify("fix the retry loop", false), Route::Ask); // no project: no edit suggestion
    }

    #[test]
    fn tasks_get_their_route() {
        assert_eq!(classify("Design a cache for slugify results", true), Route::Design);
        assert_eq!(classify("how should we store the session state?", true), Route::Design);
        assert_eq!(classify("review #3 please", true), Route::Review(Some(3)));
        assert_eq!(classify("review my last change", true), Route::Review(None));
        assert_eq!(classify("fix the retry loop in client.rs", true), Route::Edit);
        assert_eq!(classify("please add a test for slugify", true), Route::Edit);
        let (cmd, _) = suggestion(&Route::Edit, "fix it", "codex").unwrap();
        assert_eq!(cmd, "@codex! fix it");
        assert!(suggestion(&Route::Ask, "x", "codex").is_none());
    }
}

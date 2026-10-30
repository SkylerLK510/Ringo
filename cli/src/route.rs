//! Route suggestions for a plain line (`/route on`): word rules, no model call. A line that reads like a review,
//! research, design, edit or repository question gets the matching command put in the input line for the owner to
//! send, change or turn back into a plain question; anything else is asked as it is. Nothing is sent without a key
//! press, and an agent turn is only ever suggested, never started. The rules are checked in order and the first
//! that matches wins: review, research, design, edit, repository question, ask.

#[derive(Debug, PartialEq)]
pub enum Route {
    Ask,
    Research,
    Design,
    /// A review of job N's change, of a revision range (`BASE [HEAD]` as /review takes it), or of the last commit.
    Review(Option<usize>, Option<String>),
    Edit,
    Read,
}

const RESEARCH: [&str; 15] = ["cite", "citation", "citations", "sources", "papers", "paper", "study", "studies",
                               "literature", "evidence", "research", "statistics", "published", "cve", "cves"];
const RESEARCH_PHRASES: [&str; 15] = ["state of the art", "latest findings", "what does the research", "find sources",
                                      "look up", "search for", "search the web", "with links", "link the", "links to",
                                      "as of now", "what's new in", "whats new in", "verify against", "official pages"];
const DESIGN: [&str; 9] = ["design", "architect", "architecture", "architecturally", "propose", "proposal", "tradeoff",
                           "tradeoffs", "trade-off"];
const DESIGN_PHRASES: [&str; 16] = ["how should we", "how should i", "should we use", "should i use", "pros and cons",
                                    "approach for", "how would you build", "how would you design", "how would that work",
                                    "a plan for", "come up with a", "decide for me", "rollout plan", "migration plan",
                                    "schema for", "which approach"];
const REVIEW_VERBS: [&str; 13] = ["review", "check", "double check", "double-check", "sanity check", "look over", "go over",
                                  "go through", "go back through", "have a look at", "take a look at", "critique", "audit"];
const REVIEW_OBJECTS: [&str; 15] = ["diff", "change", "changes", "commit", "commits", "pr", "branch", "edit", "patch",
                                    "what the agent", "the agent did", "agent just made", "bugs", "race", "security"];
const REVIEW_PHRASES: [&str; 4] = ["ready to merge", "break anything", "did that break", "did this break"];
const EDIT: [&str; 31] = ["implement", "fix", "add", "refactor", "rename", "remove", "delete", "update", "change", "write",
                          "create", "migrate", "port", "make", "bump", "document", "rewrite", "replace", "move", "extract",
                          "split", "convert", "upgrade", "clean", "tidy", "drop", "adjust", "tweak", "rework", "revert",
                          "wire"];
/// Request openers that an edit verb may follow within three words: "can you please add ...".
const ASKS: [&str; 14] = ["can you", "could you", "would you", "will you", "please", "pls", "plz", "how about you",
                          "i want you to", "go ahead and", "let's", "lets", "i'd like you to", "you should"];
const EDIT_PHRASES: [&str; 9] = ["get rid of", "do the same", "fix that", "fix it", "fix this", "fix them",
                                 "i want to be able to", "i would like for it to", "i would like it to"];
const REPO_PHRASES: [&str; 22] = ["this repo", "the repo", "our repo", "my repo", "this project", "our project",
                                  "the project", "our code", "my code", "this code", "the codebase", "our codebase",
                                  "in our", "do we", "did we", "are we", "we have", "we call", "we handle", "my build",
                                  "my test", "the config"];
const REPO_WORDS: [&str; 7] = ["codebase", "module", "crate", "workspace", "dependencies", "subcommands", "function"];
const FILE_EXTENSIONS: [&str; 16] = ["rs", "py", "toml", "json", "yaml", "yml", "md", "ts", "tsx", "js", "log", "txt", "sh",
                                     "lock", "cfg", "ini"];
const READ_OPENERS: [&str; 20] = ["what", "whats", "what's", "why", "how", "where", "which", "who", "when", "is", "are",
                                  "does", "do", "did", "explain", "walk", "trace", "show", "find", "look"];

/// The words of a lowercased line: letters, digits and `-_#.~'` runs, with trailing dots and quotes dropped.
fn words(lower: &str) -> Vec<&str> {
    lower.split(|c: char| !(c.is_alphanumeric() || "-_#.~'".contains(c)))
        .map(|w| w.trim_matches(|c| c == '.' || c == '\''))
        .filter(|w| !w.is_empty())
        .collect()
}

/// True when `phrase` occurs in the words as whole words.
fn has(words: &[&str], phrase: &str) -> bool {
    let p: Vec<&str> = phrase.split(' ').collect();
    words.windows(p.len()).any(|w| w == p.as_slice())
}

fn any(words: &[&str], phrases: &[&str]) -> bool {
    phrases.iter().any(|p| has(words, p))
}

/// A file name (`main.rs`, `error.log`) or a snake_case identifier (`run_task`, not `top_p`).
fn names_code(word: &str) -> bool {
    let parts: Vec<&str> = word.trim_matches('_').split('_').collect();
    word.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && FILE_EXTENSIONS.contains(&ext))
        || (parts.len() > 1 && parts.iter().all(|p| p.len() > 1))
}

/// A revision or range: `HEAD~1`, `main..feature`, `a1b2c3d`.
fn names_revision(word: &str) -> bool {
    word == "head" || word.starts_with("head~") || word.starts_with("head^") || word.contains("..")
        || (word.len() >= 7 && word.chars().all(|c| c.is_ascii_hexdigit()) && word.chars().any(|c| c.is_ascii_digit()))
}

/// The /review arguments for the revisions the line names, in their own case. Recognized: `HEAD`, `HEAD~n`, a
/// commit hash and anything with `..` (branch names alone are not: the suggestion can be edited before it is sent).
/// `a..b` or two revisions review a to b, `a..` and a lone `HEAD~n` from there to HEAD, a lone hash that commit;
/// a bare `HEAD` or no revision gives None (the last commit).
fn revision_range(line: &str) -> Option<String> {
    let clean = |w: &str| -> String {
        let w = w.trim_matches(|c: char| ",;:!?'\"()".contains(c));
        w.strip_suffix('.').filter(|_| !w.ends_with("..")).unwrap_or(w).to_string()
    };
    let revs: Vec<String> = line.split_whitespace().map(clean).filter(|w| names_revision(&w.to_lowercase())).collect();
    let first = revs.first().filter(|w| revs.len() > 1 || !w.eq_ignore_ascii_case("head"))?;
    Some(match first.split_once("..") {
        Some((base, head)) if !head.is_empty() => format!("{base} {head}"),
        Some((base, _)) => base.to_string(),
        None => match revs.get(1) {
            Some(head) if !head.contains("..") => format!("{first} {head}"),
            _ if first.chars().all(|c| c.is_ascii_hexdigit()) => format!("{first}~1 {first}"),
            _ => first.clone(),
        },
    })
}

/// An edit request: an edit verb first (after "ok"/"now"/"also"), an edit verb within three words of a request
/// opener, or one of the edit phrases.
fn asks_edit(words: &[&str]) -> bool {
    let start = words.iter().position(|w| !["ok", "okay", "now", "also", "and", "so"].contains(w)).unwrap_or(0);
    if words.get(start).is_some_and(|w| EDIT.contains(w)) || any(words, &EDIT_PHRASES) {
        return true;
    }
    ASKS.iter().any(|ask| {
        let p: Vec<&str> = ask.split(' ').collect();
        words.windows(p.len()).enumerate().any(|(i, w)| {
            w == p.as_slice() && words[i + p.len()..].iter().take(3).any(|v| EDIT.contains(v))
        })
    })
}

/// The route a plain line reads as. `git`: the shell is in a git project (reviews and edits need one).
pub fn classify(line: &str, git: bool) -> Route {
    let lower = line.to_lowercase().replace('’', "'");
    let words = words(&lower);
    let job = words.iter().find_map(|w| w.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()));
    let first = words.first().copied().unwrap_or("");
    let edit = asks_edit(&words);
    // An explicit review opener or phrase wins over an edit; a review verb with a change or revision only when the
    // line is not an edit request ("add a security check to client.rs" is an edit)
    let asks_review = ["review", "critique", "audit"].contains(&first) || any(&words, &REVIEW_PHRASES)
        || (!edit && any(&words, &REVIEW_VERBS)
            && (any(&words, &REVIEW_OBJECTS) || words.iter().any(|w| names_revision(w))));
    if git && asks_review {
        return Route::Review(job, if job.is_some() { None } else { revision_range(line) });
    }
    if words.iter().any(|w| RESEARCH.contains(w)) || any(&words, &RESEARCH_PHRASES) {
        return Route::Research;
    }
    if words.iter().any(|w| DESIGN.contains(w)) || any(&words, &DESIGN_PHRASES) || first == "plan" {
        return Route::Design;
    }
    if git && edit {
        return Route::Edit;
    }
    let about_repo = any(&words, &REPO_PHRASES) || words.iter().any(|w| REPO_WORDS.contains(w) || names_code(w));
    let reads = READ_OPENERS.contains(&first) || any(&words, &["tell me", "walk me through", "show me", "trace it"]);
    if about_repo && (reads || words.first().is_some_and(|w| *w == "ok")) {
        return Route::Read;
    }
    Route::Ask
}

/// The command to put in the input line for a route, and what it costs, for the suggestion line.
pub fn suggestion(route: &Route, line: &str, agent: &str) -> Option<(String, &'static str)> {
    match route {
        Route::Ask => None,
        Route::Research => Some((format!("/research {line}"), "a live web search call and a write call; every cited quote is \
                                 fetched and checked by code; about 70k tokens")),
        Route::Design => Some((format!("/design {line}"), "one design call, about 2-4k tokens")),
        Route::Review(Some(n), _) => Some((format!("/review #{n}"), "one review call of that job's change")),
        Route::Review(None, Some(range)) => Some((format!("/review {range}"), "one review call of that change")),
        Route::Review(None, None) => Some(("/review HEAD~1".into(), "one review call of the last commit; edit BASE to review more")),
        Route::Edit => Some((format!("@{agent}! {line}"), "a full agent turn that edits in a worktree, 17k tokens and up")),
        Route::Read => Some((format!("@{agent} {line}"), "an agent turn that reads this folder, read-only, 17k tokens and up")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_questions_stay_questions() {
        for q in ["what is LangGraph for?", "why is the sky blue", "explain the release gate", "is this design pattern common?",
                  "how do i undo the last commit but keep the changes", "can you remind me how git rebase --onto works",
                  "whats a good regex for matching semver strings", "hello"] {
            let r = classify(q, true);
            assert!(r == Route::Ask || r == Route::Design, "{q}: {r:?}");
        }
        assert_eq!(classify("fix the retry loop", false), Route::Ask); // no project: no edit suggestion
    }

    #[test]
    fn tasks_get_their_route() {
        assert_eq!(classify("Design a cache for slugify results", true), Route::Design);
        assert_eq!(classify("how should we store the session state?", true), Route::Design);
        assert_eq!(classify("review #3 please", true), Route::Review(Some(3), None));
        assert_eq!(classify("review my last change", true), Route::Review(None, None));
        assert_eq!(classify("code review HEAD~3..HEAD pls", true), Route::Review(None, Some("HEAD~3 HEAD".into())));
        assert_eq!(classify("look over main..feature/x, is it ok?", true), Route::Review(None, Some("main feature/x".into())));
        assert_eq!(classify("check commit a3f9c21 for bugs", true), Route::Review(None, Some("a3f9c21~1 a3f9c21".into())));
        assert_eq!(classify("review HEAD~3 HEAD~1", true), Route::Review(None, Some("HEAD~3 HEAD~1".into())));
        assert_eq!(classify("review main..", true), Route::Review(None, Some("main".into())));
        assert_eq!(classify("review main..feature.", true), Route::Review(None, Some("main feature".into())));
        assert_eq!(classify("review HEAD a3f9c21", true), Route::Review(None, Some("HEAD a3f9c21".into())));
        assert_eq!(classify("review HEAD", true), Route::Review(None, None));
        assert_eq!(classify("can you double check the diff for race conditions", true), Route::Review(None, None));
        assert_eq!(classify("add a security check to client.rs", true), Route::Edit);
        let (cmd, _) = suggestion(&classify("code review HEAD~3..HEAD pls", true), "", "codex").unwrap();
        assert_eq!(cmd, "/review HEAD~3 HEAD");
        assert_eq!(classify("fix the retry loop in client.rs", true), Route::Edit);
        assert_eq!(classify("please add a test for slugify", true), Route::Edit);
        assert_eq!(classify("can you update the readme with the install steps", true), Route::Edit);
        assert_eq!(classify("the retry loop never sleeps. fix that", true), Route::Edit);
        assert_eq!(classify("find papers on context compaction for agents", false), Route::Research);
        assert_eq!(classify("what is the state of the art in code review agents?", true), Route::Research);
        assert_eq!(classify("give me sources on the JetBrains masking study", true), Route::Research);
        assert_eq!(classify("look up what changed in the latest rust release", true), Route::Research);
        assert_eq!(classify("what does this repo do?", true), Route::Read);
        assert_eq!(classify("where do we handle the session timeout", true), Route::Read);
        assert_eq!(classify("why is test_parse_args failing", true), Route::Read);
        let (cmd, _) = suggestion(&Route::Edit, "fix it", "codex").unwrap();
        assert_eq!(cmd, "@codex! fix it");
        let (cmd, _) = suggestion(&Route::Read, "what does this repo do?", "claude").unwrap();
        assert_eq!(cmd, "@claude what does this repo do?");
        assert!(suggestion(&Route::Ask, "x", "codex").is_none());
    }
}

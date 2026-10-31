//! Agent output parsers. Each turns one stdout line into notes for the panel and decides, at the end, whether the
//! turn succeeded. They fail closed: a line that is not JSON, a failed turn, a missing completion record or an
//! empty final reply makes the job fail, never succeed.

use serde_json::Value;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.cache_write + self.cache_read + self.output
    }
    pub fn add(&mut self, other: Tokens) {
        self.input += other.input;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.output += other.output;
    }
    /// What was added since `before`, a total the same conversation reported earlier.
    pub fn since(self, before: Tokens) -> Tokens {
        Tokens { input: self.input.saturating_sub(before.input), cache_write: self.cache_write.saturating_sub(before.cache_write),
                 cache_read: self.cache_read.saturating_sub(before.cache_read), output: self.output.saturating_sub(before.output) }
    }
}

/// An agent asking before it acts (Claude's `can_use_tool` control request; verified with Claude Code 2.1.316).
#[derive(Debug, Clone, PartialEq)]
pub struct Permission {
    /// The id the answer must carry.
    pub id: String,
    pub tool: String,
    /// The tool's input exactly as sent: an allow answer returns it unchanged.
    pub input: Value,
    /// What it wants to do, for the prompt: the command, the file, the address, or the connector tool and input.
    pub what: String,
    /// The permission rule an "allow for this session" answer adds, as Claude writes rules: `Bash(git status)`, a
    /// tool name.
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// The id that resumes this conversation.
    Session(String),
    /// What the agent (node None) or one of its subagents is doing now.
    Activity { node: Option<String>, text: String },
    /// A line for the job's log.
    Log(String),
    Subagent { id: String, label: String },
    SubagentEnd { id: String, ok: bool },
    Tokens(Tokens),
    /// One model call finished (Ringo pipelines report these).
    Call,
    /// The agent is editing this file (as the agent reported it, usually an absolute path).
    Touched(String),
    /// The turn cannot succeed; the runner stops the job.
    Fail(String),
    /// The agent waits for the owner's answer before it acts.
    Permission(Permission),
    /// A control request Ringo does not serve (its id): it is refused at once, so the turn never waits on it.
    Unserved(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// `codex exec --json` events.
    Codex,
    /// `claude -p --output-format stream-json --verbose` events.
    Claude,
    /// `agy --input-format stream-json --output-format stream-json` events.
    Agy,
    /// Plain text: every line is reply text.
    Plain,
    /// A Ringo trace.jsonl, read as it grows.
    Trace,
}

pub struct Parser {
    style: Style,
    reply: String,
    failure: Option<String>,
    completed: u32,
    subagents: Vec<String>,
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The first line of text, at most `n` characters.
pub fn short(text: &str, n: usize) -> String {
    let line = text.trim().lines().next().unwrap_or("");
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n.saturating_sub(1)).collect::<String>())
    } else {
        line.to_string()
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

impl Parser {
    pub fn new(style: Style) -> Parser {
        Parser { style, reply: String::new(), failure: None, completed: 0, subagents: Vec::new() }
    }

    /// The final reply text so far.
    pub fn reply(&self) -> &str {
        &self.reply
    }

    /// Whether the turn's completion record has arrived (Claude: then nothing more will be asked).
    pub fn completed(&self) -> bool {
        self.completed > 0
    }

    pub fn line(&mut self, line: &str) -> Vec<Note> {
        if line.trim().is_empty() {
            return Vec::new();
        }
        if self.style == Style::Plain {
            self.reply.push_str(line);
            self.reply.push('\n');
            return vec![Note::Log(line.to_string()), Note::Activity { node: None, text: "writing".into() }];
        }
        let event: Value = match serde_json::from_str(line) {
            Ok(v @ Value::Object(_)) => v,
            _ if self.style == Style::Trace => return Vec::new(), // a record still being written: read it later
            _ => return self.fail("a line that is not a JSON event".into()),
        };
        match self.style {
            Style::Codex => self.codex(&event),
            Style::Claude => self.claude(&event),
            Style::Agy => self.agy(&event),
            _ => self.trace(&event),
        }
    }

    fn fail(&mut self, reason: String) -> Vec<Note> {
        self.failure.get_or_insert(reason.clone());
        vec![Note::Fail(reason)]
    }

    fn codex(&mut self, e: &Value) -> Vec<Note> {
        let kind = s(e, "type");
        let item = e.get("item").cloned().unwrap_or(Value::Null);
        let item_type = s(&item, "type");
        let done = kind == "item.completed";
        let mut notes = Vec::new();
        match kind {
            "thread.started" => notes.push(Note::Session(s(e, "thread_id").to_string())),
            "turn.completed" => {
                self.completed += 1;
                let usage = e.get("usage").cloned().unwrap_or(Value::Null);
                let cached = u(&usage, "cached_input_tokens").min(u(&usage, "input_tokens"));
                notes.push(Note::Tokens(Tokens {
                    input: u(&usage, "input_tokens") - cached,
                    cache_read: cached,
                    cache_write: u(&usage, "cache_write_input_tokens"),
                    output: u(&usage, "output_tokens"),
                }));
            }
            "turn.failed" | "error" => {
                let detail = e.get("error").map(|v| v.get("message").unwrap_or(v).to_string());
                let detail = detail.unwrap_or_else(|| s(e, "message").to_string());
                return self.fail(format!("{kind}: {}", short(&detail, 200)));
            }
            _ => {}
        }
        let activity = |text: String| Note::Activity { node: None, text };
        match item_type {
            "agent_message" if done => {
                self.reply = s(&item, "text").to_string();
                notes.push(Note::Log(self.reply.clone()));
            }
            "reasoning" => notes.push(activity("thinking".into())),
            "command_execution" if !done => {
                let command = s(&item, "command");
                notes.push(activity(format!("$ {}", short(command, 80))));
                notes.push(Note::Log(format!("$ {command}")));
            }
            "command_execution" => notes.push(Note::Log(format!("  exit {}", item.get("exit_code").unwrap_or(&Value::Null)))),
            "file_change" if !done => {
                let paths: Vec<&str> = item.get("changes").and_then(Value::as_array).into_iter().flatten()
                    .map(|c| base_name(s(c, "path"))).collect();
                notes.push(activity(format!("editing {}", paths.join(", "))));
                notes.push(Note::Log(format!("edit {}", paths.join(", "))));
                for c in item.get("changes").and_then(Value::as_array).into_iter().flatten() {
                    notes.push(Note::Touched(s(c, "path").to_string()));
                }
            }
            "mcp_tool_call" if !done => notes.push(activity(format!("tool {}.{}", s(&item, "server"), s(&item, "tool")))),
            "web_search" if !done => notes.push(activity(format!("search {}", short(s(&item, "query"), 60)))),
            "todo_list" => {
                let items = item.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                let finished = items.iter().filter(|i| i.get("completed") == Some(&Value::Bool(true))).count();
                notes.push(activity(format!("plan {finished}/{} done", items.len())));
            }
            "collab_tool_call" => notes.extend(self.codex_subagents(&item)),
            "error" if done => notes.push(Note::Log(format!("warning: {}", short(s(&item, "message"), 200)))),
            _ => {}
        }
        notes
    }

    /// Codex's multi-agent tool calls: spawned agents become subagent nodes, their states update them.
    fn codex_subagents(&mut self, item: &Value) -> Vec<Note> {
        let mut notes = Vec::new();
        for id in item.get("receiver_thread_ids").and_then(Value::as_array).into_iter().flatten() {
            let id = id.as_str().unwrap_or("").to_string();
            if !id.is_empty() && !self.subagents.contains(&id) {
                self.subagents.push(id.clone());
                notes.push(Note::Subagent { id, label: short(s(item, "prompt"), 50) });
            }
        }
        if let Some(states) = item.get("agents_states").and_then(Value::as_object) {
            for (id, state) in states {
                let status = s(state, "status");
                match status {
                    "completed" | "shutdown" => notes.push(Note::SubagentEnd { id: id.clone(), ok: true }),
                    "errored" | "not_found" => notes.push(Note::SubagentEnd { id: id.clone(), ok: false }),
                    _ => notes.push(Note::Activity { node: Some(id.clone()), text: status.to_string() }),
                }
            }
        }
        notes
    }

    fn claude(&mut self, e: &Value) -> Vec<Note> {
        let parent = e.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let mut notes = Vec::new();
        match (s(e, "type"), s(e, "subtype")) {
            ("system", "init") => {
                let source = s(e, "apiKeySource");
                if !source.is_empty() && source != "none" {
                    return self.fail(format!("claude is using an API key ({source}), not the subscription login"));
                }
                notes.push(Note::Session(s(e, "session_id").to_string()));
            }
            ("assistant", _) => {
                for block in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    match s(block, "type") {
                        "text" if parent.is_none() => notes.push(Note::Log(s(block, "text").to_string())),
                        "text" => notes.push(Note::Activity { node: parent.clone(), text: short(s(block, "text"), 60) }),
                        "thinking" => notes.push(Note::Activity { node: parent.clone(), text: "thinking".into() }),
                        "tool_use" => notes.extend(self.claude_tool(block, &parent)),
                        _ => {}
                    }
                }
            }
            ("user", _) => {
                for block in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    let id = s(block, "tool_use_id");
                    if s(block, "type") == "tool_result" && self.subagents.iter().any(|a| a == id) {
                        let ok = block.get("is_error") != Some(&Value::Bool(true));
                        notes.push(Note::SubagentEnd { id: id.to_string(), ok });
                    }
                }
            }
            ("result", subtype) => {
                self.completed += 1;
                notes.push(Note::Tokens(claude_tokens(e)));
                if let Some(text) = e.get("result").and_then(Value::as_str) {
                    self.reply = text.to_string();
                }
                if e.get("is_error") == Some(&Value::Bool(true)) || subtype != "success" {
                    notes.extend(self.fail(format!("claude ended with {subtype}: {}", short(&self.reply.clone(), 200))));
                }
            }
            ("control_request", _) => {
                let (id, r) = (s(e, "request_id").to_string(), e.get("request").cloned().unwrap_or(Value::Null));
                if s(&r, "subtype") != "can_use_tool" {
                    return vec![Note::Unserved(id)];
                }
                let (tool, input) = (s(&r, "tool_name").to_string(), r.get("input").cloned().unwrap_or(Value::Null));
                let target = ["command", "file_path", "url", "path", "pattern", "query"].iter()
                    .map(|k| s(&input, k)).find(|v| !v.is_empty());
                // The whole action, every line of it: a prompt must not hide the part after a first newline.
                let name = match s(&r, "display_name") { "" => tool.as_str(), shown => shown };
                let what = match target {
                    Some(t) => format!("{name} {t}"),
                    None => format!("{name} {input}"),
                };
                // Claude's own suggestion when it makes one (Bash: the exact command); otherwise the tool itself.
                let rule = r.get("permission_suggestions").and_then(Value::as_array).into_iter().flatten()
                    .filter(|p| s(p, "type") == "addRules" && s(p, "behavior") == "allow")
                    .find_map(|p| p.pointer("/rules/0").cloned())
                    .map(|x| match s(&x, "ruleContent") { "" => s(&x, "toolName").to_string(),
                                                          content => format!("{}({content})", s(&x, "toolName")) })
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| match s(&input, "command") { "" => tool.clone(), c => format!("{tool}({c})") });
                notes.push(Note::Activity { node: None, text: format!("waiting for you: {}", short(&what, 60)) });
                notes.push(Note::Permission(Permission { id, tool, input, what, rule }));
            }
            _ => {}
        }
        notes
    }

    fn claude_tool(&mut self, block: &Value, parent: &Option<String>) -> Vec<Note> {
        let (name, input) = (s(block, "name"), block.get("input").cloned().unwrap_or(Value::Null));
        if name == "Task" || name == "Agent" {
            let id = s(block, "id").to_string();
            self.subagents.push(id.clone());
            let label = format!("{}: {}", s(&input, "subagent_type"), short(s(&input, "description"), 40));
            return vec![Note::Subagent { id, label }];
        }
        let target = ["command", "file_path", "pattern", "path", "url", "query"].iter()
            .map(|k| s(&input, k)).find(|v| !v.is_empty()).unwrap_or("");
        let target = if input.get("file_path").is_some() { base_name(target) } else { target };
        let text = format!("{name} {}", short(target, 70));
        let mut notes = vec![Note::Activity { node: parent.clone(), text: text.clone() }];
        if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
            notes.push(Note::Touched(s(&input, "file_path").to_string()));
        }
        if parent.is_none() {
            notes.push(Note::Log(text));
        }
        notes
    }

    /// agy 1.4.11 stream-json: `init`, `step_update` per model step or tool (ACTIVE, then DONE or ERROR; text arrives
    /// as `text_delta`), and one `result` with the reply and the whole turn's usage.
    fn agy(&mut self, e: &Value) -> Vec<Note> {
        let mut notes = Vec::new();
        match s(e, "event") {
            "init" => notes.push(Note::Session(s(e, "conversation_id").to_string())),
            "step_update" => {
                let step = e.get("step_update").cloned().unwrap_or(Value::Null);
                let (state, tool) = (s(&step, "state"), s(&step, "tool_name"));
                let params = step.pointer("/tool_info/parameters").cloned().unwrap_or(Value::Null);
                let target = ["Path", "TargetFile", "AbsolutePath", "CommandLine", "Query", "Url"].iter()
                    .map(|k| s(&params, k)).find(|v| !v.is_empty()).unwrap_or("");
                match (s(&step, "step_type"), state) {
                    ("tool", "ACTIVE") => {
                        let shown = if params.get("CommandLine").is_some() { target } else { base_name(target) };
                        let text = format!("{tool} {}", short(shown, 70));
                        notes.push(Note::Activity { node: None, text: text.clone() });
                        notes.push(Note::Log(text));
                        let reads = tool.starts_with("view") || tool.starts_with("read");
                        if tool.contains("file") && !reads && !target.is_empty() {
                            notes.push(Note::Touched(target.to_string()));
                        }
                    }
                    ("tool", "ERROR") => {
                        let why = step.pointer("/tool_info/error/message").and_then(Value::as_str).unwrap_or("");
                        notes.push(Note::Log(format!("  {tool} refused: {}", short(why, 160))));
                    }
                    ("agent_response", "ACTIVE") => notes.push(Note::Activity { node: None, text: "writing".into() }),
                    _ => {}
                }
            }
            "result" => {
                self.completed += 1;
                let r = e.get("result").cloned().unwrap_or(Value::Null);
                let usage = r.get("usage").cloned().unwrap_or(Value::Null);
                // Thinking is part of output_tokens (input + output = total_tokens); cache reads are taken to be part
                // of input_tokens, as Gemini counts them.
                let cached = u(&usage, "cache_read_tokens").min(u(&usage, "input_tokens"));
                notes.push(Note::Tokens(Tokens { input: u(&usage, "input_tokens") - cached, cache_read: cached,
                                                 cache_write: 0, output: u(&usage, "output_tokens") }));
                self.reply = s(&r, "response").to_string();
                let denied: Vec<&str> = r.get("denied_actions").and_then(Value::as_array).into_iter().flatten()
                    .map(|d| s(d, "action")).collect();
                if s(&r, "status") != "SUCCESS" {
                    let why = format!("agy ended with {}: {}", s(&r, "status"), short(&self.reply.clone(), 200));
                    notes.extend(self.fail(why));
                } else if self.reply.trim().is_empty() && !denied.is_empty() {
                    notes.extend(self.fail(format!("agy stopped: it needed permission for {}", denied.join(", "))));
                }
            }
            "error" => return self.fail(format!("agy: {}", short(&e.to_string(), 200))),
            _ => {}
        }
        notes
    }

    /// One Ringo trace record: a call starting, or a finished (paid) attempt with its usage.
    fn trace(&mut self, e: &Value) -> Vec<Note> {
        let step = s(e, "step");
        match s(e, "status") {
            "started" | "reserved" => vec![Note::Activity { node: None, text: format!("{step}: calling {}", s(e, "backend")) }],
            status @ ("ok" | "malformed" | "error") => {
                let usage = e.get("usage").cloned().unwrap_or(Value::Null);
                let tokens = Tokens { input: u(&usage, "input"), cache_write: u(&usage, "cache_write"),
                                      cache_read: u(&usage, "cache_read"), output: u(&usage, "output") };
                vec![Note::Call, Note::Tokens(tokens), Note::Log(format!("{step}: {status}, {} tokens", tokens.total()))]
            }
            _ => Vec::new(),
        }
    }

    /// Ok when the turn completed with a reply; the reason otherwise.
    pub fn finish(&self) -> Result<(), String> {
        if let Some(reason) = &self.failure {
            return Err(reason.clone());
        }
        match self.style {
            Style::Codex if self.completed != 1 => Err(format!("{} turn.completed events", self.completed)),
            Style::Claude | Style::Agy if self.completed != 1 => Err("no result event".into()),
            Style::Codex | Style::Claude | Style::Agy | Style::Plain if self.reply.trim().is_empty() => Err("no final reply".into()),
            _ => Ok(()),
        }
    }
}

/// Claude's tokens for the whole run: modelUsage covers every model, subagents included; usage is the fallback.
fn claude_tokens(e: &Value) -> Tokens {
    let mut t = Tokens::default();
    if let Some(models) = e.get("modelUsage").and_then(Value::as_object).filter(|m| !m.is_empty()) {
        for m in models.values() {
            t.add(Tokens { input: u(m, "inputTokens"), cache_write: u(m, "cacheCreationInputTokens"),
                           cache_read: u(m, "cacheReadInputTokens"), output: u(m, "outputTokens") });
        }
        return t;
    }
    let usage = e.get("usage").cloned().unwrap_or(Value::Null);
    Tokens { input: u(&usage, "input_tokens"), cache_write: u(&usage, "cache_creation_input_tokens"),
             cache_read: u(&usage, "cache_read_input_tokens"), output: u(&usage, "output_tokens") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(style: Style, lines: &[&str]) -> (Parser, Vec<Note>) {
        let mut p = Parser::new(style);
        let notes = lines.iter().flat_map(|l| p.line(l)).collect();
        (p, notes)
    }

    #[test]
    fn claude_permission_requests() {
        // Shapes from Claude Code 2.1.316 with --permission-prompt-tool stdio.
        let bash = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","input":{"command":"mkdir -p d && ls","description":"x"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"mkdir -p d && ls"}],"behavior":"allow","destination":"localSettings"},{"type":"setMode","mode":"acceptEdits","destination":"session"}]}}"#;
        let mcp = r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"mcp__plugin_design_atlassian__getVisibleJiraProjects","display_name":"GetVisibleJiraProjects","input":{"cloudId":"c"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"mcp__plugin_design_atlassian__getVisibleJiraProjects"}],"behavior":"allow","destination":"localSettings"}]}}"#;
        let bare = r#"{"type":"control_request","request_id":"r3","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"make"}}}"#;
        let other = r#"{"type":"control_request","request_id":"r4","request":{"subtype":"hook_callback"}}"#;
        let (_, notes) = run(Style::Claude, &[bash, mcp, bare, other]);
        let asked: Vec<&Permission> = notes.iter().filter_map(|n| match n { Note::Permission(p) => Some(p), _ => None }).collect();
        assert_eq!((asked[0].id.as_str(), asked[0].what.as_str(), asked[0].rule.as_str()), ("r1", "Bash mkdir -p d && ls", "Bash(mkdir -p d && ls)"));
        assert_eq!(asked[0].input["description"], "x", "the input goes back exactly as sent");
        assert_eq!((asked[1].what.as_str(), asked[1].rule.as_str()),
                   ("GetVisibleJiraProjects {\"cloudId\":\"c\"}", "mcp__plugin_design_atlassian__getVisibleJiraProjects"));
        assert_eq!(asked[2].rule, "Bash(make)", "no suggestion: the exact command");
        assert!(notes.contains(&Note::Unserved("r4".into())));
    }

    // Shapes taken from real codex-cli 0.164 `exec --json` output.
    const CODEX: &[&str] = &[
        r#"{"type":"thread.started","thread_id":"01a1"}"#,
        r#"{"type":"turn.started"}"#,
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'ls'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'ls'","aggregated_output":"a\n","exit_code":0,"status":"completed"}}"#,
        r#"{"type":"item.started","item":{"id":"item_2","type":"file_change","changes":[{"path":"/w/solution.py","kind":"update"}],"status":"in_progress"}}"#,
        r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Done: fixed it."}}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":38852,"cached_input_tokens":35456,"cache_write_input_tokens":0,"output_tokens":671}}"#,
    ];

    #[test]
    fn codex_turn() {
        let (p, notes) = run(Style::Codex, CODEX);
        assert_eq!(p.finish(), Ok(()));
        assert_eq!(p.reply(), "Done: fixed it.");
        assert!(notes.contains(&Note::Session("01a1".into())));
        assert!(notes.contains(&Note::Activity { node: None, text: "editing solution.py".into() }));
        assert!(notes.contains(&Note::Touched("/w/solution.py".into())));
        assert!(notes.contains(&Note::Tokens(Tokens { input: 3396, cache_read: 35456, cache_write: 0, output: 671 })));
    }

    #[test]
    fn codex_fails_closed() {
        let (p, _) = run(Style::Codex, &CODEX[..6]); // no turn.completed
        assert!(p.finish().is_err());
        let (p, notes) = run(Style::Codex, &[CODEX[0], "not json", CODEX[5], CODEX[6]]);
        assert!(p.finish().is_err() && notes.iter().any(|n| matches!(n, Note::Fail(_))));
        let (p, _) = run(Style::Codex, &[CODEX[0], r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#]);
        assert!(p.finish().unwrap_err().contains("usage limit"));
    }

    #[test]
    fn codex_subagents() {
        let spawn = r#"{"type":"item.completed","item":{"type":"collab_tool_call","tool":"spawn_agent","prompt":"audit parser","receiver_thread_ids":["t2"],"agents_states":{"t2":{"status":"running"}}}}"#;
        let end = r#"{"type":"item.completed","item":{"type":"collab_tool_call","tool":"wait","receiver_thread_ids":["t2"],"agents_states":{"t2":{"status":"completed"}}}}"#;
        let (_, notes) = run(Style::Codex, &[spawn, end]);
        assert_eq!(notes.iter().filter(|n| matches!(n, Note::Subagent { .. })).count(), 1);
        assert!(notes.contains(&Note::Subagent { id: "t2".into(), label: "audit parser".into() }));
        assert!(notes.contains(&Note::SubagentEnd { id: "t2".into(), ok: true }));
    }

    // Shapes taken from real Claude Code 2.1.312 stream-json output; the subagent lines follow its
    // parent_tool_use_id convention.
    const CLAUDE: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"6d74","apiKeySource":"none","tools":["Task","Read"]}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Task","input":{"subagent_type":"Explore","description":"find callers","prompt":"..."}}]},"parent_tool_use_id":null}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu2","name":"Grep","input":{"pattern":"fn parse"}}]},"parent_tool_use_id":"tu1"}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"found 3"}]},"parent_tool_use_id":null}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"OK"}]},"parent_tool_use_id":null}"#,
        r#"{"type":"rate_limit_event"}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"OK","session_id":"6d74","usage":{"input_tokens":2,"cache_creation_input_tokens":5181,"cache_read_input_tokens":11382,"output_tokens":4},"modelUsage":{"claude-opus-5-5":{"inputTokens":2,"outputTokens":4,"cacheReadInputTokens":11382,"cacheCreationInputTokens":5181}}}"#,
    ];

    #[test]
    fn claude_turn_with_subagent() {
        let (p, notes) = run(Style::Claude, CLAUDE);
        assert_eq!(p.finish(), Ok(()));
        assert_eq!(p.reply(), "OK");
        assert!(notes.contains(&Note::Subagent { id: "tu1".into(), label: "Explore: find callers".into() }));
        assert!(notes.contains(&Note::Activity { node: Some("tu1".into()), text: "Grep fn parse".into() }));
        assert!(notes.contains(&Note::SubagentEnd { id: "tu1".into(), ok: true }));
        assert!(notes.contains(&Note::Tokens(Tokens { input: 2, cache_write: 5181, cache_read: 11382, output: 4 })));
    }

    #[test]
    fn claude_fails_closed() {
        let key = r#"{"type":"system","subtype":"init","session_id":"x","apiKeySource":"ANTHROPIC_API_KEY"}"#;
        let (p, notes) = run(Style::Claude, &[key]);
        assert!(p.finish().unwrap_err().contains("API key"));
        assert!(matches!(notes[0], Note::Fail(_)));
        let (p, _) = run(Style::Claude, &CLAUDE[..5]); // no result
        assert!(p.finish().is_err());
        let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":""}"#;
        assert!(run(Style::Claude, &[CLAUDE[0], err]).0.finish().is_err());
    }

    // Shapes taken from real agy 1.4.11 stream-json output (paths shortened).
    const AGY: &[&str] = &[
        r#"{"event":"init","conversation_id":"bb23","init":{"cwd":"/w","tools":["view_file","edit_file"],"permission_mode":"request-review"}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"bb23","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/w/calc.py"}}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"bb23","step_index":4,"state":"ERROR","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"ls -la"},"error":{"type":"TOOL_ERROR","message":"Permission denied for unsandboxed(ls -la). Matches user-configured deny rule."}}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"bb23","step_index":6,"state":"ACTIVE","step_type":"tool","tool_name":"edit_file","tool_info":{"name":"edit_file","parameters":{"Path":"/w/calc.py"}}}}"#,
        r#"{"event":"step_update","step_update":{"conversation_id":"bb23","step_index":9,"state":"ACTIVE","step_type":"agent_response","text_delta":"Fixed"}}"#,
        r#"{"event":"result","result":{"conversation_id":"bb23","status":"SUCCESS","response":"Fixed calc.py.","duration_seconds":12.1,"num_turns":1,"usage":{"input_tokens":37365,"output_tokens":827,"thinking_tokens":481,"cache_read_tokens":0,"total_tokens":38192}}}"#,
    ];

    #[test]
    fn agy_turn() {
        let (p, notes) = run(Style::Agy, AGY);
        assert_eq!(p.finish(), Ok(()));
        assert_eq!(p.reply(), "Fixed calc.py.");
        assert!(notes.contains(&Note::Session("bb23".into())));
        assert!(notes.contains(&Note::Activity { node: None, text: "edit_file calc.py".into() }));
        assert_eq!(notes.iter().filter(|n| matches!(n, Note::Touched(_))).collect::<Vec<_>>(), [&Note::Touched("/w/calc.py".into())]);
        assert!(notes.iter().any(|n| matches!(n, Note::Log(l) if l.contains("run_command refused"))));
        assert!(notes.contains(&Note::Tokens(Tokens { input: 37365, cache_read: 0, cache_write: 0, output: 827 })));
    }

    #[test]
    fn agy_fails_closed() {
        assert!(run(Style::Agy, &AGY[..5]).0.finish().is_err()); // no result
        let denied = r#"{"event":"result","result":{"status":"SUCCESS","response":"","usage":{},"denied_actions":[{"action":"command","display_name":"RunCommand"}]}}"#;
        assert!(run(Style::Agy, &[AGY[0], denied]).0.finish().unwrap_err().contains("permission for command"));
        let failed = r#"{"event":"result","result":{"status":"ERROR","response":"quota","usage":{}}}"#;
        assert!(run(Style::Agy, &[AGY[0], failed]).0.finish().is_err());
        assert!(run(Style::Agy, &[AGY[0], "not json"]).0.finish().is_err());
    }

    #[test]
    fn trace_records() {
        let started = r#"{"step":"review","backend":"codex-chatgpt","status":"started"}"#;
        let ok = r#"{"step":"review","status":"ok","usage":{"input":2444,"cache_write":0,"cache_read":0,"output":1324}}"#;
        let (p, notes) = run(Style::Trace, &[started, ok, "{\"step\": \"half"]);
        assert_eq!(p.finish(), Ok(()));
        assert!(notes.contains(&Note::Call));
        assert!(notes.contains(&Note::Tokens(Tokens { input: 2444, cache_write: 0, cache_read: 0, output: 1324 })));
    }

    #[test]
    fn plain_needs_text() {
        assert!(run(Style::Plain, &[]).0.finish().is_err());
        assert_eq!(run(Style::Plain, &["hello"]).0.finish(), Ok(()));
    }
}

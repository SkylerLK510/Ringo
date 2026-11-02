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
    /// A line for the job's log, shown as it is.
    Log(String),
    /// The agent's own words: its reply, as markdown.
    /// A piece of the reply as it is written (Claude's text deltas, agy's); the whole Reply replaces them.
    Partial(String),
    Reply(String),
    /// A step the agent took (a tool call or a command) and, once known, what came of it.
    Step(String),
    Subagent { id: String, label: String },
    SubagentEnd { id: String, ok: bool },
    Tokens(Tokens),
    /// What a running turn has used so far, as its messages report it (Claude): shown until the turn's own count
    /// (`Tokens`) arrives, and kept if it never does (a stopped turn).
    Live(Tokens),
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
    /// Claude's `rate_limit_event` (its `rate_limit_info`): how much of the owner's plan is used, for /usage.
    Limits(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// `codex app-server` JSON-RPC messages.
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
    /// Claude's tool calls waiting for their result (tool_use id, tool, how the call reads): each is shown once,
    /// with what came of it.
    calls: Vec<(String, String, String)>,
    /// Claude's usage per API message so far (message id, usage): one message comes as several events, each with its
    /// usage, so each id counts once. The sum is 93% of the turn's own count on a real 452.9k-token turn (a message's
    /// output tokens are reported before it is written).
    seen: Vec<(String, Tokens)>,
    /// Claude reported a compaction (`/compact`), whose result may carry no text.
    compacted: bool,
    /// Codex: the turn/start request, sent once the thread's id is known; the thread; its usage so far.
    turn: Option<Value>,
    thread: String,
    usage: Value,
    /// Lines for the agent's stdin, written as soon as they are taken.
    writes: Vec<String>,
}

/// What a missing field reads as: borrowed, so no event payload is copied just to be read.
static NULL: Value = Value::Null;

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

/// How a step's text starts when it failed, and when the turn ended before its result came: the shell marks it, and
/// a job's log reads the same.
pub const FAILED: &str = "✗ ";
pub const UNFINISHED: &str = "◌ ";
/// A step whose start is all its agent reports (an agy tool, a Codex edit): shown without a claim about its outcome.
pub const STARTED: &str = "· ";

/// What a tool call came to, for its line: an error's first line; how much a search or a read found.
fn outcome(tool: &str, result: &Value, ok: bool) -> String {
    let text = match result.get("content") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(parts)) => parts.iter().map(|p| s(p, "text")).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let text = text.replace("<tool_use_error>", "").replace("</tool_use_error>", "");
    let (first, n) = (text.trim().lines().next().unwrap_or(""), text.trim().lines().count());
    let lines = |what: &str| format!(" · {n} {what}{}", if n == 1 { "" } else { "s" });
    match tool {
        _ if !ok => format!(" · {}", short(&text, 120)),
        "Grep" | "Glob" if first.starts_with("Found ") || first.starts_with("No ") => match first.split_once(" limit: ") {
            // "Found 50 files limit: 50, offset: 0": the search stopped at its limit
            Some((found, limit)) => format!(" · {found} (limit {})", limit.split(',').next().unwrap_or(limit)),
            None => format!(" · {}", short(first, 60)),
        },
        "Grep" => lines("line"),
        "Glob" => lines("file"),
        "Read" => lines("line"),
        _ => String::new(),
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

impl Parser {
    pub fn new(style: Style) -> Parser {
        Parser { style, reply: String::new(), failure: None, completed: 0, subagents: Vec::new(), calls: Vec::new(),
                 seen: Vec::new(), compacted: false, turn: None, thread: String::new(), usage: Value::Null,
                 writes: Vec::new() }
    }

    /// A Codex parser that sends `turn` (a turn/start request without its threadId) once the thread is known.
    pub fn with_turn(mut self, turn: Value) -> Parser {
        self.turn = Some(turn);
        self
    }

    pub fn style(&self) -> Style {
        self.style
    }

    /// What the agent must be sent now, one JSON message a line.
    pub fn take_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.writes)
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

    /// `codex app-server` messages (JSON-RPC over stdio): the answers to Ringo's requests (1 initialize, 2 the
    /// thread, 3 the turn), the thread's notifications, and requests of the server's own, refused at once so the turn
    /// never waits on them (none come with approvals "never"; the auto reviewer answers its own).
    fn codex(&mut self, m: &Value) -> Vec<Note> {
        let method = s(m, "method");
        if let Some(id) = m.get("id") {
            if !method.is_empty() {
                self.writes.push(serde_json::json!({"id": id, "error": {"code": -32601,
                    "message": "Ringo does not serve this request"}}).to_string());
                return vec![Note::Log(format!("refused codex's {method} request"))];
            }
            if let Some(e) = m.get("error") {
                return self.fail(format!("codex: {}", short(e.get("message").and_then(Value::as_str).unwrap_or("an error"), 200)));
            }
            if id.as_u64() == Some(2) {
                self.thread = m.pointer("/result/thread/id").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(mut turn) = self.turn.take() {
                    turn["params"]["threadId"] = self.thread.clone().into();
                    self.writes.push(turn.to_string());
                }
                return vec![Note::Session(self.thread.clone())];
            }
            return Vec::new();
        }
        let p = m.get("params").unwrap_or(&NULL);
        // The main thread's own: a subagent's thread reports through its parent's collab items.
        if !self.thread.is_empty() && p.get("threadId").and_then(Value::as_str).is_some_and(|t| t != self.thread) {
            return Vec::new();
        }
        let item = p.get("item").unwrap_or(&NULL);
        let activity = |text: String| Note::Activity { node: None, text };
        let mut notes = Vec::new();
        match (method, s(item, "type")) {
            ("item/agentMessage/delta", _) => notes.push(Note::Partial(s(p, "delta").to_string())),
            ("item/completed", "agentMessage") => {
                self.reply = s(item, "text").to_string();
                notes.push(Note::Reply(self.reply.clone()));
            }
            ("item/started", "reasoning") | ("item/reasoning/summaryTextDelta", _) => notes.push(activity("thinking".into())),
            ("item/started", "commandExecution") => notes.push(activity(format!("$ {}", short(s(item, "command"), 80)))),
            ("item/completed", "commandExecution") => {
                // One line once it ran: the command, and its exit code when it failed.
                let how = match (item.get("exitCode").and_then(Value::as_i64), s(item, "status")) {
                    (Some(0), _) => String::new(),
                    (Some(code), _) => format!(" · exit {code}"),
                    (None, "" | "completed") => String::new(),
                    (None, status) => format!(" · {status}"),
                };
                let mark = if how.is_empty() { "" } else { FAILED };
                notes.push(Note::Step(format!("{mark}$ {}{how}", short(s(item, "command"), 300))));
            }
            ("item/started" | "item/completed", "fileChange") => {
                let changes = item.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
                let names = changes.iter().map(|c| base_name(s(c, "path"))).collect::<Vec<_>>().join(", ");
                if method == "item/started" {
                    notes.push(activity(format!("editing {names}")));
                    notes.extend(changes.iter().map(|c| Note::Touched(s(c, "path").to_string())));
                } else {
                    let mark = if matches!(s(item, "status"), "failed" | "declined") { FAILED } else { "" };
                    notes.push(Note::Step(format!("{mark}edit {names}")));
                }
            }
            ("item/started", "mcpToolCall") => notes.push(activity(format!("tool {}.{}", s(item, "server"), s(item, "tool")))),
            ("item/completed", "mcpToolCall") => {
                let mark = if s(item, "status") == "failed" { FAILED } else { "" };
                notes.push(Note::Step(format!("{mark}{}.{}", s(item, "server"), s(item, "tool"))));
            }
            ("item/started", "webSearch") => notes.push(activity(format!("search {}", short(s(item, "query"), 60)))),
            ("item/completed", "webSearch") => notes.push(Note::Step(format!("search {}", short(s(item, "query"), 120)))),
            ("item/completed", "collabAgentToolCall") => notes.extend(self.codex_subagents(item)),
            ("item/completed", "contextCompaction") => notes.push(Note::Log("compacted the conversation".into())),
            ("turn/plan/updated", _) => {
                let plan = p.get("plan").and_then(Value::as_array).cloned().unwrap_or_default();
                let finished = plan.iter().filter(|step| s(step, "status") == "completed").count();
                notes.push(activity(format!("plan {finished}/{} done", plan.len())));
            }
            // The thread's usage so far; a resumed thread's counts its earlier turns too, as `codex exec` did.
            ("thread/tokenUsage/updated", _) => self.usage = p.pointer("/tokenUsage/total").cloned().unwrap_or_default(),
            ("turn/completed", _) => {
                self.completed += 1;
                let turn = p.get("turn").unwrap_or(&NULL);
                if s(turn, "status") == "failed" {
                    let why = turn.pointer("/error/message").and_then(Value::as_str).unwrap_or("the turn failed");
                    return self.fail(format!("codex: {}", short(why, 200)));
                }
                let usage = &self.usage;
                let cached = u(usage, "cachedInputTokens").min(u(usage, "inputTokens"));
                notes.push(Note::Tokens(Tokens { input: u(usage, "inputTokens") - cached, cache_read: cached,
                                                 cache_write: u(usage, "cacheWriteInputTokens"),
                                                 output: u(usage, "outputTokens") }));
            }
            ("error", _) if p.get("willRetry").and_then(Value::as_bool) == Some(true) => {
                notes.push(Note::Log(format!("retrying: {}", short(p.pointer("/error/message").and_then(Value::as_str).unwrap_or(""), 200))));
            }
            ("error", _) => {
                let why = p.pointer("/error/message").and_then(Value::as_str).unwrap_or("an error");
                return self.fail(format!("codex: {}", short(why, 200)));
            }
            ("warning" | "configWarning", _) => {
                let text = p.get("message").or_else(|| p.get("summary")).and_then(Value::as_str).unwrap_or("");
                notes.push(Note::Log(format!("warning: {}", short(text, 200))));
            }
            _ => {}
        }
        notes
    }

    /// Codex's multi-agent tool calls: spawned agents become subagent nodes, their states update them.
    fn codex_subagents(&mut self, item: &Value) -> Vec<Note> {
        let mut notes = Vec::new();
        for id in item.get("receiverThreadIds").and_then(Value::as_array).into_iter().flatten() {
            let id = id.as_str().unwrap_or("").to_string();
            if !id.is_empty() && !self.subagents.contains(&id) {
                self.subagents.push(id.clone());
                notes.push(Note::Subagent { id, label: short(s(item, "prompt"), 50) });
            }
        }
        if let Some(states) = item.get("agentsStates").and_then(Value::as_object) {
            for (id, state) in states {
                let status = s(state, "status");
                match status {
                    "completed" | "shutdown" => notes.push(Note::SubagentEnd { id: id.clone(), ok: true }),
                    "errored" | "notFound" | "interrupted" => notes.push(Note::SubagentEnd { id: id.clone(), ok: false }),
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
            ("stream_event", _) => {
                // --include-partial-messages: the reply's text as it is written. Claude Code sends these for the main
                // conversation only; each text block's whole Reply follows them.
                let event = e.get("event").unwrap_or(&NULL);
                if s(event, "type") == "content_block_delta" && event.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                    && parent.is_none() {
                    notes.push(Note::Partial(event.pointer("/delta/text").and_then(Value::as_str).unwrap_or("").to_string()));
                }
            }
            ("rate_limit_event", _) => {
                if let Some(info) = e.get("rate_limit_info").filter(|i| i.is_object()) {
                    notes.push(Note::Limits(info.clone()));
                }
            }
            ("system", "compact_boundary") => {
                let before = e.pointer("/compact_metadata/pre_tokens").and_then(Value::as_u64).unwrap_or(0);
                notes.push(Note::Log(format!("compacted the conversation (it held {before} tokens)")));
                self.compacted = true;
            }
            ("assistant", _) => {
                if let (Some(id), Some(usage)) = (e.pointer("/message/id").and_then(Value::as_str), e.pointer("/message/usage")) {
                    let t = Tokens { input: u(usage, "input_tokens"), cache_write: u(usage, "cache_creation_input_tokens"),
                                     cache_read: u(usage, "cache_read_input_tokens"), output: u(usage, "output_tokens") };
                    match self.seen.iter_mut().find(|(seen, _)| seen == id) {
                        Some((_, before)) if t.total() > before.total() => *before = t,
                        Some(_) => {}
                        None => self.seen.push((id.to_string(), t)),
                    }
                    let mut so_far = Tokens::default();
                    self.seen.iter().for_each(|(_, t)| so_far.add(*t));
                    notes.push(Note::Live(so_far));
                }
                for block in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    match s(block, "type") {
                        "text" if parent.is_none() => notes.push(Note::Reply(s(block, "text").to_string())),
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
                    if s(block, "type") != "tool_result" {
                        continue;
                    }
                    let ok = block.get("is_error") != Some(&Value::Bool(true));
                    if self.subagents.iter().any(|a| a == id) {
                        notes.push(Note::SubagentEnd { id: id.to_string(), ok });
                    } else if let Some(at) = self.calls.iter().position(|(call, ..)| call == id) {
                        let (_, tool, text) = self.calls.remove(at);
                        let mark = if ok { "" } else { FAILED };
                        notes.push(Note::Step(format!("{mark}{text}{}", outcome(&tool, block, ok))));
                    }
                }
            }
            ("result", subtype) => {
                self.completed += 1;
                // calls whose result never came (the turn stopped first) are shown as they were made
                notes.extend(self.calls.drain(..).map(|(_, _, text)| Note::Step(format!("{UNFINISHED}{text}"))));
                notes.push(Note::Tokens(claude_tokens(e)));
                if let Some(text) = e.get("result").and_then(Value::as_str) {
                    self.reply = text.to_string();
                }
                if self.compacted && self.reply.trim().is_empty() {
                    self.reply = "compacted the conversation".into(); // a compaction is done without words
                }
                if e.get("is_error") == Some(&Value::Bool(true)) || subtype != "success" {
                    notes.extend(self.fail(format!("claude ended with {subtype}: {}", short(&self.reply.clone(), 200))));
                }
            }
            ("control_request", _) => {
                let (id, r) = (s(e, "request_id").to_string(), e.get("request").unwrap_or(&NULL));
                if s(r, "subtype") != "can_use_tool" {
                    return vec![Note::Unserved(id)];
                }
                let (tool, input) = (s(r, "tool_name").to_string(), r.get("input").unwrap_or(&NULL));
                let target = ["command", "file_path", "url", "path", "pattern", "query"].iter()
                    .map(|k| s(input, k)).find(|v| !v.is_empty());
                // The whole action, every line of it: a prompt must not hide the part after a first newline.
                let name = match s(r, "display_name") { "" => tool.as_str(), shown => shown };
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
                    .unwrap_or_else(|| match s(input, "command") { "" => tool.clone(), c => format!("{tool}({c})") });
                notes.push(Note::Activity { node: None, text: format!("waiting for you: {}", short(&what, 60)) });
                notes.push(Note::Permission(Permission { id, tool, input: input.clone(), what, rule }));
            }
            _ => {}
        }
        notes
    }

    fn claude_tool(&mut self, block: &Value, parent: &Option<String>) -> Vec<Note> {
        let (name, input) = (s(block, "name"), block.get("input").unwrap_or(&NULL));
        if name == "Task" || name == "Agent" {
            let id = s(block, "id").to_string();
            self.subagents.push(id.clone());
            let label = format!("{}: {}", s(input, "subagent_type"), short(s(input, "description"), 40));
            return vec![Note::Subagent { id, label }];
        }
        let target = ["command", "file_path", "pattern", "path", "url", "query"].iter()
            .map(|k| s(input, k)).find(|v| !v.is_empty()).unwrap_or("");
        let target = if input.get("file_path").is_some() { base_name(target) } else { target };
        let mut notes = vec![Note::Activity { node: parent.clone(), text: format!("{name} {}", short(target, 70)) }];
        if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
            notes.push(Note::Touched(s(input, "file_path").to_string()));
        }
        if parent.is_none() {
            // A search names where it looked: five greps for one pattern in five folders read as five lines.
            let place = match s(input, "path") {
                "" => String::new(),
                path if !s(input, "pattern").is_empty() => format!(" in {}", base_name(path.trim_end_matches('/'))),
                _ => String::new(),
            };
            let text = match name {
                "Bash" => format!("$ {}", short(target, 300)),
                _ => format!("{name} {}{place}", short(target, 120)),
            };
            self.calls.push((s(block, "id").to_string(), name.to_string(), text));
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
                let step = e.get("step_update").unwrap_or(&NULL);
                let (state, tool) = (s(step, "state"), s(step, "tool_name"));
                let params = step.pointer("/tool_info/parameters").unwrap_or(&NULL);
                let target = ["Path", "TargetFile", "AbsolutePath", "CommandLine", "Query", "Url"].iter()
                    .map(|k| s(params, k)).find(|v| !v.is_empty()).unwrap_or("");
                match (s(step, "step_type"), state) {
                    ("tool", "ACTIVE") => {
                        let shown = if params.get("CommandLine").is_some() { target } else { base_name(target) };
                        let text = format!("{tool} {}", short(shown, 70));
                        notes.push(Note::Activity { node: None, text: text.clone() });
                        notes.push(Note::Step(format!("{STARTED}{text}")));
                        let reads = tool.starts_with("view") || tool.starts_with("read");
                        if tool.contains("file") && !reads && !target.is_empty() {
                            notes.push(Note::Touched(target.to_string()));
                        }
                    }
                    ("tool", "ERROR") => {
                        let why = step.pointer("/tool_info/error/message").and_then(Value::as_str).unwrap_or("");
                        notes.push(Note::Step(format!("{FAILED}{tool} refused: {}", short(why, 160))));
                    }
                    ("agent_response", "ACTIVE") => {
                        notes.push(Note::Activity { node: None, text: "writing".into() });
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                            notes.push(Note::Partial(delta.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            "result" => {
                self.completed += 1;
                let r = e.get("result").unwrap_or(&NULL);
                let usage = r.get("usage").unwrap_or(&NULL);
                // Thinking is part of output_tokens (input + output = total_tokens); cache reads are taken to be part
                // of input_tokens, as Gemini counts them.
                let cached = u(usage, "cache_read_tokens").min(u(usage, "input_tokens"));
                notes.push(Note::Tokens(Tokens { input: u(usage, "input_tokens") - cached, cache_read: cached,
                                                 cache_write: 0, output: u(usage, "output_tokens") }));
                self.reply = s(r, "response").to_string();
                let denied: Vec<&str> = r.get("denied_actions").and_then(Value::as_array).into_iter().flatten()
                    .map(|d| s(d, "action")).collect();
                if s(r, "status") != "SUCCESS" {
                    let why = format!("agy ended with {}: {}", s(r, "status"), short(&self.reply.clone(), 200));
                    notes.extend(self.fail(why));
                } else if self.reply.trim().is_empty() && !denied.is_empty() {
                    notes.extend(self.fail(format!("agy stopped: it needed permission for {}", denied.join(", "))));
                } else {
                    notes.push(Note::Reply(self.reply.clone()));
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
                let usage = e.get("usage").unwrap_or(&NULL);
                let tokens = Tokens { input: u(usage, "input"), cache_write: u(usage, "cache_write"),
                                      cache_read: u(usage, "cache_read"), output: u(usage, "output") };
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
            Style::Codex if self.completed != 1 => Err(format!("{} turn/completed messages", self.completed)),
            Style::Claude | Style::Agy if self.completed != 1 => Err("no result event".into()),
            Style::Codex | Style::Claude | Style::Agy | Style::Plain if self.reply.trim().is_empty() => Err("no final reply".into()),
            _ => Ok(()),
        }
    }
}

/// Claude's tokens for this run, every model and subagent included. A resumed session's `modelUsage` is the whole
/// session's so far (a second turn read 452,900 where it used 433,179: the first turn's 19,721 again), so
/// `model_usage_since_last_result`, this run's alone, comes first (Claude Code 2.1.316); `usage` is the last fallback.
fn claude_tokens(e: &Value) -> Tokens {
    let mut t = Tokens::default();
    let models = ["model_usage_since_last_result", "modelUsage"].into_iter()
        .find_map(|k| e.get(k).and_then(Value::as_object).filter(|m| !m.is_empty()));
    if let Some(models) = models {
        for m in models.values() {
            t.add(Tokens { input: u(m, "inputTokens"), cache_write: u(m, "cacheCreationInputTokens"),
                           cache_read: u(m, "cacheReadInputTokens"), output: u(m, "outputTokens") });
        }
        return t;
    }
    let usage = e.get("usage").unwrap_or(&NULL);
    Tokens { input: u(usage, "input_tokens"), cache_write: u(usage, "cache_creation_input_tokens"),
             cache_read: u(usage, "cache_read_input_tokens"), output: u(usage, "output_tokens") }
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

    // Shapes taken from real codex-cli 0.165.0 `app-server` output (ids shortened, fields trimmed).
    const CODEX: &[&str] = &[
        r#"{"id":1,"result":{"userAgent":"ringo/0.165.0"}}"#,
        r#"{"method":"configWarning","params":{"summary":"Ignored unsupported project-local config keys"}}"#,
        r#"{"id":2,"result":{"thread":{"id":"01a1"},"sandbox":{"type":"workspaceWrite"}}}"#,
        r#"{"id":3,"result":{"turn":{"id":"u1","items":[],"status":"inProgress"}}}"#,
        r#"{"method":"item/started","params":{"threadId":"01a1","turnId":"u1","item":{"type":"commandExecution","id":"c1","command":"/bin/zsh -lc 'ls'","status":"inProgress","exitCode":null}}}"#,
        r#"{"method":"item/completed","params":{"threadId":"01a1","turnId":"u1","item":{"type":"commandExecution","id":"c1","command":"/bin/zsh -lc 'ls'","status":"completed","exitCode":0}}}"#,
        r#"{"method":"item/started","params":{"threadId":"01a1","turnId":"u1","item":{"type":"fileChange","id":"f1","changes":[{"path":"/w/solution.py","kind":{"type":"update"},"diff":""}],"status":"inProgress"}}}"#,
        r#"{"method":"item/completed","params":{"threadId":"01a1","turnId":"u1","item":{"type":"fileChange","id":"f1","changes":[{"path":"/w/solution.py","kind":{"type":"update"},"diff":""}],"status":"completed"}}}"#,
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"01a1","turnId":"u1","itemId":"m1","delta":"Done: "}}"#,
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"02b2","turnId":"u9","itemId":"m9","delta":"a subagent's words"}}"#,
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"01a1","turnId":"u1","itemId":"m1","delta":"fixed it."}}"#,
        r#"{"method":"item/completed","params":{"threadId":"01a1","turnId":"u1","item":{"type":"agentMessage","id":"m1","text":"Done: fixed it.","phase":null}}}"#,
        r#"{"method":"thread/tokenUsage/updated","params":{"threadId":"01a1","turnId":"u1","tokenUsage":{"total":{"totalTokens":39523,"inputTokens":38852,"cachedInputTokens":35456,"cacheWriteInputTokens":0,"outputTokens":671,"reasoningOutputTokens":0},"last":{"totalTokens":39523,"inputTokens":38852,"cachedInputTokens":35456,"cacheWriteInputTokens":0,"outputTokens":671,"reasoningOutputTokens":0}}}}"#,
        r#"{"method":"turn/completed","params":{"threadId":"01a1","turn":{"id":"u1","items":[],"status":"completed","error":null}}}"#,
    ];

    #[test]
    fn codex_turn() {
        let mut p = Parser::new(Style::Codex).with_turn(serde_json::json!({"id": 3, "method": "turn/start",
            "params": {"input": [{"type": "text", "text": "fix it"}]}}));
        let mut notes = Vec::new();
        let mut writes = Vec::new();
        for line in CODEX {
            notes.extend(p.line(line));
            writes.extend(p.take_writes());
        }
        assert_eq!(p.finish(), Ok(()));
        assert_eq!(p.reply(), "Done: fixed it.");
        // the turn goes in once the thread is known
        assert_eq!(writes, [r#"{"id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"fix it"}],"threadId":"01a1"}}"#]);
        assert!(notes.contains(&Note::Session("01a1".into())));
        assert!(notes.contains(&Note::Activity { node: None, text: "editing solution.py".into() }));
        assert!(notes.contains(&Note::Touched("/w/solution.py".into())));
        assert!(notes.contains(&Note::Step("edit solution.py".into())));
        assert!(notes.contains(&Note::Tokens(Tokens { input: 3396, cache_read: 35456, cache_write: 0, output: 671 })));
        // the reply streams, the subagent's words stay out of it, and the whole reply replaces the pieces
        let said: Vec<&Note> = notes.iter().filter(|n| matches!(n, Note::Partial(_) | Note::Reply(_))).collect();
        assert_eq!(said, [&Note::Partial("Done: ".into()), &Note::Partial("fixed it.".into()), &Note::Reply("Done: fixed it.".into())]);
        assert!(notes.contains(&Note::Log("warning: Ignored unsupported project-local config keys".into())));
        // a command is one line once it ran; its exit code only when it failed
        assert!(notes.contains(&Note::Step("$ /bin/zsh -lc 'ls'".into())));
        let failed = r#"{"method":"item/completed","params":{"item":{"type":"commandExecution","command":"make test","exitCode":2,"status":"failed"}}}"#;
        assert_eq!(run(Style::Codex, &[failed]).1, [Note::Step("✗ $ make test · exit 2".into())]);
        // a request of the server's own is refused at once, so the turn never waits on it
        let mut p = Parser::new(Style::Codex);
        p.line(r#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"01a1"}}"#);
        assert_eq!(p.take_writes(), [r#"{"id":7,"error":{"code":-32601,"message":"Ringo does not serve this request"}}"#]);
    }

    #[test]
    fn claude_tool_calls_show_once_with_what_came_of_them() {
        let call = |id: &str, name: &str, input: &str| format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}},"parent_tool_use_id":null}}"#);
        let result = |id: &str, content: &str, error: bool| format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","content":{content},"is_error":{error}}}]}},"parent_tool_use_id":null}}"#);
        let lines = [
            call("g", "Grep", r#"{"pattern":"fn parse","path":"/w/src/"}"#),
            call("r", "Read", r#"{"file_path":"/w/src/a.rs"}"#),
            call("b", "Bash", r#"{"command":"cargo test"}"#),
            call("e", "Edit", r#"{"file_path":"/w/src/a.rs"}"#),
            result("r", r#""1\tuse x;\n2\tfn a() {}""#, false),
            result("g", r#"[{"type":"text","text":"Found 3 files\nsrc/a.rs"}]"#, false),
            result("b", r#""<tool_use_error>Permission to use Bash has been denied.</tool_use_error>""#, true),
            CLAUDE[6].to_string(),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let steps: Vec<Note> = run(Style::Claude, &lines).1.into_iter().filter(|n| matches!(n, Note::Step(_))).collect();
        assert_eq!(steps, [
            Note::Step("Read a.rs · 2 lines".into()),
            Note::Step("Grep fn parse in src · Found 3 files".into()),
            Note::Step("✗ $ cargo test · Permission to use Bash has been denied.".into()),
            Note::Step("◌ Edit a.rs".into()), // no result before the turn ended
        ]);
        let limited = [call("l", "Grep", r#"{"pattern":"x"}"#),
                       result("l", r#""Found 50 files limit: 50, offset: 0\na.rs""#, false)];
        let limited: Vec<&str> = limited.iter().map(String::as_str).collect();
        let steps: Vec<Note> = run(Style::Claude, &limited).1.into_iter().filter(|n| matches!(n, Note::Step(_))).collect();
        assert_eq!(steps, [Note::Step("Grep x · Found 50 files (limit 50)".into())]);
    }

    #[test]
    fn a_claude_turn_counts_its_tokens_as_it_goes() {
        // One API message arrives as one event per content block, each carrying the message's usage: it counts once.
        let event = |id: &str, out: u64| format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","content":[],"usage":{{"input_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":{out}}}}},"parent_tool_use_id":null}}"#);
        let lines = [event("m1", 5), event("m1", 5), event("m2", 3), event("m2", 9)];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let live: Vec<u64> = run(Style::Claude, &lines).1.into_iter()
            .filter_map(|n| if let Note::Live(t) = n { Some(t.total()) } else { None }).collect();
        assert_eq!(live, [1107, 1107, 2212, 2218]);
    }

    #[test]
    fn codex_fails_closed() {
        let (p, _) = run(Style::Codex, &CODEX[..13]); // no turn/completed
        assert!(p.finish().is_err());
        let (p, notes) = run(Style::Codex, &[CODEX[2], "not json", CODEX[11], CODEX[13]]);
        assert!(p.finish().is_err() && notes.iter().any(|n| matches!(n, Note::Fail(_))));
        let failed = r#"{"method":"turn/completed","params":{"threadId":"01a1","turn":{"id":"u1","items":[],"status":"failed","error":{"message":"usage limit"}}}}"#;
        assert!(run(Style::Codex, &[CODEX[2], failed]).0.finish().unwrap_err().contains("usage limit"));
        let refused = r#"{"id":2,"error":{"code":-32603,"message":"thread/resume failed: no rollout"}}"#;
        assert!(run(Style::Codex, &[refused]).0.finish().unwrap_err().contains("no rollout"));
        // a retried stream error is only logged; one that is not retried ends the turn
        let retry = r#"{"method":"error","params":{"threadId":"01a1","willRetry":true,"error":{"message":"stream disconnected"}}}"#;
        let (p, notes) = run(Style::Codex, &[CODEX[2], retry, CODEX[11], CODEX[13]]);
        assert_eq!(p.finish(), Ok(()));
        assert!(notes.contains(&Note::Log("retrying: stream disconnected".into())));
        let fatal = retry.replace("true", "false");
        assert!(run(Style::Codex, &[CODEX[2], &fatal]).0.finish().unwrap_err().contains("stream disconnected"));
    }

    #[test]
    fn a_resumed_claude_turn_counts_its_own_tokens_and_reports_the_plan() {
        // A second turn of one session: modelUsage is the session's so far, model_usage_since_last_result this run's.
        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{"input_tokens":24,"cache_creation_input_tokens":1,"cache_read_input_tokens":2,"output_tokens":10630},"modelUsage":{"claude-opus-5-5":{"inputTokens":26,"outputTokens":13070,"cacheReadInputTokens":3,"cacheCreationInputTokens":1}},"model_usage_since_last_result":{"claude-opus-5-5":{"inputTokens":24,"outputTokens":10630,"cacheReadInputTokens":2,"cacheCreationInputTokens":1}}}"#;
        let limits = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.21,"resetsAt":1793332800}}}}"#;
        let (_, notes) = run(Style::Claude, &[CLAUDE[0], limits, result]);
        assert!(notes.contains(&Note::Tokens(Tokens { input: 24, cache_write: 1, cache_read: 2, output: 10630 })), "{notes:?}");
        assert!(notes.iter().any(|n| matches!(n, Note::Limits(i) if i["unifiedWindows"]["five_hour"]["utilization"] == 0.21)));
        // older Claude Code without the per-run field: modelUsage as before
        let old = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","modelUsage":{"m":{"inputTokens":5,"outputTokens":6}}}"#;
        assert!(run(Style::Claude, &[CLAUDE[0], old]).1.contains(&Note::Tokens(Tokens { input: 5, output: 6, ..Tokens::default() })));
    }

    #[test]
    fn codex_subagents() {
        let spawn = r#"{"method":"item/completed","params":{"item":{"type":"collabAgentToolCall","tool":"spawnAgent","prompt":"audit parser","receiverThreadIds":["t2"],"agentsStates":{"t2":{"status":"running"}}}}}"#;
        let end = r#"{"method":"item/completed","params":{"item":{"type":"collabAgentToolCall","tool":"wait","receiverThreadIds":["t2"],"agentsStates":{"t2":{"status":"completed"}}}}}"#;
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
    fn replies_stream_as_they_are_written() {
        // Shapes from the Agent SDK's streaming docs: text deltas of the main conversation, then the whole block.
        let delta = |text: &str, parent: &str| format!(r#"{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}},"parent_tool_use_id":{parent}}}"#);
        let input = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"}}}"#;
        let whole = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"Hello"}]},"parent_tool_use_id":null}"#;
        let lines = [delta("Hel", "null"), delta("lo", "null"), input.to_string(), delta("x", "\"tu1\""), whole.to_string()];
        let notes = run(Style::Claude, &lines.iter().map(String::as_str).collect::<Vec<_>>()).1;
        let shown: Vec<&Note> = notes.iter().filter(|n| matches!(n, Note::Partial(_) | Note::Reply(_))).collect();
        assert_eq!(shown, [&Note::Partial("Hel".into()), &Note::Partial("lo".into()), &Note::Reply("Hello".into())],
                   "tool input and a subagent's text are not the reply");
        let agy = r#"{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"agent_response","text_delta":"Fix"}}"#;
        assert!(run(Style::Agy, &[agy]).1.contains(&Note::Partial("Fix".into())));
    }

    #[test]
    fn a_compaction_is_logged() {
        // The shape the Agent SDK documents for `/compact` on a resumed session.
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":41230}}"#;
        assert_eq!(run(Style::Claude, &[boundary]).1, [Note::Log("compacted the conversation (it held 41230 tokens)".into())]);
        let done = r#"{"type":"result","subtype":"success","is_error":false,"result":""}"#;
        let (p, _) = run(Style::Claude, &[boundary, done]);
        assert_eq!(p.finish(), Ok(()), "a compaction that says nothing is still done");
        let (p, _) = run(Style::Claude, &[done]);
        assert!(p.finish().is_err(), "any other turn without a reply fails closed");
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
        assert!(notes.iter().any(|n| matches!(n, Note::Step(l) if l.contains("run_command refused"))));
        assert!(notes.contains(&Note::Reply("Fixed calc.py.".into())), "the reply is in the conversation");
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

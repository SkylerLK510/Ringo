//! `/connect` and `ringo connect`: organized harness-level access control for external integrations
//! (GitHub, Git, ClickUp, Linear, Jira, Slack, Notion, Google Drive/Gmail, Figma, Datadog) and a
//! simple yes/no toggle for model internet/web access.
//!
//! Ringo acts as the harness: settings here override the underlying model CLI configs (Claude Code's
//! `--disallowed-tools` / `--strict-mcp-config`, Codex CLI's `-c web_search=...` / `network_access` /
//! `features.apps`, and Agy's tool deny rules) on every turn, with zero model calls.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde::Serialize;

use crate::theme;

/// Built-in catalog of services users commonly connect to coding agents.
#[derive(Clone, Copy, Debug)]
pub struct ConnectorSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub keywords: &'static [&'static str],
    pub summary: &'static str,
    pub setup_hint: &'static str,
}

pub const CONNECTORS: &[ConnectorSpec] = &[
    ConnectorSpec {
        id: "github",
        label: "GitHub",
        keywords: &["github", "gh"],
        summary: "PRs, issues, CI checks, and repo API via GitHub CLI / MCP",
        setup_hint: "run `ringo connect github` (launches browser device login via `gh auth login`)",
    },
    ConnectorSpec {
        id: "git",
        label: "Git",
        keywords: &["git"],
        summary: "local commits, branches, diffs, and remote push/pull",
        setup_hint: "install git (`brew install git`) and configure a remote in your repo",
    },
    ConnectorSpec {
        id: "clickup",
        label: "ClickUp",
        keywords: &["clickup"],
        summary: "tasks, lists, docs, and sprints via ClickUp MCP",
        setup_hint: "add the ClickUp MCP server in Claude Code (`claude mcp add clickup ...`) or Codex Apps",
    },
    ConnectorSpec {
        id: "linear",
        label: "Linear",
        keywords: &["linear"],
        summary: "issues, cycles, and projects via Linear MCP",
        setup_hint: "add the Linear MCP server in Claude Code (`claude mcp add linear ...`) or Codex Apps",
    },
    ConnectorSpec {
        id: "jira",
        label: "Jira / Confluence",
        keywords: &["jira", "confluence", "atlassian"],
        summary: "Atlassian issues and wiki pages via Atlassian MCP",
        setup_hint: "add the Atlassian MCP server in Claude Code or Codex Apps",
    },
    ConnectorSpec {
        id: "slack",
        label: "Slack",
        keywords: &["slack"],
        summary: "channels, threads, and messages via Slack MCP",
        setup_hint: "add the Slack MCP server in Claude Code or Codex Apps",
    },
    ConnectorSpec {
        id: "notion",
        label: "Notion",
        keywords: &["notion"],
        summary: "pages and databases via Notion MCP",
        setup_hint: "add the Notion MCP server in Claude Code or Codex Apps",
    },
    ConnectorSpec {
        id: "gdrive",
        label: "Google Workspace",
        keywords: &["gdrive", "gmail", "email", "emails", "inbox", "calendar", "google"],
        summary: "Drive, Docs, Gmail, and Calendar via Google MCP",
        setup_hint: "add a Google Workspace MCP server in Claude Code or Codex Apps",
    },
    ConnectorSpec {
        id: "figma",
        label: "Figma",
        keywords: &["figma"],
        summary: "design frames, tokens, and specs via Figma MCP",
        setup_hint: "add the Figma MCP server in Claude Code or Codex Apps",
    },
    ConnectorSpec {
        id: "datadog",
        label: "Datadog",
        keywords: &["datadog"],
        summary: "logs, metrics, and monitors via Datadog MCP",
        setup_hint: "add the Datadog MCP server in Claude Code or Codex Apps",
    },
];

pub fn find_connector(name: &str) -> Option<&'static ConnectorSpec> {
    let key = name.trim().to_ascii_lowercase();
    CONNECTORS.iter().find(|c| c.id == key || c.keywords.contains(&key.as_str()))
}

/// Parses a human yes/no answer for harness toggles like `/connect web yes|no`.
pub fn parse_yes_no(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" | "on" | "true" | "allow" | "enable" | "enabled" | "1" => Some(true),
        "no" | "n" | "off" | "false" | "deny" | "block" | "disable" | "disabled" | "0" => Some(false),
        _ => None,
    }
}

/// Persisted harness-level access policy (stored in Ringo's settings.json).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccessConfig {
    /// Whether models are allowed to search/fetch the web or use network access.
    /// Overrides underlying Claude Code / Codex CLI / Agy configs.
    pub web_access: bool,
    /// Connector IDs (e.g. "clickup", "slack") paused by the user in Ringo.
    pub paused: BTreeSet<String>,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self { web_access: true, paused: BTreeSet::new() }
    }
}

impl AccessConfig {
    pub fn from_settings(saved: &serde_json::Value) -> Self {
        let web_access = saved.get("web_access").and_then(|v| v.as_bool()).unwrap_or(true);
        let paused = saved
            .get("paused_connectors")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_ascii_lowercase()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self { web_access, paused }
    }

    pub fn load() -> Self {
        Self::from_settings(&theme::settings_all())
    }

    pub fn save_web_access(&mut self, allow: bool) -> Result<(), String> {
        self.web_access = allow;
        theme::set_setting("web_access", serde_json::json!(allow))
    }

    pub fn set_paused(&mut self, id: &str, paused: bool) -> Result<(), String> {
        let key = id.trim().to_ascii_lowercase();
        if paused {
            self.paused.insert(key);
        } else {
            self.paused.remove(&key);
        }
        let list: Vec<&String> = self.paused.iter().collect();
        theme::set_setting("paused_connectors", serde_json::json!(list))
    }

    pub fn is_paused(&self, id: &str) -> bool {
        self.paused.contains(&id.trim().to_ascii_lowercase())
    }

    /// Expands paused connector IDs to their matching `CONNECTOR_WORDS` keywords for `Session`.
    pub fn paused_keywords(&self) -> Vec<String> {
        let mut out = Vec::new();
        for id in &self.paused {
            out.push(id.clone());
            if let Some(spec) = find_connector(id) {
                for kw in spec.keywords {
                    if !out.iter().any(|x| x == kw) {
                        out.push((*kw).to_string());
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ConnectorStatus {
    pub id: String,
    pub label: String,
    pub configured: bool,
    pub allowed: bool,
    pub detail: String,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Reads local Claude Code and Codex config files once to detect configured MCP servers (zero model calls).
fn mcp_config_blob() -> String {
    let Some(home) = home_dir() else { return String::new() };
    let paths = [
        home.join(".claude.json"),
        home.join(".claude").join("settings.json"),
        home.join(".codex").join("config.toml"),
    ];
    let mut blob = String::new();
    for p in paths {
        if let Ok(text) = std::fs::read_to_string(&p) {
            blob.push_str(&text.to_ascii_lowercase());
            blob.push('\n');
        }
    }
    blob
}

fn probe_github() -> (bool, String) {
    let out = Command::new("gh")
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let user = text
                .lines()
                .find(|l| l.contains("Logged in to"))
                .map(|l| l.trim().to_string())
                .unwrap_or_else(|| "signed in via gh".into());
            (true, user)
        }
        Ok(_) => (false, "gh installed, not signed in".into()),
        Err(_) => (false, "gh CLI not installed".into()),
    }
}

fn probe_git(repo: &Path) -> (bool, String) {
    let ver = Command::new("git").arg("--version").stdin(Stdio::null()).output();
    let Ok(v) = ver else { return (false, "git not installed".into()) };
    if !v.status.success() {
        return (false, "git not runnable".into());
    }
    let remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .stdin(Stdio::null())
        .output();
    match remote {
        Ok(r) if r.status.success() => {
            let url = String::from_utf8_lossy(&r.stdout).trim().to_string();
            (true, format!("repo remote: {url}"))
        }
        _ => (true, "git ready (no origin remote in this folder)".into()),
    }
}

pub fn probe_all(config: &AccessConfig, repo: &Path) -> Vec<ConnectorStatus> {
    let mcp_blob = mcp_config_blob();
    CONNECTORS
        .iter()
        .map(|spec| {
            let (configured, detail) = match spec.id {
                "github" => probe_github(),
                "git" => probe_git(repo),
                _ => {
                    let found = spec.keywords.iter().any(|kw| mcp_blob.contains(kw));
                    if found {
                        (true, "detected in Claude/Codex MCP config".into())
                    } else {
                        (false, "not configured in Claude/Codex MCP".into())
                    }
                }
            };
            ConnectorStatus {
                id: spec.id.into(),
                label: spec.label.into(),
                configured,
                allowed: !config.is_paused(spec.id),
                detail,
            }
        })
        .collect()
}

/// Formatted dashboard for `/connect` inside the Ringo TUI shell.
pub fn shell_report(config: &AccessConfig, repo: &Path) -> String {
    let statuses = probe_all(config, repo);
    let mut lines = Vec::new();
    let web_line = if config.web_access {
        "  Internet / Web access:  YES (allowed — overrides model defaults; `/connect web no` blocks)"
    } else {
        "  Internet / Web access:  NO  (blocked — overrides Claude WebSearch/WebFetch & Codex web search; `/connect web yes` allows)"
    };
    lines.push(web_line.to_string());
    lines.push(String::new());
    lines.push("  Connected services (Ringo harness access control):".to_string());
    for s in &statuses {
        let state = match (s.configured, s.allowed) {
            (true, true) => "✓ ready   [allowed]",
            (true, false) => "⏸ ready   [paused] ",
            (false, true) => "· not set [allowed]",
            (false, false) => "· not set [paused] ",
        };
        lines.push(format!("    {:<10} {:<20} {}", s.id, state, s.detail));
    }
    lines.push(String::new());
    lines.push("  Quick commands: /connect web yes|no · /connect pause NAME · /connect allow NAME · /connect NAME".to_string());
    lines.join("\n")
}

/// Handles `/connect ...` inside the interactive TUI shell.
pub fn handle_shell(args: &str, config: &mut AccessConfig, repo: &Path) -> Result<String, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    if words.is_empty() {
        return Ok(shell_report(config, repo));
    }
    match words[0].to_ascii_lowercase().as_str() {
        "web" | "internet" | "net" => match words.get(1).copied() {
            None => {
                let cur = if config.web_access { "yes (allowed)" } else { "no (blocked)" };
                Ok(format!("  Internet / Web access: {cur} — run `/connect web yes` or `/connect web no`"))
            }
            Some(val) => {
                let allow = parse_yes_no(val)
                    .ok_or_else(|| "expected yes or no: `/connect web yes` or `/connect web no`".to_string())?;
                let saved = config.save_web_access(allow).err()
                    .map_or(" (saved)".to_string(), |e| format!(" (not saved: {e})"));
                let what = if allow {
                    "YES — models may search/fetch the web (overrides model defaults)"
                } else {
                    "NO — models are blocked from web search/fetch and sandbox network (overrides model configs)"
                };
                Ok(format!("  Internet / Web access: {what}{saved}"))
            }
        },
        "pause" | "off" | "block" | "disable" | "remove" => {
            let target = words.get(1).copied()
                .ok_or_else(|| "name a service to pause: `/connect pause clickup` (or `all`)".to_string())?;
            if target.eq_ignore_ascii_case("all") {
                for c in CONNECTORS {
                    let _ = config.set_paused(c.id, true);
                }
                return Ok("  paused all connectors — Ringo will not load MCP connectors on agent turns".into());
            }
            let spec = find_connector(target)
                .ok_or_else(|| format!("unknown service `{target}`; `/connect` lists available services"))?;
            let saved = config.set_paused(spec.id, true).err()
                .map_or(" (saved)".to_string(), |e| format!(" (not saved: {e})"));
            Ok(format!("  {} paused — Ringo will withhold {} connectors from agent turns{saved}", spec.label, spec.label))
        }
        "allow" | "on" | "enable" => {
            let target = words.get(1).copied()
                .ok_or_else(|| "name a service to allow: `/connect allow clickup` (or `all`)".to_string())?;
            if target.eq_ignore_ascii_case("all") {
                for c in CONNECTORS {
                    let _ = config.set_paused(c.id, false);
                }
                return Ok("  allowed all configured connectors (still loaded lazily only when a turn mentions them)".into());
            }
            let spec = find_connector(target)
                .ok_or_else(|| format!("unknown service `{target}`; `/connect` lists available services"))?;
            let saved = config.set_paused(spec.id, false).err()
                .map_or(" (saved)".to_string(), |e| format!(" (not saved: {e})"));
            Ok(format!("  {} allowed (loaded on demand when a turn mentions it){saved}", spec.label))
        }
        other => {
            let spec = find_connector(other)
                .ok_or_else(|| format!("unknown service `{other}`; run `/connect` to list services or `/connect web yes|no`"))?;
            let status = probe_all(config, repo).into_iter().find(|s| s.id == spec.id);
            let detail = status.as_ref().map(|s| s.detail.as_str()).unwrap_or("unknown");
            let access = if config.is_paused(spec.id) { "paused" } else { "allowed" };
            Ok(format!(
                "  {} ({}) — status: {} · access: {}\n  summary: {}\n  setup:   {}\n  control: /connect allow {} · /connect pause {}",
                spec.label, spec.id, detail, access, spec.summary, spec.setup_hint, spec.id, spec.id
            ))
        }
    }
}

fn connect_github_interactive() -> ExitCode {
    println!("Connecting GitHub via GitHub CLI browser login...");
    let status = Command::new("gh")
        .args(["auth", "login", "--hostname", "github.com", "--git-protocol", "https", "--web"])
        .status();
    match status {
        Ok(s) if s.success() => {
            let _ = Command::new("gh").args(["auth", "setup-git"]).status();
            println!("✓ GitHub connected and Git credentials configured.");
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("ringo connect github: `gh auth login` exited with status {}", s.code().unwrap_or(1));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("ringo connect github: could not run `gh`: {e} (install with `brew install gh`)");
            ExitCode::from(2)
        }
    }
}

/// Standalone `ringo connect [...]` entry point.
pub fn run_cli(args: &[OsString]) -> ExitCode {
    let strs: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let repo = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut config = AccessConfig::load();

    if strs.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "ringo connect — manage connected services and model web access (harness-level overrides)\n\n\
             Usage:\n  \
             ringo connect                  interactive menu (web access yes/no + services)\n  \
             ringo connect --json           print web access and connector status as JSON\n  \
             ringo connect web yes|no       allow or block model internet/web access\n  \
             ringo connect github           connect GitHub via guided browser login\n  \
             ringo connect allow NAME       allow agents to use a service (or `all`)\n  \
             ringo connect pause NAME       pause a service so agents cannot load it (or `all`)"
        );
        return ExitCode::SUCCESS;
    }

    if strs.iter().any(|a| a == "--json") {
        let payload = serde_json::json!({
            "web_access": config.web_access,
            "connectors": probe_all(&config, &repo),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return ExitCode::SUCCESS;
    }

    if strs.len() == 1 && strs[0].eq_ignore_ascii_case("github") {
        return connect_github_interactive();
    }

    if !strs.is_empty() {
        let joined = strs.join(" ");
        return match handle_shell(&joined, &mut config, &repo) {
            Ok(msg) => {
                println!("{msg}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ringo connect: {e}");
                ExitCode::from(2)
            }
        };
    }

    // Interactive mode when run on a terminal with no args.
    println!("{}", shell_report(&config, &repo));
    if !io::stdin().is_terminal() {
        return ExitCode::SUCCESS;
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let cur = if config.web_access { "yes" } else { "no" };
    print!("\nAllow models to access the internet/web? [y/n] (current: {cur}, Enter to keep): ");
    let _ = io::stdout().flush();
    if let Some(Ok(line)) = lines.next() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            match parse_yes_no(trimmed) {
                Some(allow) => {
                    let _ = config.save_web_access(allow);
                    println!("  Internet / Web access set to: {}", if allow { "YES" } else { "NO" });
                }
                None => println!("  Keeping current setting ({cur})."),
            }
        }
    }
    println!("\nEnter a command (e.g. `github`, `pause clickup`, `allow clickup`, `web no`, or Enter to finish):");
    loop {
        print!("connect> ");
        let _ = io::stdout().flush();
        let Some(Ok(line)) = lines.next() else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() || matches!(trimmed, "q" | "quit" | "exit" | "done") {
            break;
        }
        if trimmed.eq_ignore_ascii_case("github") {
            let _ = connect_github_interactive();
            continue;
        }
        match handle_shell(trimmed, &mut config, &repo) {
            Ok(msg) => println!("{msg}"),
            Err(e) => println!("  {e}"),
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_yes_and_no_variants_cleanly() {
        for y in ["yes", "Y", "on", "true", "allow", "1"] {
            assert_eq!(parse_yes_no(y), Some(true), "{y}");
        }
        for n in ["no", "N", "off", "false", "deny", "block", "0"] {
            assert_eq!(parse_yes_no(n), Some(false), "{n}");
        }
        assert_eq!(parse_yes_no("maybe"), None);
    }

    #[test]
    fn access_config_loads_and_expands_paused_keywords() {
        let saved = serde_json::json!({
            "web_access": false,
            "paused_connectors": ["jira", "clickup"]
        });
        let cfg = AccessConfig::from_settings(&saved);
        assert!(!cfg.web_access);
        assert!(cfg.is_paused("jira"));
        assert!(cfg.is_paused("clickup"));
        assert!(!cfg.is_paused("github"));
        let kws = cfg.paused_keywords();
        assert!(kws.contains(&"jira".to_string()));
        assert!(kws.contains(&"confluence".to_string()));
        assert!(kws.contains(&"atlassian".to_string()));
        assert!(kws.contains(&"clickup".to_string()));
    }

    #[test]
    fn find_connector_matches_id_and_aliases() {
        assert_eq!(find_connector("github").unwrap().id, "github");
        assert_eq!(find_connector("gh").unwrap().id, "github");
        assert_eq!(find_connector("ClickUp").unwrap().id, "clickup");
        assert_eq!(find_connector("confluence").unwrap().id, "jira");
        assert!(find_connector("unknown_service").is_none());
    }
}

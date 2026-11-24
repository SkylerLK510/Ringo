//! Subscription and environment health checks (`ringo doctor`, `ringo status`, `/doctor`, `/status`).
//!
//! Zero model calls: checks installed agent CLIs, their versions, subscription sign-in status,
//! environment-variable hygiene (confirming API-key / endpoint overrides are stripped to keep turns
//! on subscription billing), Python runtime version, and local git / release state.

use crate::theme::Theme;
use ringo_cli::{Adapter, Agent, SignIn};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// Environment variables that Ringo strips before launching agents so turns stay on subscription logins.
const STRIPPED_EXACT: &[&str] = &[
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE",
    "OPENAI_ACCESS_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "TYPESAFE_API_KEY",
];

/// Returns the sorted names of any API-key or endpoint-override variables currently set in `vars`.
pub fn overridden_env_vars<'a>(vars: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<String> = vars
        .into_iter()
        .filter(|k| {
            k.ends_with("API_KEY")
                || k.ends_with("AUTH_TOKEN")
                || STRIPPED_EXACT.contains(k)
        })
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Parses `Python 3.14.7` into `(3, 14, 7)`.
pub fn parse_python_version(text: &str) -> Option<(u32, u32, u32)> {
    let ver = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.'))?;
    let mut parts = ver.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0)))
}

fn python_info() -> (String, Option<(u32, u32, u32)>) {
    let bin = ringo_cli::python();
    let label = bin.to_string_lossy().into_owned();
    let out = Command::new(&bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok();
    let version = out.and_then(|o| {
        let combined = format!(
            "{} {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        parse_python_version(&combined)
    });
    (label, version)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SubscriptionCheck {
    pub name: String,
    pub company: String,
    pub configured: bool,
    pub installed: bool,
    pub version: Option<String>,
    pub signed_in: Option<bool>,
    pub sign_in_detail: String,
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_summary: Option<String>,
}

fn fix_for(agent: Agent, installed: bool, sign_in: &SignIn) -> Option<String> {
    let spec = agent.spec();
    if !installed {
        return Some(if spec.install.is_empty() {
            format!("install `{}` and add it to PATH", spec.program)
        } else {
            spec.install.clone()
        });
    }
    if matches!(sign_in, SignIn::No(_)) {
        return Some(match spec.adapter {
            Adapter::Codex => "codex login  (or run `ringo setup` / `/setup`)".into(),
            Adapter::Claude => "claude auth login  (or run `ringo setup` / `/setup`)".into(),
            _ => format!("sign in to {} and run `ringo setup`", spec.name),
        });
    }
    None
}

fn probe_one(agent: Agent, configured: bool, include_limits: bool) -> SubscriptionCheck {
    let spec = agent.spec();
    let program = PathBuf::from(&spec.program);
    let installed = ringo_cli::installed(&program);
    if !installed {
        return SubscriptionCheck {
            name: spec.name.clone(),
            company: spec.company.clone(),
            configured,
            installed: false,
            version: None,
            signed_in: Some(false),
            sign_in_detail: "not installed".into(),
            fix: fix_for(agent, false, &SignIn::Unknown),
            plan_summary: None,
        };
    }
    let version = crate::updates::agent_version(agent);
    let sign_in = ringo_cli::sign_in(agent, &program);
    let (signed_in, sign_in_detail) = match &sign_in {
        SignIn::Yes(how) => (Some(true), format!("subscription ({how})")),
        SignIn::No(why) => (Some(false), why.clone()),
        SignIn::Unknown => (None, "status probe unsupported".into()),
    };
    let fix = fix_for(agent, true, &sign_in);
    let plan_summary = if include_limits
        && signed_in == Some(true)
        && matches!(spec.adapter, Adapter::Claude | Adapter::Codex)
    {
        match ringo_cli::plan(agent, &program) {
            Ok(plan) if !plan.windows.is_empty() => {
                let parts: Vec<String> = plan
                    .windows
                    .iter()
                    .map(|w| format!("{} {:.0}%", w.name, w.percent))
                    .collect();
                let prefix = if plan.name.is_empty() {
                    String::new()
                } else {
                    format!("{} · ", plan.name)
                };
                Some(format!("{prefix}{}", parts.join(", ")))
            }
            _ => None,
        }
    } else {
        None
    };
    SubscriptionCheck {
        name: spec.name.clone(),
        company: spec.company.clone(),
        configured,
        installed: true,
        version,
        signed_in,
        sign_in_detail,
        fix,
        plan_summary,
    }
}

/// Probes every configured agent (and any installed-but-unconfigured built-in agents) in parallel.
pub fn probe_subscriptions(configured: &[Agent], include_limits: bool) -> Vec<SubscriptionCheck> {
    let mut targets: Vec<(Agent, bool)> = configured.iter().map(|&a| (a, true)).collect();
    for agent in Agent::all() {
        if !agent.spec().custom
            && !configured.contains(&agent)
            && ringo_cli::installed(&PathBuf::from(&agent.spec().program))
        {
            targets.push((agent, false));
        }
    }
    let handles: Vec<_> = targets
        .into_iter()
        .map(|(agent, is_cfg)| {
            std::thread::spawn(move || probe_one(agent, is_cfg, include_limits))
        })
        .collect();
    handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .collect()
}

/// Actionable hint when an agent turn fails due to rate limits or expired subscription authentication.
pub fn failure_hint(agent: Agent, reason: &str, configured: &[Agent]) -> Option<String> {
    let lower = reason.to_ascii_lowercase();
    let rate_limited = [
        "rate limit",
        "rate_limit",
        "429",
        "usage limit",
        "quota",
        "too many requests",
        "limit reached",
        "overloaded",
        "capacity",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if rate_limited {
        let others: Vec<String> = configured
            .iter()
            .filter(|a| **a != agent && a.spec().read)
            .map(|a| format!("@{}", a.name()))
            .collect();
        let switch = if others.is_empty() {
            "run `/usage` to check when your limit resets".into()
        } else {
            format!(
                "switch to {} to keep going in this same conversation, or run `/usage` to see reset times",
                others.join(" or ")
            )
        };
        return Some(format!(
            "  hint: {} hit a usage/rate limit — {switch}",
            agent.name()
        ));
    }
    let auth_error = [
        "not logged in",
        "not signed in",
        "unauthorized",
        "401",
        "authentication",
        "auth_error",
        "token expired",
        "login required",
        "api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if auth_error {
        let cmd = match agent.spec().adapter {
            Adapter::Codex => "codex login",
            Adapter::Claude => "claude auth login",
            _ => "/setup",
        };
        return Some(format!(
            "  hint: {}'s subscription login needs attention — run `{cmd}` (or `/setup`), then `/doctor` to verify",
            agent.name()
        ));
    }
    None
}

/// Concise multi-line report for `/doctor` and `/status` inside the TUI shell (zero model calls).
pub fn shell_report(configured: &[Agent], repo: &Path, git: bool) -> String {
    let checks = probe_subscriptions(configured, false);
    let env_keys: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    let overrides = overridden_env_vars(env_keys.iter().map(String::as_str));
    let (py_bin, py_ver) = python_info();
    let commit = &ringo_cli::COMMIT[..ringo_cli::COMMIT.len().min(7)];
    let ver = env!("CARGO_PKG_VERSION");
    let commit_tag = if commit.is_empty() { String::new() } else { format!(" ({commit})") };

    let mut out = Vec::new();
    out.push(format!("  Ringo {ver}{commit_tag} · repo {} ({})", repo.display(), if git { "git + /undo" } else { "no git" }));
    match py_ver {
        Some((maj, min, pat)) if (maj, min) >= (3, 11) => {
            out.push(format!("  Python    ✓ {py_bin} ({maj}.{min}.{pat})"));
        }
        Some((maj, min, pat)) => {
            out.push(format!(
                "  Python    ⚠ {py_bin} ({maj}.{min}.{pat}) — pipelines need 3.11+ (set RINGO_PYTHON=/opt/homebrew/bin/python3)"
            ));
        }
        None => {
            out.push(format!("  Python    ⚠ {py_bin} not found — pipelines need Python 3.11+"));
        }
    }
    if overrides.is_empty() {
        out.push("  Billing   ✓ subscription-only (no API key or endpoint overrides in environment)".into());
    } else {
        out.push(format!(
            "  Billing   ✓ subscription-only (Ringo strips {} from agent turns)",
            overrides.join(", ")
        ));
    }
    out.push("  Subscriptions:".into());
    let pad = checks.iter().map(|c| c.name.len()).max().unwrap_or(6).max(6);
    for c in &checks {
        let mark = match (c.installed, c.signed_in) {
            (true, Some(true)) => "✓",
            (true, None) => "·",
            _ => "✗",
        };
        let ver_str = c.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
        let tag = if c.configured { "" } else { " [detected, not in /setup]" };
        out.push(format!("    {mark} {:<pad$}{ver_str}  {}{tag}", c.name, c.sign_in_detail));
        if let Some(fix) = &c.fix {
            out.push(format!("      fix: {fix}"));
        }
    }
    out.join("\n")
}

const DOCTOR_USAGE: &str = "\
usage: ringo doctor|status [OPTIONS]

Inspect connected subscriptions, subscription-only billing hygiene, Python/git readiness, and release state.

Options:
  --limits      Also query live subscription plan windows (no model calls)
  --json        Emit machine-readable JSON on stdout
  -h, --help    Show this help message
";

/// Standalone `ringo doctor` / `ringo status` CLI subcommand.
pub fn run_cli(args: &[std::ffi::OsString]) -> ExitCode {
    let (mut json_out, mut include_limits) = (false, false);
    for arg in args {
        match arg.to_str() {
            Some("--json") => json_out = true,
            Some("--limits") => include_limits = true,
            Some("-h" | "--help") => {
                print!("{DOCTOR_USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("ringo doctor: unknown argument {other:?}\n\n{DOCTOR_USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let configured = ringo_cli::configured_agents().unwrap_or_default();
    let checks = probe_subscriptions(&configured, include_limits);
    let env_keys: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    let overrides = overridden_env_vars(env_keys.iter().map(String::as_str));
    let (py_bin, py_ver) = python_info();
    let py_ok = py_ver.is_some_and(|(maj, min, _)| (maj, min) >= (3, 11));
    let commit = &ringo_cli::COMMIT[..ringo_cli::COMMIT.len().min(7)];
    let auto_update = crate::theme::settings_all()
        .get("auto_update")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_problem = checks.iter().any(|c| c.configured && (!c.installed || c.signed_in == Some(false)));

    if json_out {
        let payload = serde_json::json!({
            "ringo": {
                "version": env!("CARGO_PKG_VERSION"),
                "commit": commit,
                "home": ringo_cli::ringo_home().display().to_string(),
                "auto_update": auto_update,
                "releases_active": crate::updates::releases_on(),
            },
            "python": {
                "program": py_bin,
                "version": py_ver.map(|(a, b, c)| format!("{a}.{b}.{c}")),
                "ok": py_ok,
            },
            "billing": {
                "subscription_only": true,
                "stripped_env_vars": overrides,
            },
            "subscriptions": checks,
            "ok": !has_problem && py_ok,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return if has_problem { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }

    let t = Theme::load();
    let commit_tag = if commit.is_empty() { String::new() } else { format!(" ({commit})") };
    println!(
        "{}",
        t.paint(
            t.title,
            &format!("Ringo {} {commit_tag} — Subscription & CLI Health", env!("CARGO_PKG_VERSION"))
        )
    );
    println!("  Home:       {}", ringo_cli::ringo_home().display());
    println!("  Auto-update: {}", if auto_update { "on (agent package managers)" } else { "off (`ringo update --auto on` to enable)" });
    match py_ver {
        Some((maj, min, pat)) if py_ok => {
            println!("  Python:     {} {py_bin} ({maj}.{min}.{pat})", t.paint(t.ok, "✓"));
        }
        Some((maj, min, pat)) => {
            println!(
                "  Python:     {} {py_bin} ({maj}.{min}.{pat}) — pipelines require 3.11+ (set RINGO_PYTHON=/opt/homebrew/bin/python3)",
                t.paint(t.warn, "⚠")
            );
        }
        None => {
            println!(
                "  Python:     {} {py_bin} not found — pipelines require Python 3.11+",
                t.paint(t.warn, "⚠")
            );
        }
    }
    if overrides.is_empty() {
        println!(
            "  Billing:    {} subscription-only (no API key or endpoint overrides in environment)",
            t.paint(t.ok, "✓")
        );
    } else {
        println!(
            "  Billing:    {} subscription-only (parent shell has {}; Ringo strips them so turns stay on subscription)",
            t.paint(t.ok, "✓"),
            overrides.join(", ")
        );
    }

    println!("\nConnected Subscriptions:");
    if checks.is_empty() {
        println!("  {} No agents configured yet. Run `ringo setup` to connect Claude Code or Codex.", t.paint(t.warn, "⚠"));
    } else {
        let pad = checks.iter().map(|c| c.name.len()).max().unwrap_or(6).max(6);
        for c in &checks {
            let mark = match (c.installed, c.signed_in) {
                (true, Some(true)) => t.paint(t.ok, "✓"),
                (true, None) => t.paint(t.dim, "·"),
                _ => t.paint(t.fail, "✗"),
            };
            let ver = c.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
            let tag = if c.configured { "" } else { " (detected, not yet in `ringo setup`)" };
            println!("  {mark} {:<pad$}{ver}  {}{tag}", c.name, c.sign_in_detail);
            if let Some(plan) = &c.plan_summary {
                println!("      plan: {plan}");
            }
            if let Some(fix) = &c.fix {
                println!("      fix:  {fix}");
            }
        }
    }

    if has_problem {
        println!("\nRun `ringo setup` to sign in or manage connected subscriptions.");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_api_key_and_endpoint_overrides() {
        let vars = [
            "PATH",
            "HOME",
            "OPENAI_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CUSTOM_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "TERM",
        ];
        let found = overridden_env_vars(vars);
        assert_eq!(
            found,
            vec![
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_USE_BEDROCK",
                "CUSTOM_AUTH_TOKEN",
                "OPENAI_API_KEY",
            ]
        );
    }

    #[test]
    fn parses_python_versions() {
        assert_eq!(parse_python_version("Python 3.14.7\n"), Some((3, 14, 7)));
        assert_eq!(parse_python_version("Python 3.9.6"), Some((3, 9, 6)));
        assert_eq!(parse_python_version("not python"), None);
    }

    #[test]
    fn failure_hint_suggests_peer_on_rate_limit_and_relogin_on_auth_error() {
        let claude = Agent::from_name("claude").unwrap();
        let codex = Agent::from_name("codex").unwrap();
        let both = vec![claude, codex];

        let rl = failure_hint(claude, "HTTP 429 rate limit exceeded", &both).unwrap();
        assert!(rl.contains("@codex") && rl.contains("/usage"), "{rl}");

        let auth = failure_hint(codex, "401 unauthorized: not logged in", &both).unwrap();
        assert!(auth.contains("codex login") && auth.contains("/doctor"), "{auth}");

        assert!(failure_hint(claude, "syntax error in user code", &both).is_none());
    }
}

//! Manual role assignments and honest capability discovery for the task workflow.
//!
//! Routing here is explicit and offline: nothing in this module starts a model call, switches provider, downloads a
//! model or treats an adapter's advertised features as capabilities Ringo integrated. A capability says what this
//! checkout can actually do today; an unavailable capability carries a concrete corrective action.

use crate::{Adapter, Agent, SignIn, Spec, takes_model, valid_model};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// The roles a task can assign. Manual selection only: nothing here recommends or starts an assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Worker,
    Reviewer,
    Design,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Worker, Role::Reviewer, Role::Design];

    pub fn name(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
            Self::Design => "design",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == text.trim().to_ascii_lowercase())
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Whether an assignment can run now, and what to do when it cannot. `Unavailable` is not an error to hide: it is
/// why no model call may start and the concrete action that changes that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Unavailable { reason: String, corrective: String },
}

impl Readiness {
    pub fn ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn problem(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::Unavailable { reason, corrective } => Some(format!("{reason}. {corrective}")),
        }
    }
}

/// How much of a provider's usage the shell can account for. Subscription calls report tokens but no dollar cost;
/// unknown stays unknown rather than zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageCoverage {
    Complete,
    Partial,
    Unknown,
}

impl UsageCoverage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

/// What one registered adapter can do here and now. Fields are descriptive, not scores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub adapter: String,
    pub installed: bool,
    pub sign_in: String,
    pub roles: Vec<Role>,
    pub readiness: Readiness,
    pub tools: String,
    pub edit: String,
    pub cancellation: String,
    /// None means unknown; a number would claim a limit this checkout does not know.
    pub context: Option<u64>,
    pub usage: UsageCoverage,
    pub local: bool,
}

impl Capability {
    pub fn available(&self) -> bool {
        self.readiness.ready()
    }

    /// One line for diagnostics and the owner's terminal.
    pub fn summary(&self) -> String {
        let roles = self.roles.iter().map(|role| role.name()).collect::<Vec<_>>().join(", ");
        let state = match &self.readiness {
            Readiness::Ready => "available".to_string(),
            Readiness::Unavailable { reason, corrective } => format!("unavailable: {reason} ({corrective})"),
        };
        let context = self.context.map_or("unknown".to_string(), |tokens| tokens.to_string());
        format!("{} [{}] · {state} · roles {roles} · tools {} · edit {} · cancellation {} · context {context} · usage {}",
                self.name, self.adapter, self.tools, self.edit, self.cancellation, self.usage.name())
    }
}

/// One agent's capability, from probes the caller already made. `sign_in` is the agent's own status answer (no model
/// call); `git` says whether this folder can host a worktree copy.
/// `read_mode`: the shell is in read mode, where no agent edits, so an adapter whose only turns edit its own copy
/// cannot take a worker turn until the owner changes mode.
pub fn agent_capability(
    agent: Agent, program: &Path, installed: bool, sign_in: &SignIn, git: bool, read_mode: bool,
) -> Capability {
    let spec: &Spec = agent.spec();
    let signed = match sign_in {
        SignIn::Yes(how) => format!("signed in ({how})"),
        SignIn::No(why) => format!("not signed in ({why})"),
        SignIn::Unknown => "unknown (the adapter has no status command)".to_string(),
    };
    let readiness = if !installed {
        Readiness::Unavailable {
            reason: format!("not installed (no `{}` on PATH)", program.display()),
            corrective: "install it and run /setup".into(),
        }
    } else if let SignIn::No(why) = sign_in {
        Readiness::Unavailable { reason: format!("not signed in ({why})"), corrective: sign_in_action(spec).into() }
    } else if !git && !spec.read {
        Readiness::Unavailable {
            reason: "its only edit mode is its own git worktree copy, which needs a git repository".into(),
            corrective: "start Ringo inside a git project (or run git init here)".into(),
        }
    } else if read_mode && !spec.read {
        Readiness::Unavailable {
            reason: "read mode: its only turns edit its own copy, and in read mode no agent edits".into(),
            corrective: "/mode ask (or Shift-Tab)".into(),
        }
    } else {
        Readiness::Ready
    };
    // A read-only adapter can review and design; a write-only one can only take bounded worker turns. This says what
    // the adapter can do, not that a workflow path exists yet.
    let roles = match spec.read {
        true => vec![Role::Worker, Role::Reviewer, Role::Design],
        false => vec![Role::Worker],
    };
    let edit = match (spec.read, git) {
        (true, true) => "the owner's folder with /undo, or its own git worktree copy".to_string(),
        (true, false) => "the owner's folder with /undo; no worktree copy outside a git project".to_string(),
        (false, _) => "its own git worktree copy only".to_string(),
    };
    let usage = match spec.adapter {
        // These report token counts on every call; the subscription has no per-call dollar amount.
        Adapter::Claude | Adapter::Codex => UsageCoverage::Partial,
        // No token accounting in the protocol Ringo reads.
        Adapter::Agy | Adapter::Plain => UsageCoverage::Unknown,
    };
    Capability {
        name: spec.name.clone(),
        adapter: adapter_name(spec.adapter).into(),
        installed,
        sign_in: signed,
        roles,
        readiness,
        tools: "its own (Ringo does not integrate them)".into(),
        edit,
        cancellation: "stops the turn's process group (/kill, Esc)".into(),
        context: None,
        usage,
        local: false,
    }
}

/// The standalone local worker, described from its checked-in readiness rather than assumed. It refuses live
/// generation today, so its capability is unavailable even when the package is present.
pub fn local_worker_capability(ringo_home: &Path) -> Capability {
    let present = ringo_home.join("local_worker").join("runner.py").is_file();
    let readiness = if !present {
        Readiness::Unavailable {
            reason: "the local_worker package was not found in this checkout".into(),
            corrective: "run Ringo from its own checkout".into(),
        }
    } else {
        Readiness::Unavailable {
            reason: "live generation is refused until server-side cancellation is verified against a real runtime"
                .into(),
            corrective: "use a subscription agent for now; local execution needs its own certification deliverable \
                         (docs/LOCAL-WORKER.md)"
                .into(),
        }
    };
    Capability {
        name: "local-worker".into(),
        adapter: "local-worker".into(),
        installed: present,
        sign_in: "not applicable (no provider account)".into(),
        roles: vec![Role::Worker],
        readiness,
        tools: "host-configured checks only; no shell from the model".into(),
        edit: "an isolated detached git worktree; never the owner's folder".into(),
        cancellation: "proven against a hung loopback request only; not certified for a live runtime".into(),
        context: None,
        usage: UsageCoverage::Unknown,
        local: true,
    }
}

/// Why an assignment's model is not used, when it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelOrigin {
    TaskAssignment,
    SessionModel,
    AgentDefault,
}

impl ModelOrigin {
    pub fn name(self) -> &'static str {
        match self {
            Self::TaskAssignment => "task assignment",
            Self::SessionModel => "shell /model",
            Self::AgentDefault => "agent default",
        }
    }
}

/// The assignment a role actually resolves to. Precedence is fixed and documented: the task's own model, then the
/// shell's /model choice for that agent, then the agent's default. A per-role assignment never borrows another
/// provider and never falls back to a different agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub role: Role,
    pub agent: Agent,
    pub model: Option<String>,
    pub origin: ModelOrigin,
}

pub fn effective(role: Role, agent: Agent, assigned_model: Option<&str>, session_model: Option<&str>) -> Effective {
    match assigned_model {
        Some(model) => Effective {
            role, agent, model: Some(model.to_string()), origin: ModelOrigin::TaskAssignment,
        },
        None => match session_model {
            Some(model) => Effective {
                role, agent, model: Some(model.to_string()), origin: ModelOrigin::SessionModel,
            },
            None => Effective { role, agent, model: None, origin: ModelOrigin::AgentDefault },
        },
    }
}

impl Effective {
    pub fn model_label(&self) -> String {
        self.model.clone().unwrap_or_else(|| "default".into())
    }

    pub fn summary(&self) -> String {
        format!("{}/{} ({})", self.agent.name(), self.model_label(), self.origin.name())
    }
}

/// A model named for an agent must be one that agent takes and that Ringo can pass safely. A missing model is not a
/// failure: the agent's own default applies.
pub fn model_problem(agent: Agent, model: &str) -> Option<String> {
    if !takes_model(agent) {
        return Some(format!("{} chooses its own model and takes none from Ringo", agent.name()));
    }
    if !valid_model(model) {
        return Some("a model's name is one word, as its agent takes it (for example opus or gpt-6-astra)".into());
    }
    None
}

fn sign_in_action(spec: &Spec) -> String {
    match spec.adapter {
        Adapter::Codex => "run `codex login` with your ChatGPT account".into(),
        Adapter::Claude => "run `claude auth login` with your claude.ai account".into(),
        _ => format!("sign in to {} and run /setup", spec.name),
    }
}

fn adapter_name(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::Codex => "codex",
        Adapter::Claude => "claude",
        Adapter::Agy => "agy",
        Adapter::Plain => "plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> Agent {
        Agent::from_name("claude").unwrap()
    }

    #[test]
    fn roles_parse_and_round_trip() {
        for role in Role::ALL {
            assert_eq!(Role::parse(role.name()), Some(role));
        }
        assert_eq!(Role::parse(" Worker "), Some(Role::Worker));
        assert_eq!(Role::parse("planner"), None);
    }

    #[test]
    fn not_installed_has_a_corrective_action() {
        let capability = agent_capability(claude(), Path::new("/nope/claude"), false, &SignIn::Unknown, true, false);
        assert!(!capability.available());
        let problem = capability.readiness.problem().unwrap();
        assert!(problem.contains("not installed") && problem.contains("/setup"), "{problem}");
    }

    #[test]
    fn an_api_key_login_is_unavailable_and_names_the_subscription_login() {
        let capability = agent_capability(
            claude(), Path::new("/bin/true"), true, &SignIn::No("signed in with api_key".into()), true, false,
        );
        let problem = capability.readiness.problem().unwrap();
        assert!(problem.contains("not signed in") && problem.contains("claude auth login"), "{problem}");
    }

    #[test]
    fn write_only_adapters_support_only_the_worker_role_and_need_git() {
        let capability = agent_capability(claude(), Path::new("/bin/true"), true, &SignIn::Unknown, true, false);
        assert_eq!(capability.roles, vec![Role::Worker, Role::Reviewer, Role::Design]);
        // Grok is shipped with read: false.
        let grok = Agent::from_name("grok").unwrap();
        let write_only = agent_capability(grok, Path::new("/bin/true"), true, &SignIn::Unknown, true, false);
        assert_eq!(write_only.roles, vec![Role::Worker]);
        let no_git = agent_capability(grok, Path::new("/bin/true"), true, &SignIn::Unknown, false, false);
        assert!(!no_git.available());
        assert!(no_git.readiness.problem().unwrap().contains("git repository"));
        // Read mode lets no agent edit, so a write-only adapter cannot take a worker turn; a read adapter still can.
        let read_mode = agent_capability(grok, Path::new("/bin/true"), true, &SignIn::Unknown, true, true);
        let problem = read_mode.readiness.problem().unwrap();
        assert!(problem.contains("read mode") && problem.contains("/mode ask"), "{problem}");
        assert!(agent_capability(claude(), Path::new("/bin/true"), true, &SignIn::Unknown, true, true).available());
    }

    #[test]
    fn token_reporting_is_partial_not_complete_and_plain_is_unknown() {
        let claude = agent_capability(self::claude(), Path::new("/bin/true"), true, &SignIn::Yes("claude.ai".into()), true, false);
        assert_eq!(claude.usage, UsageCoverage::Partial);
        let grok = Agent::from_name("grok").unwrap();
        let plain = agent_capability(grok, Path::new("/bin/true"), true, &SignIn::Unknown, true, false);
        assert_eq!(plain.usage, UsageCoverage::Unknown);
    }

    #[test]
    fn the_local_worker_is_never_advertised_available() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let capability = local_worker_capability(here);
        assert!(capability.local && capability.installed, "the package is in this checkout");
        assert!(!capability.available(), "live generation is not certified");
        assert!(capability.readiness.problem().unwrap().contains("cancellation"));
        let missing = local_worker_capability(Path::new("/nonexistent"));
        assert!(!missing.available());
        assert!(missing.readiness.problem().unwrap().contains("not found"));
    }

    #[test]
    fn precedence_is_task_model_then_session_model_then_default() {
        let agent = claude();
        assert_eq!(effective(Role::Worker, agent, Some("opus"), Some("sonnet")),
                   Effective { role: Role::Worker, agent, model: Some("opus".into()), origin: ModelOrigin::TaskAssignment });
        assert_eq!(effective(Role::Worker, agent, None, Some("sonnet")),
                   Effective { role: Role::Worker, agent, model: Some("sonnet".into()), origin: ModelOrigin::SessionModel });
        assert_eq!(effective(Role::Worker, agent, None, None),
                   Effective { role: Role::Worker, agent, model: None, origin: ModelOrigin::AgentDefault });
    }

    #[test]
    fn model_validation_names_the_adapter_limit() {
        let grok = Agent::from_name("grok").unwrap();
        assert!(model_problem(grok, "anything").unwrap().contains("none from Ringo"));
        assert!(model_problem(claude(), "-bad").is_some());
        assert!(model_problem(claude(), "two words").is_some());
        assert!(model_problem(claude(), "opus").is_none());
    }
}

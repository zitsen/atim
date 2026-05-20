use std::sync::Arc;

use crate::message::AgentKind;

use super::trait_def::{Agent, AgentId};
use super::AgentParser;

// ── AgentHandle ──

/// Thread-safe, cloneable handle to a boxed `Agent`.
#[derive(Clone)]
pub struct AgentHandle {
    inner: Arc<dyn Agent>,
}

impl AgentHandle {
    pub fn new(agent: impl Agent + 'static) -> Self {
        Self {
            inner: Arc::new(agent),
        }
    }

    pub fn id(&self) -> AgentId {
        self.inner.id()
    }
    pub fn name(&self) -> &'static str {
        self.inner.name()
    }
    pub fn kind(&self) -> AgentKind {
        self.inner.kind()
    }
    pub fn new_session_command(&self) -> String {
        self.inner.new_session_command()
    }
    pub fn resume_command(&self, session_id: &str) -> Option<String> {
        self.inner.resume_command(session_id)
    }
    pub fn extra_args(&self) -> Vec<String> {
        self.inner.extra_args()
    }
    pub fn required_env(&self) -> Vec<(&str, &str)> {
        self.inner.required_env()
    }
    pub fn supports_sessions(&self) -> bool {
        self.inner.supports_sessions()
    }
    pub fn has_session_start_hook(&self) -> bool {
        self.inner.has_session_start_hook()
    }
    pub fn install_hook(&self) -> crate::error::Result<()> {
        self.inner.install_hook()
    }
    pub fn output_source(&self) -> super::trait_def::OutputSource {
        self.inner.output_source()
    }
    pub fn parser(&self) -> Box<dyn AgentParser> {
        self.inner.parser()
    }
    pub fn session_discoverer(&self) -> Option<Box<dyn super::trait_def::SessionDiscoverer>> {
        self.inner.session_discoverer()
    }
    pub fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        self.inner.graceful_shutdown_keys()
    }
}

impl std::fmt::Debug for AgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHandle")
            .field("name", &self.name())
            .finish()
    }
}

// ── AgentRegistry ──

/// Read-only registry of available agents, built once from config.
///
/// All three built-in agents (Claude, Copilot, Codex) are always
/// registered so that `/switch` can resolve them by name.
#[derive(Clone)]
pub struct AgentRegistry {
    agents: Vec<AgentHandle>,
    default_name: String,
}

impl AgentRegistry {
    /// Create an empty registry (for testing).
    pub fn empty() -> Self {
        Self {
            agents: Vec::new(),
            default_name: "claude".into(),
        }
    }

    /// Register an agent.
    pub fn register(&mut self, agent: AgentHandle) {
        self.agents.push(agent);
    }

    /// Look up an agent by name.
    pub fn get(&self, name: &str) -> Option<&AgentHandle> {
        self.agents.iter().find(|a| a.name() == name)
    }

    /// Return the default agent (fallback when window state is empty).
    pub fn default(&self) -> &AgentHandle {
        self.get(&self.default_name)
            .expect("default agent not registered")
    }

    /// Set the default agent name.
    pub fn set_default(&mut self, name: &str) {
        self.default_name = name.to_string();
    }

    /// Register all built-in agents and select a default based on config.
    pub fn from_env() -> Self {
        let mut reg = Self::empty();

        // Always register all agents so `/switch` works
        reg.register(AgentHandle::new(super::claude::ClaudeAgent));
        reg.register(AgentHandle::new(super::copilot::CopilotAgent));
        reg.register(AgentHandle::new(super::codex::CodexAgent));

        // Determine default from ATIM_AGENT_COMMAND (or fallback env vars)
        let cmd = std::env::var("ATIM_AGENT_COMMAND")
            .or_else(|_| std::env::var("AGENT_COMMAND"))
            .unwrap_or_else(|_| "claude".into())
            .to_lowercase();

        let default = if cmd.contains("copilot") {
            "copilot"
        } else if cmd.contains("codex") {
            "codex"
        } else {
            "claude"
        };
        reg.default_name = default.to_string();

        reg
    }

    /// Iterate over all registered agents.
    pub fn iter(&self) -> impl Iterator<Item = &AgentHandle> {
        self.agents.iter()
    }
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("agents", &self.agents.iter().map(|a| a.name()).collect::<Vec<_>>())
            .field("default", &self.default_name)
            .finish()
    }
}

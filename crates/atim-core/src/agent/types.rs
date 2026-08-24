use std::collections::HashMap;
use std::sync::Arc;

use crate::config::AgentConfig;
use crate::message::AgentKind;

use super::AgentParser;
use super::trait_def::{Agent, AgentId};

// ── AgentHandle ──

/// Thread-safe, cloneable handle to a boxed `Agent`.
#[derive(Clone)]
pub struct AgentHandle {
    inner: Arc<dyn Agent>,
    override_command: Option<String>,
    extra_args_override: Option<Vec<String>>,
}

impl AgentHandle {
    pub fn new(agent: impl Agent + 'static) -> Self {
        Self {
            inner: Arc::new(agent),
            override_command: None,
            extra_args_override: None,
        }
    }

    pub fn with_overrides(
        agent: impl Agent + 'static,
        command: Option<String>,
        args: Option<Vec<String>>,
    ) -> Self {
        Self {
            inner: Arc::new(agent),
            override_command: command.filter(|s| !s.is_empty()),
            extra_args_override: args.filter(|v| !v.is_empty()),
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
        self.override_command
            .clone()
            .unwrap_or_else(|| self.inner.new_session_command())
    }
    pub fn resume_command(&self, session_id: &str) -> Option<String> {
        self.inner.resume_command(session_id)
    }
    pub fn extra_args(&self) -> Vec<String> {
        self.extra_args_override
            .clone()
            .unwrap_or_else(|| self.inner.extra_args())
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
    pub fn graceful_shutdown_keys(&self) -> Vec<&'static str> {
        self.inner.graceful_shutdown_keys()
    }
    pub fn discover_session(
        &self,
        cwd: &str,
        known_ids: &std::collections::HashSet<String>,
    ) -> crate::error::Result<Option<String>> {
        self.inner.discover_session(cwd, known_ids)
    }
    pub fn discover_session_by_pid(&self, window_id: &str) -> crate::error::Result<Option<String>> {
        self.inner.discover_session_by_pid(window_id)
    }
    pub fn scan_sessions(
        &self,
        path: &std::path::Path,
    ) -> crate::error::Result<Vec<crate::agent::DetectedSession>> {
        self.inner.scan_sessions(path)
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
    ///
    /// Gracefully falls back to the first registered agent if the configured
    /// default name isn't registered (e.g. a stale ATIM_DEFAULT_AGENT env
    /// value), rather than panicking and taking down the whole process.
    pub fn default(&self) -> &AgentHandle {
        self.get(&self.default_name)
            .or_else(|| self.agents.first())
            .unwrap_or_else(|| panic!("no agents registered in AgentRegistry"))
    }

    /// Set the default agent name.
    pub fn set_default(&mut self, name: &str) {
        self.default_name = name.to_string();
    }

    /// Register all built-in agents and select a default based on config.
    pub fn from_env() -> Self {
        Self::from_env_with_configs(&HashMap::new())
    }

    /// Register all built-in agents with per-agent config overrides.
    pub fn from_env_with_configs(agent_configs: &HashMap<String, AgentConfig>) -> Self {
        let mut reg = Self::empty();

        macro_rules! register_agent {
            ($agent:expr, $name:expr) => {
                if let Some(cfg) = agent_configs.get($name) {
                    reg.register(AgentHandle::with_overrides(
                        $agent,
                        Some(cfg.command.clone()).filter(|s| !s.is_empty()),
                        Some(cfg.args.clone()).filter(|v| !v.is_empty()),
                    ));
                } else {
                    reg.register(AgentHandle::new($agent));
                }
            };
        }

        // Always register all agents so `/switch` works
        register_agent!(super::claude::ClaudeAgent, "claude");
        register_agent!(super::copilot::CopilotAgent, "copilot");
        register_agent!(super::codex::CodexAgent, "codex");
        register_agent!(super::mimo::MimoAgent, "mimo");

        // Determine default from ATIM_AGENT_COMMAND (or fallback env vars)
        let cmd = std::env::var("ATIM_AGENT_COMMAND")
            .or_else(|_| std::env::var("AGENT_COMMAND"))
            .unwrap_or_else(|_| "claude".into())
            .to_lowercase();

        let default = if cmd.contains("copilot") {
            "copilot"
        } else if cmd.contains("codex") {
            "codex"
        } else if cmd.contains("mimo") {
            "mimo"
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
            .field(
                "agents",
                &self.agents.iter().map(|a| a.name()).collect::<Vec<_>>(),
            )
            .field("default", &self.default_name)
            .finish()
    }
}

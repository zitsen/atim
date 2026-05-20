use std::path::PathBuf;

use crate::agent::AgentRegistry;
use crate::error::{Error, Result};

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // ── General ──
    pub atim_dir: PathBuf,

    // ── IM (Telegram) ──
    pub telegram_bot_token: String,
    pub allowed_users: Vec<i64>,

    // ── IM (Feishu) ──
    pub feishu_app_id: String,
    pub feishu_app_secret: String,
    pub feishu_webhook_port: u16,

    // ── Tmux ──
    pub tmux_session_name: String,
    pub tmux_main_window_name: String,

    // ── Agent command ──
    pub agent_command: String,

    // ── Agent registry (built from env) ──
    pub agent_registry: AgentRegistry,

    // ── Paths ──
    pub state_file: PathBuf,
    pub session_map_file: PathBuf,
    pub monitor_state_file: PathBuf,

    // ── Monitoring ──
    pub monitor_poll_interval_secs: f64,

    // ── Display ──
    pub show_user_messages: bool,
    pub show_tool_calls: bool,
    pub show_hidden_dirs: bool,

    // ── OpenAI (voice transcription) ──
    pub openai_api_key: String,
    pub openai_base_url: String,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        let atim_dir = resolve_atim_dir();

        let telegram_bot_token = std::env::var("ATIM_TELEGRAM_TOKEN")
            .or_else(|_| std::env::var("TELEGRAM_BOT_TOKEN"))
            .unwrap_or_default();

        let allowed_users_str = std::env::var("ATIM_ALLOWED_USERS")
            .or_else(|_| std::env::var("ALLOWED_USERS"))
            .unwrap_or_default();

        let allowed_users: std::result::Result<Vec<i64>, _> = allowed_users_str
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.trim()
                    .parse::<i64>()
                    .map_err(|e| Error::Config(format!("Invalid user ID '{}': {}", s, e)))
            })
            .collect();
        let allowed_users = allowed_users?;

        let telegram_configured = !telegram_bot_token.is_empty();

        // ── Feishu ──
        let feishu_app_id = std::env::var("ATIM_FEISHU_APP_ID").unwrap_or_default();
        let feishu_app_secret = std::env::var("ATIM_FEISHU_APP_SECRET").unwrap_or_default();
        let feishu_webhook_port = std::env::var("ATIM_FEISHU_WEBHOOK_PORT")
            .unwrap_or_else(|_| "9090".into())
            .parse::<u16>()
            .unwrap_or(9090);

        let feishu_configured = !feishu_app_id.is_empty() && !feishu_app_secret.is_empty();

        // Require at least one backend
        if !telegram_configured && !feishu_configured {
            return Err(Error::Config(
                "At least one IM backend must be configured: "
                    .to_string()
                    .to_string()
                    + "ATIM_TELEGRAM_TOKEN or ATIM_FEISHU_APP_ID + ATIM_FEISHU_APP_SECRET",
            ));
        }

        let tmux_session_name =
            std::env::var("ATIM_TMUX_SESSION").unwrap_or_else(|_| "atim".into());

        let agent_command = std::env::var("ATIM_AGENT_COMMAND").unwrap_or_else(|_| "claude".into());
        let agent_registry = AgentRegistry::from_env();

        let state_file = atim_dir.join("state.json");
        let session_map_file = atim_dir.join("session_map.json");
        let monitor_state_file = atim_dir.join("monitor_state.json");

        let monitor_poll_interval = std::env::var("ATIM_MONITOR_POLL_INTERVAL")
            .or_else(|_| std::env::var("MONITOR_POLL_INTERVAL"))
            .unwrap_or_else(|_| "2.0".into())
            .parse::<f64>()
            .unwrap_or(2.0);

        let show_user_messages = std::env::var("ATIM_SHOW_USER_MESSAGES")
            .or_else(|_| std::env::var("CCBOT_SHOW_USER_MESSAGES"))
            .unwrap_or_else(|_| "true".into())
            .to_lowercase()
            != "false";

        let show_tool_calls = std::env::var("ATIM_SHOW_TOOL_CALLS")
            .or_else(|_| std::env::var("CCBOT_SHOW_TOOL_CALLS"))
            .unwrap_or_else(|_| "true".into())
            .to_lowercase()
            != "false";

        let show_hidden_dirs = std::env::var("ATIM_SHOW_HIDDEN_DIRS")
            .unwrap_or_else(|_| "false".into())
            .to_lowercase()
            == "true";

        let openai_api_key = std::env::var("ATIM_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default();

        let openai_base_url = std::env::var("ATIM_OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        Ok(Self {
            atim_dir,
            telegram_bot_token,
            allowed_users,
            feishu_app_id,
            feishu_app_secret,
            feishu_webhook_port,
            tmux_session_name,
            tmux_main_window_name: "__main__".into(),
            agent_command,
            agent_registry,
            state_file,
            session_map_file,
            monitor_state_file,
            monitor_poll_interval_secs: monitor_poll_interval,
            show_user_messages,
            show_tool_calls,
            show_hidden_dirs,
            openai_api_key,
            openai_base_url,
        })
    }

    pub fn is_user_allowed(&self, user_id: i64) -> bool {
        self.allowed_users.is_empty() || self.allowed_users.contains(&user_id)
    }

    /// Derive the agent type ("claude", "copilot", "codex") from the agent command.
    pub fn agent_type(&self) -> &'static str {
        let cmd = self.agent_command.to_lowercase();
        if cmd.contains("copilot") {
            "copilot"
        } else if cmd.contains("codex") {
            "codex"
        } else {
            "claude"
        }
    }
}

fn resolve_atim_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ATIM_DIR") {
        PathBuf::from(dir)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".atim")
    }
}

// Sensitive env vars to scrub from child processes.
pub const SENSITIVE_ENV_VARS: &[&str] = &[
    "ATIM_TELEGRAM_TOKEN",
    "TELEGRAM_BOT_TOKEN",
    "ATIM_ALLOWED_USERS",
    "ALLOWED_USERS",
    "ATIM_OPENAI_API_KEY",
    "OPENAI_API_KEY",
    "ATIM_FEISHU_APP_ID",
    "ATIM_FEISHU_APP_SECRET",
];

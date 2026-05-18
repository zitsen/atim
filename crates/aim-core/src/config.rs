use std::path::PathBuf;

use crate::error::{Error, Result};

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // ── General ──
    pub aim_dir: PathBuf,

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
        let aim_dir = resolve_aim_dir();

        let telegram_bot_token = std::env::var("AIM_TELEGRAM_TOKEN")
            .or_else(|_| std::env::var("TELEGRAM_BOT_TOKEN"))
            .unwrap_or_default();

        let allowed_users_str = std::env::var("AIM_ALLOWED_USERS")
            .or_else(|_| std::env::var("ALLOWED_USERS"))
            .unwrap_or_default();

        let allowed_users: std::result::Result<Vec<i64>, _> = allowed_users_str
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse::<i64>().map_err(|e| Error::Config(format!("Invalid user ID '{}': {}", s, e))))
            .collect();
        let allowed_users = allowed_users?;

        let telegram_configured = !telegram_bot_token.is_empty();

        // ── Feishu ──
        let feishu_app_id = std::env::var("AIM_FEISHU_APP_ID").unwrap_or_default();
        let feishu_app_secret = std::env::var("AIM_FEISHU_APP_SECRET").unwrap_or_default();
        let feishu_webhook_port = std::env::var("AIM_FEISHU_WEBHOOK_PORT")
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
                    + "AIM_TELEGRAM_TOKEN or AIM_FEISHU_APP_ID + AIM_FEISHU_APP_SECRET",
            ));
        }

        let tmux_session_name =
            std::env::var("AIM_TMUX_SESSION").unwrap_or_else(|_| "aim".into());

        let agent_command = std::env::var("AIM_AGENT_COMMAND").unwrap_or_else(|_| "claude".into());

        let state_file = aim_dir.join("state.json");
        let session_map_file = aim_dir.join("session_map.json");
        let monitor_state_file = aim_dir.join("monitor_state.json");

        let monitor_poll_interval = std::env::var("AIM_MONITOR_POLL_INTERVAL")
            .or_else(|_| std::env::var("MONITOR_POLL_INTERVAL"))
            .unwrap_or_else(|_| "2.0".into())
            .parse::<f64>()
            .unwrap_or(2.0);

        let show_user_messages = std::env::var("AIM_SHOW_USER_MESSAGES")
            .or_else(|_| std::env::var("CCBOT_SHOW_USER_MESSAGES"))
            .unwrap_or_else(|_| "true".into())
            .to_lowercase()
            != "false";

        let show_tool_calls = std::env::var("AIM_SHOW_TOOL_CALLS")
            .or_else(|_| std::env::var("CCBOT_SHOW_TOOL_CALLS"))
            .unwrap_or_else(|_| "true".into())
            .to_lowercase()
            != "false";

        let show_hidden_dirs = std::env::var("AIM_SHOW_HIDDEN_DIRS")
            .unwrap_or_else(|_| "false".into())
            .to_lowercase()
            == "true";

        let openai_api_key = std::env::var("AIM_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default();

        let openai_base_url = std::env::var("AIM_OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        Ok(Self {
            aim_dir,
            telegram_bot_token,
            allowed_users,
            feishu_app_id,
            feishu_app_secret,
            feishu_webhook_port,
            tmux_session_name,
            tmux_main_window_name: "__main__".into(),
            agent_command,
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
}

fn resolve_aim_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIM_DIR") {
        PathBuf::from(dir)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".aim")
    }
}

// Sensitive env vars to scrub from child processes.
pub const SENSITIVE_ENV_VARS: &[&str] = &[
    "AIM_TELEGRAM_TOKEN",
    "TELEGRAM_BOT_TOKEN",
    "AIM_ALLOWED_USERS",
    "ALLOWED_USERS",
    "AIM_OPENAI_API_KEY",
    "OPENAI_API_KEY",
    "AIM_FEISHU_APP_ID",
    "AIM_FEISHU_APP_SECRET",
];

use std::collections::HashMap;
use std::path::PathBuf;

use crate::agent::AgentRegistry;
use crate::error::{Error, Result};

// ── Config TOML types ──
// Canonical config.toml representation shared with atim-state for persistence.

/// Config values stored in `~/.atim/config.toml`.
/// Fields default to empty string / false when absent; env vars take precedence.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ConfigToml {
    #[serde(default)]
    pub im: ImSection,
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub tmux: TmuxSection,
    #[serde(default)]
    pub monitor: MonitorSection,
    #[serde(default)]
    pub display: DisplaySection,
    #[serde(default)]
    pub openai: OpenaiSection,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImSection {
    #[serde(default = "_default_im_backend")]
    pub backend: String,
    #[serde(default)]
    pub feishu: FeishuImSection,
    #[serde(default)]
    pub telegram: TelegramImSection,
}
fn _default_im_backend() -> String {
    "telegram".into()
}
impl Default for ImSection {
    fn default() -> Self {
        Self {
            backend: "telegram".into(),
            feishu: Default::default(),
            telegram: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FeishuImSection {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TelegramImSection {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub allowed_users: String,
}

/// Per-agent config entry (under [agent.<name>]).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentSection {
    #[serde(default, alias = "default")]
    pub default_agent: String,
    /// Starting work directory for new sessions (replaces HOME).
    #[serde(default)]
    pub workdir: String,
    /// Per-agent overrides keyed by agent name (e.g. "claude", "copilot").
    /// Populated from [agent.claude], [agent.copilot], etc. in config.toml.
    #[serde(default, flatten)]
    pub agents: HashMap<String, AgentConfig>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmuxSection {
    #[serde(default = "_default_tmux_session")]
    pub session: String,
    /// When true, keep tmux session alive after atim stops (default: false).
    #[serde(default)]
    pub keep_running: bool,
}
fn _default_tmux_session() -> String {
    "atim".into()
}
impl Default for TmuxSection {
    fn default() -> Self {
        Self {
            session: "atim".into(),
            keep_running: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MonitorSection {
    #[serde(default = "_default_poll_interval")]
    pub poll_interval: String,
}
fn _default_poll_interval() -> String {
    "2.0".into()
}
impl Default for MonitorSection {
    fn default() -> Self {
        Self {
            poll_interval: "2.0".into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplaySection {
    #[serde(default = "_default_true_str")]
    pub show_user_messages: String,
    #[serde(default = "_default_true_str")]
    pub show_tool_calls: String,
    #[serde(default)]
    pub show_hidden_dirs: bool,
}
fn _default_true_str() -> String {
    "true".into()
}
impl Default for DisplaySection {
    fn default() -> Self {
        Self {
            show_user_messages: "true".into(),
            show_tool_calls: "true".into(),
            show_hidden_dirs: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenaiSection {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "_default_openai_base_url")]
    pub base_url: String,
}
fn _default_openai_base_url() -> String {
    "https://api.openai.com/v1".into()
}
impl Default for OpenaiSection {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".into(),
        }
    }
}

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    // ── General ──
    pub atim_dir: PathBuf,
    pub im_backend: String,

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
    pub tmux_keep_running: bool,

    // ── Agent ──
    pub default_agent: String,
    pub workdir: String,

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
        let toml_cfg = load_config_toml(&atim_dir);

        let telegram_bot_token = std::env::var("ATIM_TELEGRAM_TOKEN")
            .or_else(|_| std::env::var("TELEGRAM_BOT_TOKEN"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.im.telegram.token.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();

        let allowed_users_str = std::env::var("ATIM_ALLOWED_USERS")
            .or_else(|_| std::env::var("ALLOWED_USERS"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.im.telegram.allowed_users.clone())
                    .filter(|s| !s.is_empty())
            })
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
        let im_backend = std::env::var("ATIM_IM_BACKEND")
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.im.backend.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "telegram".into());

        // ── Feishu ──
        let feishu_app_id = std::env::var("ATIM_FEISHU_APP_ID")
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.im.feishu.app_id.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
        let feishu_app_secret = std::env::var("ATIM_FEISHU_APP_SECRET")
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.im.feishu.app_secret.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
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

        let tmux_session_name = std::env::var("ATIM_TMUX_SESSION")
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.tmux.session.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "atim".into());

        let default_agent = std::env::var("ATIM_DEFAULT_AGENT")
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.agent.default_agent.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
        let workdir = toml_cfg
            .as_ref()
            .map(|c| c.agent.workdir.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let agent_configs = toml_cfg
            .as_ref()
            .map(|c| c.agent.agents.clone())
            .unwrap_or_default();
        let agent_registry = AgentRegistry::from_env_with_configs(&agent_configs);

        // Backward-compat fields now point to the new canonical files.
        let state_file = atim_dir.join("store.db");
        let session_map_file = atim_dir.join("store.db");
        let monitor_state_file = atim_dir.join("store.db");

        let monitor_poll_interval = std::env::var("ATIM_MONITOR_POLL_INTERVAL")
            .or_else(|_| std::env::var("MONITOR_POLL_INTERVAL"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.monitor.poll_interval.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "2.0".into())
            .parse::<f64>()
            .unwrap_or(2.0);

        let show_user_messages = std::env::var("ATIM_SHOW_USER_MESSAGES")
            .or_else(|_| std::env::var("CCBOT_SHOW_USER_MESSAGES"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.display.show_user_messages.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "true".into())
            .to_lowercase()
            != "false";

        let show_tool_calls = std::env::var("ATIM_SHOW_TOOL_CALLS")
            .or_else(|_| std::env::var("CCBOT_SHOW_TOOL_CALLS"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.display.show_tool_calls.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "true".into())
            .to_lowercase()
            != "false";
        let show_hidden_dirs = std::env::var("ATIM_SHOW_HIDDEN_DIRS")
            .ok()
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.display.show_hidden_dirs)
                    .unwrap_or(false)
            });

        let openai_api_key = std::env::var("ATIM_OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.openai.api_key.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();

        let openai_base_url = std::env::var("ATIM_OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .ok()
            .or_else(|| {
                toml_cfg
                    .as_ref()
                    .map(|c| c.openai.base_url.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        Ok(Self {
            atim_dir,
            im_backend,
            telegram_bot_token,
            allowed_users,
            feishu_app_id,
            feishu_app_secret,
            feishu_webhook_port,
            tmux_session_name,
            tmux_main_window_name: "__main__".into(),
            tmux_keep_running: toml_cfg
                .as_ref()
                .map(|c| c.tmux.keep_running)
                .unwrap_or(false),
            default_agent,
            workdir,
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

    /// Resolve the starting work directory for new sessions.
    /// Returns the configured workdir if set, otherwise the process cwd.
    pub fn start_path(&self) -> std::path::PathBuf {
        if !self.workdir.is_empty() {
            std::path::PathBuf::from(&self.workdir)
        } else {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        }
    }
}

fn load_config_toml(atim_dir: &std::path::Path) -> Option<ConfigToml> {
    let path = atim_dir.join("config.toml");
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<ConfigToml>(&data) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            None
        }
    }
}

fn resolve_atim_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ATIM_DIR") {
        PathBuf::from(dir)
    } else {
        // home::home_dir() handles HOME (Unix) and USERPROFILE (Windows).
        let home = home::home_dir().unwrap_or_else(std::env::temp_dir);
        home.join(".atim")
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

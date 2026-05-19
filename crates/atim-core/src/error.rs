use thiserror::Error;

/// Unified error type for all Aim operations.
#[derive(Error, Debug)]
pub enum Error {
    // ── IM layer ──
    #[error("IM adapter error: {0}")]
    Im(String),

    #[error("Telegram API error: {0}")]
    Telegram(String),

    #[error("Feishu API error: {0}")]
    Feishu(String),

    // ── Tmux layer ──
    #[error("Tmux error: {0}")]
    Tmux(String),

    #[error("Window not found: {0}")]
    WindowNotFound(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    // ── Parser layer ──
    #[error("Parse error: {0}")]
    Parse(String),

    #[error("JSON decode error: {0}")]
    Json(#[from] serde_json::Error),

    // ── IO / persistence ──
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("State error: {0}")]
    State(String),

    // ── Config ──
    #[error("Configuration error: {0}")]
    Config(String),

    // ── General ──
    #[error("Agent error: {0}")]
    Agent(String),

    #[error("Font error: {0}")]
    Font(String),

    #[error("PNG encoding error: {0}")]
    PngEncoding(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Already exists: {0}")]
    AlreadyExists(String),

    #[error("Unsupported: {0}")]
    Unsupported(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

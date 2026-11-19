//! Read and answer OpenCode v2 "forms" (the AskUserQuestion prompt).
//!
//! OpenCode keeps pending questions in its running HTTP service, not in the
//! SQLite store, so atim asks the service for them and submits the answer
//! through the same API. The service URL comes from `opencode service status`
//! and its basic-auth password from `~/.config/opencode/service.json`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FormField {
    pub key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub options: Vec<FormOption>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FormInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub fields: Vec<FormField>,
}

impl FormInfo {
    /// The `question` tool sets metadata.kind = "question". Other forms
    /// (permission prompts) must not be answered as a question.
    pub fn is_question(&self) -> bool {
        self.metadata
            .as_ref()
            .and_then(|m| m.get("kind"))
            .and_then(|k| k.as_str())
            == Some("question")
    }
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    data: Vec<FormInfo>,
}

#[derive(Serialize)]
struct ReplyBody {
    answer: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Deserialize)]
struct PermissionListResponse {
    #[serde(default)]
    data: Vec<PermissionRequest>,
}

#[derive(Serialize)]
struct PermissionReplyBody {
    decision: String,
    message: Option<String>,
}

static ENDPOINT: tokio::sync::Mutex<Option<(String, String, Instant)>> =
    tokio::sync::Mutex::const_new(None);

/// Resolve the running OpenCode service URL and basic-auth password.
async fn endpoint() -> Option<(String, String)> {
    {
        let guard = ENDPOINT.lock().await;
        if let Some((url, pw, at)) = guard.as_ref()
            && at.elapsed() < Duration::from_secs(60)
        {
            return Some((url.clone(), pw.clone()));
        }
    }

    let home = home::home_dir()?;
    let raw = tokio::fs::read_to_string(home.join(".config/opencode/service.json"))
        .await
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let password = value.get("password")?.as_str()?.to_string();

    let output = tokio::process::Command::new("opencode")
        .args(["service", "status"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("http"))?
        .trim_end_matches('/')
        .to_string();

    *ENDPOINT.lock().await = Some((url.clone(), password.clone(), Instant::now()));
    Some((url, password))
}

async fn invalidate_endpoint() {
    *ENDPOINT.lock().await = None;
}

/// Pending forms for a session. Returns empty on any transport error.
pub async fn list_forms(session_id: &str) -> Vec<FormInfo> {
    let Some((url, password)) = endpoint().await else {
        return Vec::new();
    };
    let client = reqwest::Client::new();
    let request = client
        .get(format!("{url}/api/session/{session_id}/form"))
        .basic_auth("opencode", Some(&password));
    let response = match request.send().await {
        Ok(r) => r,
        Err(_) => {
            invalidate_endpoint().await;
            return Vec::new();
        }
    };
    if response.status().as_u16() == 401 {
        invalidate_endpoint().await;
        return Vec::new();
    }
    match response.json::<ListResponse>().await {
        Ok(body) => body.data,
        Err(_) => Vec::new(),
    }
}

/// Submit one answer for one field.
pub async fn reply_form(
    session_id: &str,
    form_id: &str,
    field_key: &str,
    value: &str,
) -> Result<(), String> {
    let (url, password) = endpoint()
        .await
        .ok_or_else(|| "OpenCode service is not reachable".to_string())?;
    let body = ReplyBody {
        answer: HashMap::from([(field_key.to_string(), value.to_string())]),
    };
    let response = reqwest::Client::new()
        .post(format!(
            "{url}/api/session/{session_id}/form/{form_id}/reply"
        ))
        .basic_auth("opencode", Some(&password))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("reply request failed: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().as_u16() == 401 {
        invalidate_endpoint().await;
        Err("OpenCode service rejected the password".to_string())
    } else {
        Err(format!(
            "reply rejected: HTTP {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ))
    }
}

/// Pending permission requests across all bound projects.
pub async fn list_permissions() -> Vec<PermissionRequest> {
    let Some((url, password)) = endpoint().await else {
        return Vec::new();
    };
    let response = reqwest::Client::new()
        .get(format!("{url}/api/permission/request"))
        .basic_auth("opencode", Some(&password))
        .send()
        .await;
    let response = match response {
        Ok(r) => r,
        Err(_) => {
            invalidate_endpoint().await;
            return Vec::new();
        }
    };
    if response.status().as_u16() == 401 {
        invalidate_endpoint().await;
        return Vec::new();
    }
    response
        .json::<PermissionListResponse>()
        .await
        .map(|body| body.data)
        .unwrap_or_default()
}

/// Respond to a pending permission request.
pub async fn reply_permission(
    session_id: &str,
    request_id: &str,
    decision: &str,
) -> Result<(), String> {
    let (url, password) = endpoint()
        .await
        .ok_or_else(|| "OpenCode service is not reachable".to_string())?;
    let body = PermissionReplyBody {
        decision: decision.to_string(),
        message: None,
    };
    let response = reqwest::Client::new()
        .post(format!(
            "{url}/api/session/{session_id}/permission/{request_id}/reply"
        ))
        .basic_auth("opencode", Some(&password))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("permission reply failed: {e}"))?;
    if response.status().is_success() {
        Ok(())
    } else if response.status().as_u16() == 401 {
        invalidate_endpoint().await;
        Err("OpenCode service rejected the password".to_string())
    } else {
        Err(format!(
            "permission reply rejected: HTTP {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ))
    }
}

/// Map a short chat reply to an OpenCode permission decision.
pub fn permission_decision(text: &str) -> Option<&'static str> {
    match text.trim().to_lowercase().as_str() {
        "1" | "once" | "allow" | "允许" | "同意" | "好" => Some("once"),
        "2" | "always" | "总是" | "一直允许" | "始终允许" => Some("always"),
        "3" | "reject" | "deny" | "拒绝" | "不允许" => Some("reject"),
        _ => None,
    }
}

/// Human-readable permission prompt for an IM message.
pub fn format_permission(request: &PermissionRequest) -> String {
    let mut text = format!("🔐 **opencode 请求权限：{}**\n", request.action);
    if let Some(message) = request.message.as_deref().filter(|m| !m.is_empty()) {
        text.push_str(message);
        text.push('\n');
    }
    if !request.resources.is_empty() {
        text.push_str("资源：\n");
        for resource in &request.resources {
            text.push_str(&format!("- `{resource}`\n"));
        }
    }
    text.push_str("\n回复数字：1 允许一次 / 2 始终允许 / 3 拒绝");
    text
}

/// Human-readable form for an IM message.
pub fn format_form(form: &FormInfo) -> String {
    let mut text = String::new();
    let title = form.title.as_deref().unwrap_or("OpenCode 需要确认");
    text.push_str(&format!("❓ **{title}**\n\n"));

    for field in &form.fields {
        if let Some(title) = field.title.as_deref().filter(|t| !t.is_empty()) {
            text.push_str(&format!("**{title}**\n"));
        }
        if let Some(desc) = field.description.as_deref().filter(|d| !d.is_empty()) {
            text.push_str(desc);
            text.push('\n');
        }
        for (i, option) in field.options.iter().enumerate() {
            text.push_str(&format!("{}. {}", i + 1, option.label));
            if let Some(desc) = option.description.as_deref().filter(|d| !d.is_empty()) {
                text.push_str(&format!(" — {desc}"));
            }
            text.push('\n');
        }
        text.push('\n');
    }

    if form.fields.len() == 1 && !form.fields[0].options.is_empty() {
        text.push_str("回复数字选择，或直接回复自定义答案。");
    } else {
        text.push_str("请在终端里回答这个问题。");
    }
    text
}

/// Resolve a user's text into an answer value for the field.
pub fn answer_value(field: &FormField, text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(index) = trimmed.parse::<usize>()
        && index > 0
        && let Some(option) = field.options.get(index - 1)
    {
        return option.value.clone();
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(value: &str) -> FormOption {
        FormOption {
            value: value.to_string(),
            label: value.to_string(),
            description: None,
        }
    }

    fn question_field() -> FormField {
        FormField {
            key: "q0".to_string(),
            title: Some("Pick one".to_string()),
            description: Some("Choose an option".to_string()),
            options: vec![option("alpha"), option("beta")],
        }
    }

    #[test]
    fn answer_value_maps_a_number_to_the_option_value() {
        let field = question_field();
        assert_eq!(answer_value(&field, "2"), "beta");
        assert_eq!(answer_value(&field, " 1 "), "alpha");
    }

    #[test]
    fn answer_value_keeps_custom_text() {
        let field = question_field();
        assert_eq!(answer_value(&field, "something else"), "something else");
        assert_eq!(answer_value(&field, "99"), "99");
    }

    #[test]
    fn permission_decision_accepts_numbers_and_words() {
        assert_eq!(permission_decision("1"), Some("once"));
        assert_eq!(permission_decision("2"), Some("always"));
        assert_eq!(permission_decision("3"), Some("reject"));
        assert_eq!(permission_decision("允许"), Some("once"));
        assert_eq!(permission_decision("拒绝"), Some("reject"));
        assert_eq!(permission_decision("later"), None);
    }

    #[test]
    fn format_form_lists_options_and_reply_hint() {
        let form = FormInfo {
            id: "frm_test".to_string(),
            title: Some("Questions".to_string()),
            metadata: Some(serde_json::json!({"kind": "question"})),
            fields: vec![question_field()],
        };
        let text = format_form(&form);
        assert!(text.contains("Questions"));
        assert!(text.contains("1. alpha"));
        assert!(text.contains("2. beta"));
        assert!(text.contains("回复数字"));
        assert!(form.is_question());
    }

    #[test]
    fn format_permission_includes_action_and_resources() {
        let request = PermissionRequest {
            id: "per_test".to_string(),
            session_id: "ses_test".to_string(),
            action: "bash".to_string(),
            resources: vec!["rm -rf build".to_string()],
            message: Some("Run a command".to_string()),
        };
        let text = format_permission(&request);
        assert!(text.contains("bash"));
        assert!(text.contains("rm -rf build"));
        assert!(text.contains("1 允许一次"));
    }
}

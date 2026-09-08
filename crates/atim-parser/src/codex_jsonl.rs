use std::path::Path;

use atim_core::error::Result;
use atim_core::message::{ContentType, ParsedEntry};
use tokio::fs;
use tokio::io::AsyncSeekExt;

use crate::truncate_utf8;

/// Max chars of Bash output included in a ToolResult card (keeps huge logs
/// from drowning the IM message; the send layer caps at MAX_MSG_LEN too).
const MAX_BASH_OUTPUT_CHARS: usize = 3000;

/// Reads and parses Codex JSONL session logs.
///
/// Codex writes rollout logs at `~/.codex/sessions/YYYY/MM/DD/rollout-TIMESTAMP-SESSION_ID.jsonl`.
/// Key entry types:
/// - `session_meta` (first line): session_id, cwd
/// - `event_msg` with `payload.type: "item_completed"`: contains AgentMessage, CommandExecution, etc.
pub struct CodexJsonlParser;

/// Session metadata extracted from the first line of a Codex JSONL file.
#[derive(Debug, Clone)]
pub struct CodexSessionMeta {
    pub session_id: String,
    pub cwd: String,
}

impl CodexJsonlParser {
    /// Read session metadata (session_id, cwd) from the first line of a Codex JSONL file.
    pub async fn read_meta(path: &Path) -> Result<CodexSessionMeta> {
        let file = fs::File::open(path).await?;
        let mut reader = tokio::io::BufReader::new(file);
        let mut first_line = String::new();
        use tokio::io::AsyncBufReadExt;
        reader
            .read_line(&mut first_line)
            .await
            .map_err(|e| atim_core::error::Error::Io(std::io::Error::other(e)))?;

        let v: serde_json::Value = serde_json::from_str(first_line.trim())
            .map_err(|e| atim_core::error::Error::Parse(format!("codex meta: {e}")))?;

        let payload = &v["payload"];
        Ok(CodexSessionMeta {
            session_id: payload["session_id"].as_str().unwrap_or("").to_string(),
            cwd: payload["cwd"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Read new entries from a Codex JSONL file starting at `offset`.
    /// Returns (entries, new_offset).
    pub async fn read_new(path: &Path, offset: u64) -> Result<(Vec<ParsedEntry>, u64)> {
        let mut file = fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        let file_size = metadata.len();
        if file_size <= offset {
            return Ok((Vec::new(), file_size));
        }

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut reader = tokio::io::BufReader::new(file);
        let mut new_data = Vec::new();
        use tokio::io::AsyncReadExt;
        reader
            .read_to_end(&mut new_data)
            .await
            .map_err(atim_core::error::Error::Io)?;

        let text = String::from_utf8_lossy(&new_data);
        let entries = Self::parse_str(&text);
        let new_offset = file_size;
        Ok((entries, new_offset))
    }

    /// Parse Codex JSONL text into ParsedEntry items.
    fn parse_str(data: &str) -> Vec<ParsedEntry> {
        let mut entries = Vec::new();
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(mut parsed) = Self::parse_line(line) {
                entries.append(&mut parsed);
            }
        }
        entries
    }

    /// Parse a single JSONL line. Returns entries or None if not relevant.
    fn parse_line(line: &str) -> Option<Vec<ParsedEntry>> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let entry_type = v.get("type")?.as_str()?;

        match entry_type {
            "event_msg" => Self::parse_event_msg(&v),
            "response_item" => Self::parse_response_item(&v),
            _ => None,
        }
    }

    /// Parse `response_item` lines (Codex Responses API: LocalShellCall, AgentMessage, etc.)
    ///
    /// `response_item` is internally tagged with `#[serde(tag = "type", rename_all = "snake_case")]`,
    /// so `payload.type` identifies the variant.
    fn parse_response_item(v: &serde_json::Value) -> Option<Vec<ParsedEntry>> {
        let payload = v.get("payload")?;
        let item_type = payload.get("type")?.as_str()?;
        let timestamp = v
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);

        match item_type {
            "local_shell_call" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| payload.get("id").and_then(|v| v.as_str()))
                    .map(String::from);
                let action = payload.get("action")?;
                // action is internally tagged: {"type":"exec","command":["cmd"]}
                if action.get("type").and_then(|v| v.as_str()) != Some("exec") {
                    return None;
                }
                let command = extract_codex_command(action);
                let summary = format!("💻 Bash:\n```bash\n{command}\n```");
                let tool_use_id = call_id;

                let mut entries = Vec::new();
                entries.push(ParsedEntry {
                    role: "assistant".into(),
                    text: summary,
                    content_type: ContentType::ToolUse,
                    tool_use_id: tool_use_id.clone(),
                    tool_name: Some(crate::TOOL_BASH.into()),
                    timestamp: timestamp.clone(),
                    image_data: None,
                    raw_input: None,
                });

                // ToolResult: status only (no output available in response_item)
                let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
                let result_text = match status {
                    "completed" => "✅ ok".to_string(),
                    "failed" => "❌ failed".to_string(),
                    s if !s.is_empty() => format!("⏳ {s}"),
                    _ => String::new(),
                };
                entries.push(ParsedEntry {
                    role: "user".into(),
                    text: result_text,
                    content_type: ContentType::ToolResult,
                    tool_use_id,
                    tool_name: Some(crate::TOOL_BASH.into()),
                    timestamp,
                    image_data: None,
                    raw_input: None,
                });
                Some(entries)
            }
            "agent_message" => {
                let content = payload.get("content")?;
                let text = extract_text_from_content(content);
                if text.is_empty() {
                    return None;
                }
                Some(vec![ParsedEntry {
                    role: "assistant".into(),
                    text,
                    content_type: ContentType::Text,
                    tool_use_id: None,
                    tool_name: None,
                    timestamp,
                    image_data: None,
                    raw_input: None,
                }])
            }
            other => {
                tracing::debug!("Unknown Codex response_item type: {other}");
                None
            }
        }
    }

    /// Parse `event_msg` entries (item_completed with AgentMessage, CommandExecution, etc.)
    fn parse_event_msg(v: &serde_json::Value) -> Option<Vec<ParsedEntry>> {
        let payload = v.get("payload")?;
        let payload_type = payload.get("type")?.as_str()?;

        match payload_type {
            "item_completed" => Self::parse_item_completed(payload),
            _ => None,
        }
    }

    /// Parse an `item_completed` event.
    fn parse_item_completed(payload: &serde_json::Value) -> Option<Vec<ParsedEntry>> {
        let item = payload.get("item")?;
        let item_type = item.get("type")?.as_str()?;
        let timestamp = payload
            .get("completed_at_ms")
            .and_then(|v| v.as_i64())
            .map(|ms| {
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            });

        match item_type {
            "AgentMessage" => {
                let content = item.get("content")?;
                let text = extract_text_from_content(content);
                if text.is_empty() {
                    return None;
                }
                Some(vec![ParsedEntry {
                    role: "assistant".into(),
                    text,
                    content_type: ContentType::Text,
                    tool_use_id: None,
                    tool_name: None,
                    timestamp,
                    image_data: None,
                    raw_input: None,
                }])
            }
            "CommandExecution" => {
                let command = extract_codex_command(item);
                let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
                let output = extract_codex_output(item);

                let tool_use_id = item.get("id").and_then(|v| v.as_str()).map(String::from);

                let mut entries = Vec::new();

                // ToolUse entry
                let summary = format!("💻 Bash:\n```bash\n{command}\n```");
                entries.push(ParsedEntry {
                    role: "assistant".into(),
                    text: summary,
                    content_type: ContentType::ToolUse,
                    tool_use_id: tool_use_id.clone(),
                    tool_name: Some(crate::TOOL_BASH.into()),
                    timestamp: timestamp.clone(),
                    image_data: None,
                    raw_input: None,
                });

                // ToolResult entry — command output (truncated) + exit status.
                let mut result_text = String::new();
                if !output.is_empty() {
                    let output = output.trim_end();
                    let line_count = output.lines().count();
                    let shown = truncate_utf8(output, MAX_BASH_OUTPUT_CHARS);
                    let truncated = shown.len() < output.len();
                    result_text.push_str(&format!("```\n{shown}\n```"));
                    if truncated {
                        result_text.push_str(&format!("\n({line_count} lines, truncated)"));
                    }
                }
                if let Some(code) = exit_code {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    if code == 0 {
                        result_text.push_str("✅ exit 0");
                    } else {
                        result_text.push_str(&format!("❌ exit {code}"));
                    }
                }

                entries.push(ParsedEntry {
                    role: "user".into(),
                    text: result_text,
                    content_type: ContentType::ToolResult,
                    tool_use_id,
                    tool_name: Some(crate::TOOL_BASH.into()),
                    timestamp,
                    image_data: None,
                    raw_input: None,
                });

                Some(entries)
            }
            "DynamicToolCallItem" => {
                // MCP / dynamic tool execution.
                let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
                let namespace = item.get("namespace").and_then(|v| v.as_str());
                let tool_label = match namespace {
                    Some(ns) => format!("{ns}::{tool}"),
                    None => tool.to_string(),
                };
                let args = item
                    .get("arguments")
                    .map(|v| {
                        let s = v.to_string();
                        truncate_utf8(&s, MAX_BASH_OUTPUT_CHARS)
                    })
                    .unwrap_or_default();
                let success = item.get("success").and_then(|v| v.as_bool());
                let error = item.get("error").and_then(|v| v.as_str());
                let output_items = item.get("content_items").and_then(|v| v.as_array());

                let tool_use_id = item.get("id").and_then(|v| v.as_str()).map(String::from);
                let mut entries = Vec::new();

                // ToolUse entry
                let summary = format!("🔧 {tool_label}:\n```\n{args}\n```");
                entries.push(ParsedEntry {
                    role: "assistant".into(),
                    text: summary,
                    content_type: ContentType::ToolUse,
                    tool_use_id: tool_use_id.clone(),
                    tool_name: Some(tool.to_string()),
                    timestamp: timestamp.clone(),
                    image_data: None,
                    raw_input: None,
                });

                // ToolResult entry — extract text from content_items
                let mut result_text = String::new();
                if let Some(items) = output_items {
                    let mut parts = Vec::new();
                    for ci in items {
                        if let Some(t) = ci.get("text").and_then(|v| v.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    let joined = parts.join("\n");
                    if !joined.is_empty() {
                        result_text.push_str(&format!(
                            "```\n{}\n```",
                            truncate_utf8(&joined, MAX_BASH_OUTPUT_CHARS)
                        ));
                    }
                }
                if success == Some(false) || error.is_some() {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    result_text.push_str(&format!("❌ {}", error.unwrap_or("failed")));
                } else if success == Some(true) {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    result_text.push_str("✅ ok");
                }

                entries.push(ParsedEntry {
                    role: "user".into(),
                    text: result_text,
                    content_type: ContentType::ToolResult,
                    tool_use_id,
                    tool_name: Some(tool.to_string()),
                    timestamp,
                    image_data: None,
                    raw_input: None,
                });
                Some(entries)
            }
            other => {
                tracing::debug!("Unknown Codex item_completed type: {other}");
                None
            }
        }
    }
}

/// Extract text from a Codex content array.
/// Handles both `[{type: "Text", text: "..."}]` and `[{type: "input_text", text: "..."}]` formats.
fn extract_text_from_content(content: &serde_json::Value) -> String {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return String::new(),
    };

    let mut parts = Vec::new();
    for item in arr {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
}

/// Extract the readable shell command from a Codex `CommandExecution` item.
///
/// `command` is usually an *argv array* (`["/bin/zsh", "-lc", "cmd"]`), so
/// `.as_str()` alone returns nothing. Codex also writes `parsed_cmd` with a
/// readable `.cmd` — prefer it; fall back to a plain-string command or the
/// last argv element.
fn extract_codex_command(item: &serde_json::Value) -> String {
    if let Some(parsed) = item.get("parsed_cmd").and_then(|v| v.as_array())
        && let Some(first) = parsed.first()
        && let Some(cmd) = first.get("cmd").and_then(|v| v.as_str())
        && !cmd.is_empty()
    {
        return cmd.to_string();
    }
    match item.get("command") {
        Some(c) if c.is_string() => c.as_str().unwrap_or("").to_string(),
        Some(c) if c.is_array() => c
            .as_array()
            .and_then(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .next_back()
                    .map(String::from)
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Extract Bash output from a Codex `CommandExecution` item.
///
/// Codex stores `stdout`/`stderr` plus the combined `aggregated_output`
/// (preferred; `formatted_output` as fallback), not a bare `output` field.
fn extract_codex_output(item: &serde_json::Value) -> String {
    for key in ["aggregated_output", "formatted_output", "stdout"] {
        if let Some(s) = item.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    item.get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agent_message() {
        let line = r#"{"timestamp":"2026-08-31T08:43:18.137Z","ordinal":10,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"msg_123","content":[{"type":"Text","text":"Hello world!"}]},"completed_at_ms":1788165798137}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::Text);
        assert_eq!(entries[0].text, "Hello world!");
        assert_eq!(entries[0].role, "assistant");
    }

    #[test]
    fn test_parse_command_execution() {
        // Real Codex items: command is an argv array, output lives in aggregated_output.
        let line = r#"{"timestamp":"2026-08-31T08:43:20.000Z","ordinal":11,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call_456","command":["/bin/sh","-c","echo hello"],"exit_code":0,"aggregated_output":"hello\n"},"completed_at_ms":1788165799000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[0].tool_name.as_deref(), Some("Bash"));
        // Command is extracted from the argv array.
        assert!(entries[0].text.contains("echo hello"));
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        // Output is now included, plus exit status.
        assert!(entries[1].text.contains("hello"));
        assert!(entries[1].text.contains("✅ exit 0"));
    }

    #[test]
    fn test_parse_command_execution_null_command() {
        // A `command` that is not a string/array must not produce an empty shell block.
        let line = r#"{"timestamp":"2026-08-31T08:43:20.000Z","ordinal":11,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call_111","command":null,"exit_code":0,"aggregated_output":"ok"},"completed_at_ms":1788165799000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        // Command not resolvable → still a usable card, no runaway.
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[1].text, "```\nok\n```\n✅ exit 0");
    }

    #[test]
    fn test_parse_command_execution_failed() {
        let line = r#"{"timestamp":"2026-08-31T08:43:20.000Z","ordinal":11,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call_789","command":"false","exit_code":1,"aggregated_output":""},"completed_at_ms":1788165799000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert_eq!(entries[1].text, "❌ exit 1");
    }

    #[test]
    fn test_parse_session_meta_skipped() {
        let line = r#"{"timestamp":"2026-08-31T08:43:13.085Z","ordinal":0,"type":"session_meta","payload":{"session_id":"abc","cwd":"/tmp"}}"#;
        assert!(CodexJsonlParser::parse_line(line).is_none());
    }

    #[test]
    fn test_parse_empty_content() {
        let line = r#"{"timestamp":"2026-08-31T08:43:18.137Z","ordinal":10,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"msg_123","content":[]},"completed_at_ms":1788165798137}}"#;
        assert!(CodexJsonlParser::parse_line(line).is_none());
    }

    #[test]
    fn test_parse_dynamic_tool_call_item() {
        let line = r#"{"timestamp":"2026-09-01T00:00:00Z","ordinal":5,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"DynamicToolCallItem","id":"dtc_1","tool":"web_search","namespace":"mcp","arguments":{"query":"rust macros"},"status":"completed","content_items":[{"type":"inputText","text":"Rust macros explained..."}],"success":true},"completed_at_ms":1788500000000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert!(entries[0].text.contains("mcp::web_search"));
        assert_eq!(entries[0].tool_name.as_deref(), Some("web_search"));
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert!(entries[1].text.contains("Rust macros explained..."));
        assert!(entries[1].text.contains("✅"));
    }

    #[test]
    fn test_parse_dynamic_tool_call_failed() {
        let line = r#"{"timestamp":"2026-09-01T00:00:00Z","ordinal":5,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"DynamicToolCallItem","id":"dtc_2","tool":"db_query","arguments":"{}","status":"failed","content_items":null,"success":false,"error":"timeout"},"completed_at_ms":1788500000000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[1].text.contains("❌ timeout"));
    }

    #[test]
    fn test_parse_response_item_local_shell_call() {
        // Codex Responses API: local_shell_call uses internally-tagged action.
        let line = r#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"call_789","status":"completed","action":{"type":"exec","command":["/bin/sh","-c","echo ok"]}},"timestamp":"2026-09-01T01:00:00Z"}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert!(entries[0].text.contains("echo ok"));
        assert_eq!(entries[0].tool_use_id.as_deref(), Some("call_789"));
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert!(entries[1].text.contains("✅ ok"));
    }

    #[test]
    fn test_parse_response_item_agent_message() {
        let line = r#"{"type":"response_item","payload":{"type":"agent_message","author":"codex","recipient":"user","content":[{"type":"input_text","text":"Here is the result."}]},"timestamp":"2026-09-01T02:00:00Z"}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].text, "Here is the result.");
    }
}

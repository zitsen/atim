use std::path::Path;

use atim_core::error::Result;
use atim_core::message::{ContentType, ParsedEntry};
use tokio::fs;
use tokio::io::AsyncSeekExt;

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
            _ => None,
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
                let command = item.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
                let _output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");

                let tool_use_id = item.get("id").and_then(|v| v.as_str()).map(String::from);

                let mut entries = Vec::new();

                // ToolUse entry
                let summary = format!("💻 Bash:\n```bash\n{command}\n```");
                entries.push(ParsedEntry {
                    role: "assistant".into(),
                    text: summary,
                    content_type: ContentType::ToolUse,
                    tool_use_id: tool_use_id.clone(),
                    tool_name: Some("Bash".into()),
                    timestamp: timestamp.clone(),
                    image_data: None,
                    raw_input: None,
                });

                // ToolResult entry
                let result_suffix = if let Some(code) = exit_code {
                    if code == 0 {
                        String::new()
                    } else {
                        format!("exit {code}")
                    }
                } else {
                    String::new()
                };
                entries.push(ParsedEntry {
                    role: "user".into(),
                    text: result_suffix,
                    content_type: ContentType::ToolResult,
                    tool_use_id,
                    tool_name: Some("Bash".into()),
                    timestamp,
                    image_data: None,
                    raw_input: None,
                });

                Some(entries)
            }
            _ => None,
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
        let line = r#"{"timestamp":"2026-08-31T08:43:20.000Z","ordinal":11,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call_456","command":"echo hello","exit_code":0,"output":"hello\n"},"completed_at_ms":1788165799000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[0].tool_name.as_deref(), Some("Bash"));
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert_eq!(entries[1].text, ""); // exit 0 = empty suffix
    }

    #[test]
    fn test_parse_command_execution_failed() {
        let line = r#"{"timestamp":"2026-08-31T08:43:20.000Z","ordinal":11,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"call_789","command":"false","exit_code":1,"output":""},"completed_at_ms":1788165799000}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert_eq!(entries[1].text, "exit 1");
    }

    #[test]
    fn test_parse_response_item() {
        let line = r#"{"timestamp":"2026-08-31T08:43:13.085Z","ordinal":2,"type":"response_item","payload":{"type":"message","id":"msg_abc","role":"assistant","content":[{"type":"output_text","text":"Let me help you."}]}}"#;
        let entries = CodexJsonlParser::parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].text, "Let me help you.");
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
}

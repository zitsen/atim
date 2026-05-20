use std::path::Path;

use atim_core::error::Result;
use atim_core::message::{ContentType, ParsedEntry};
use tokio::fs;
use tokio::io::AsyncSeekExt;

/// Reads and parses Copilot CLI JSONL session logs.
///
/// Copilot stores session logs at:
///   `~/.copilot/session-state/<uuid>/events.jsonl`
///
/// Each line is a JSON object with a `type` field and `data` payload.
/// Key event types:
///   - session.start, session.info, session.model_change (metadata — skipped)
///   - user.message           → user Text entry
///   - assistant.message      → assistant Text entry(s) + optional ToolUse
///   - assistant.turn_start/end → turn boundaries (used for completeness detection)
///   - tool.execution_start   → start of tool (skipped, execution_complete carries result)
///   - tool.execution_complete → ToolResult entry
///
/// Supports byte-offset incremental reading.
pub struct CopilotJsonlParser;

impl CopilotJsonlParser {
    /// Parse a complete Copilot JSONL file, returning all entries.
    pub async fn parse_file(path: &Path) -> Result<Vec<ParsedEntry>> {
        let data = fs::read_to_string(path).await?;
        Self::parse_str(&data)
    }

    /// Parse Copilot JSONL from a string.
    pub fn parse_str(data: &str) -> Result<Vec<ParsedEntry>> {
        let mut entries = Vec::new();
        let mut tool_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match Self::parse_line(line, &mut tool_names) {
                Ok(mut parsed) => entries.append(&mut parsed),
                Err(e) => {
                    tracing::warn!("Skipping malformed Copilot JSONL line {}: {e}", i + 1);
                }
            }
        }
        Ok(entries)
    }

    /// Read new data from a file starting at a given byte offset.
    pub async fn read_new<P: AsRef<Path>>(path: P, offset: u64) -> Result<(Vec<ParsedEntry>, u64)> {
        let mut file = fs::File::open(path.as_ref()).await?;
        let metadata = file.metadata().await?;
        let file_size = metadata.len();

        if file_size <= offset {
            return Ok((Vec::new(), file_size));
        }

        file.seek(std::io::SeekFrom::Start(offset)).await?;

        use tokio::io::AsyncReadExt;
        let mut reader = tokio::io::BufReader::new(file);
        let mut new_data = Vec::new();
        reader.read_to_end(&mut new_data).await?;

        let text = String::from_utf8_lossy(&new_data);
        let entries = Self::parse_str(&text)?;

        Ok((entries, file_size))
    }

    /// Parse a single Copilot JSONL line into zero or more `ParsedEntry`s.
    ///
    /// `tool_names` is a mutable cache mapping `toolCallId → toolName` across
    /// multiple lines in a single parse batch.
    fn parse_line(
        line: &str,
        tool_names: &mut std::collections::HashMap<String, String>,
    ) -> Result<Vec<ParsedEntry>> {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| atim_core::error::Error::Parse(format!("Copilot JSON parse error: {e}")))?;

        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = value.get("data");
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);

        let mut entries = Vec::new();

        match event_type {
            "user.message" => {
                let content = data
                    .and_then(|d| d.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !content.is_empty() {
                    entries.push(ParsedEntry {
                        role: "user".into(),
                        text: content,
                        content_type: ContentType::Text,
                        tool_use_id: None,
                        tool_name: None,
                        timestamp,
                        image_data: None,
                    });
                }
            }
            "assistant.message" => {
                // Text content
                let content = data
                    .and_then(|d| d.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !content.is_empty() {
                    entries.push(ParsedEntry {
                        role: "assistant".into(),
                        text: content,
                        content_type: ContentType::Text,
                        tool_use_id: None,
                        tool_name: None,
                        timestamp: timestamp.clone(),
                        image_data: None,
                    });
                }

                // Tool requests embedded in assistant messages
                if let Some(tool_requests) = data.and_then(|d| d.get("toolRequests")).and_then(|v| v.as_array()) {
                    for tr in tool_requests {
                        let tool_call_id = tr.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
                        let tool_name = tr.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let arguments = tr.get("arguments");

                        tool_names.insert(tool_call_id.to_string(), tool_name.to_string());

                        let summary = summarize_tool_use(tool_name, arguments);
                        entries.push(ParsedEntry {
                            role: "assistant".into(),
                            text: summary,
                            content_type: ContentType::ToolUse,
                            tool_use_id: Some(tool_call_id.to_string()),
                            tool_name: Some(tool_name.to_string()),
                            timestamp: timestamp.clone(),
                            image_data: None,
                        });
                    }
                }
            }
            "tool.execution_complete" => {
                let tool_call_id = data
                    .and_then(|d| d.get("toolCallId"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let result = data
                    .and_then(|d| d.get("result"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let tool_name = tool_names.get(tool_call_id).map(String::as_str);

                let summary = summarize_tool_result(&result, tool_name);
                entries.push(ParsedEntry {
                    role: "user".into(),
                    text: summary,
                    content_type: ContentType::ToolResult,
                    tool_use_id: Some(tool_call_id.to_string()),
                    tool_name: tool_name.map(String::from),
                    timestamp,
                    image_data: None,
                });
            }
            // Skip all other event types (session.start, system.message,
            // assistant.turn_start, assistant.turn_end, tool.execution_start,
            // session.model_change, session.info, abort, etc.)
            _ => {}
        }

        Ok(entries)
    }
}

/// Generate a one-line summary of a tool_use for display.
fn summarize_tool_use(tool_name: &str, input: Option<&serde_json::Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return format!("🔧 {tool_name}"),
    };

    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return format!("🔧 {tool_name}: {path}");
    }
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        let truncated = if cmd.len() > 50 {
            format!("{}…", &cmd[..47])
        } else {
            cmd.to_string()
        };
        return format!("🔧 {tool_name}: {truncated}");
    }
    if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
        return format!("🔧 {tool_name}: {query}");
    }
    if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
        return format!("🔧 {tool_name}: {pattern}");
    }

    format!("🔧 {tool_name}")
}

/// Generate a brief summary of tool_result for display.
fn summarize_tool_result(text: &str, tool_name: Option<&str>) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "✅ Done (empty result)".into();
    }
    let line_count = text.lines().count();

    if let Some(name) = tool_name {
        match name {
            "Read" | "FileReadTool" => return format!("📄 Read {line_count} lines"),
            "Bash" | "BashRuntime" | "CommandRun" => {
                let exit = extract_exit_code(text);
                return match exit {
                    Some(0) => format!("💻 Completed (exit 0, {line_count} lines)"),
                    Some(n) => format!("💻 Failed (exit {n}, {line_count} lines)"),
                    None => format!("💻 Completed ({line_count} lines)"),
                };
            }
            "Write" | "Create" => return format!("📝 Wrote {} chars", text.len()),
            "Edit" | "TextEdit" => return format!("✏️ Edited ({line_count} lines)"),
            "Glob" | "Grep" | "Search" => {
                return format!("🔍 Found matches ({line_count} lines)")
            }
            _ => {}
        }
    }

    let char_count = text.len();
    if line_count <= 3 && char_count <= 120 {
        return format!("✅ {text}");
    }
    format!("✅ Done ({line_count} lines, {char_count} chars)")
}

/// Try to extract a numeric exit code from command output.
fn extract_exit_code(text: &str) -> Option<i32> {
    for line in text.lines().rev() {
        let line = line.trim();
        if let Some(cap) = line.strip_prefix("[Exit ")
            && let Some(code_str) = cap.strip_suffix(']')
                && let Ok(code) = code_str.parse::<i32>() {
                    return Some(code);
                }
        if line.contains("exit code") || line.contains("exit_code") {
            for word in line.split_whitespace() {
                if let Ok(n) = word
                    .trim_end_matches(&['.', ',', ';', ')', ']'][..])
                    .parse::<i32>()
                {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_message() {
        let line = r#"{"type":"user.message","data":{"content":"hello world"},"id":"u1","timestamp":"2026-01-01T00:00:00Z","parentId":null}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].text, "hello world");
        assert_eq!(entries[0].content_type, ContentType::Text);
    }

    #[test]
    fn test_assistant_message_text() {
        let line = r#"{"type":"assistant.message","data":{"content":"Hi there!"},"id":"a1","timestamp":"2026-01-01T00:00:01Z","parentId":"u1"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].text, "Hi there!");
        assert_eq!(entries[0].content_type, ContentType::Text);
    }

    #[test]
    fn test_assistant_message_empty_content_skipped() {
        let line = r#"{"type":"assistant.message","data":{"content":""},"id":"a1","timestamp":"2026-01-01T00:00:01Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_assistant_message_with_tool_requests() {
        let line = r#"{"type":"assistant.message","data":{"content":"","toolRequests":[{"toolCallId":"call_abc","name":"Read","arguments":{"file_path":"src/main.rs"}}]},"id":"a1","timestamp":"2026-01-01T00:00:01Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[0].tool_use_id.as_deref(), Some("call_abc"));
        assert!(entries[0].text.contains("Read"));
        assert!(entries[0].text.contains("src/main.rs"));
    }

    #[test]
    fn test_assistant_message_text_and_tool_requests() {
        let line = r#"{"type":"assistant.message","data":{"content":"Let me check","toolRequests":[{"toolCallId":"call_abc","name":"Read","arguments":{"file_path":"src/main.rs"}}]},"id":"a1","timestamp":"2026-01-01T00:00:01Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::Text);
        assert_eq!(entries[0].text, "Let me check");
        assert_eq!(entries[1].content_type, ContentType::ToolUse);
    }

    #[test]
    fn test_tool_execution_complete() {
        let mut cache = std::collections::HashMap::new();
        cache.insert("call_abc".to_string(), "Read".to_string());

        let line = r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_abc","result":"fn main() {\n    println!(\"hi\");\n}\n"},"id":"t1","timestamp":"2026-01-01T00:00:02Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut cache).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolResult);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].tool_use_id.as_deref(), Some("call_abc"));
        assert!(entries[0].text.contains("Read"));
    }

    #[test]
    fn test_tool_execution_complete_with_result_text() {
        let mut cache = std::collections::HashMap::new();
        cache.insert("call_xyz".to_string(), "Bash".to_string());

        let line = r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_xyz","result":"Build succeeded\n[Exit 0]\n"},"id":"t1","timestamp":"2026-01-01T00:00:02Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut cache).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolResult);
        assert!(entries[0].text.contains("exit 0"));
    }

    #[test]
    fn test_skip_session_start() {
        let line = r#"{"type":"session.start","data":{"sessionId":"abc"},"id":"s1","timestamp":"2026-01-01T00:00:00Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_skip_turn_events() {
        let line_start = r#"{"type":"assistant.turn_start","id":"ts1","timestamp":"2026-01-01T00:00:00Z"}"#;
        let line_end = r#"{"type":"assistant.turn_end","id":"te1","timestamp":"2026-01-01T00:00:00Z"}"#;
        let mut cache = std::collections::HashMap::new();
        assert_eq!(CopilotJsonlParser::parse_line(line_start, &mut cache).unwrap().len(), 0);
        assert_eq!(CopilotJsonlParser::parse_line(line_end, &mut cache).unwrap().len(), 0);
    }

    #[test]
    fn test_skip_tool_execution_start() {
        let line = r#"{"type":"tool.execution_start","data":{"toolCallId":"call_abc","toolName":"Read","arguments":{}},"id":"t1","timestamp":"2026-01-01T00:00:00Z"}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_tool_names_cache_carries_to_execution_complete() {
        let data = r#"{"type":"assistant.message","data":{"toolRequests":[{"toolCallId":"call_read","name":"Read","arguments":{"file_path":"test.rs"}}]},"id":"a1"}
    {"type":"tool.execution_complete","data":{"toolCallId":"call_read","result":"content"},"id":"t1"}
    "#;
        let entries = CopilotJsonlParser::parse_str(data).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert!(entries[1].text.contains("Read"));
    }

    #[test]
    fn test_parse_multi_line_session() {
        let data = r#"{"type":"session.start","data":{"sessionId":"abc"},"id":"s1"}
    {"type":"user.message","data":{"content":"hello"},"id":"u1"}
    {"type":"assistant.message","data":{"content":"hi"},"id":"a1"}
    "#;
        let entries = CopilotJsonlParser::parse_str(data).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[1].role, "assistant");
    }

    #[test]
    fn test_system_message_skipped() {
        let line = r#"{"type":"system.message","data":{"role":"system","content":"You are an assistant"}}"#;
        let entries = CopilotJsonlParser::parse_line(line, &mut std::collections::HashMap::new()).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_extract_exit_code() {
        assert_eq!(extract_exit_code("[Exit 0]"), Some(0));
        assert_eq!(extract_exit_code("[Exit 1]"), Some(1));
        assert_eq!(extract_exit_code("no code"), None);
    }

    #[test]
    fn test_summarize_tool_result_with_tool_name() {
        let r = summarize_tool_result("fn main() {}", Some("Read"));
        assert!(r.contains("Read"));
    }

    #[test]
    fn test_summarize_tool_result_empty() {
        let r = summarize_tool_result("", None);
        assert!(r.contains("empty result"));
    }

    #[test]
    fn test_summarize_tool_use_with_file_path() {
        let input = serde_json::json!({"file_path": "src/main.rs"});
        let s = summarize_tool_use("Read", Some(&input));
        assert!(s.contains("src/main.rs"));
    }

    #[test]
    fn test_summarize_tool_use_no_input() {
        let s = summarize_tool_use("Read", None);
        assert_eq!(s, "🔧 Read");
    }
}

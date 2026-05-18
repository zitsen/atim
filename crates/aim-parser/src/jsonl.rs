use std::path::Path;

use aim_core::error::{Error, Result};
use aim_core::message::{ContentType, ParsedEntry};
use tokio::fs;
use tokio::io::AsyncSeekExt;

/// Reads and parses Claude Code JSONL session logs.
///
/// Claude Code writes a session log at `~/.claude/sessions/<session_id>.jsonl`.
/// Each line is a JSON object with `role`, `content`, etc.
///
/// This parser supports byte-offset incremental reading so the monitor
/// can pick up only new lines since the last poll.
pub struct JsonlParser;

impl JsonlParser {
    /// Parse a complete JSONL file, returning all entries.
    pub async fn parse_file(path: &Path) -> Result<Vec<ParsedEntry>> {
        let data = fs::read_to_string(path).await?;
        Self::parse_str(&data)
    }

    /// Parse JSONL from a string, returning all entries.
    pub fn parse_str(data: &str) -> Result<Vec<ParsedEntry>> {
        let mut entries = Vec::new();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match Self::parse_line(line) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}   // skip non-message lines
                Err(e) => {
                    // Log and skip malformed lines
                    tracing::warn!("Skipping malformed JSONL line {}: {e}", i + 1);
                }
            }
        }
        Ok(entries)
    }

    /// Read new data from a file starting at a given byte offset.
    ///
    /// Returns the parsed entries plus the new total file size (for updating
    /// the tracked offset).
    pub async fn read_new<P: AsRef<Path>>(
        path: P,
        offset: u64,
    ) -> Result<(Vec<ParsedEntry>, u64)> {
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

    /// Parse a single JSONL line into a `ParsedEntry`.
    ///
    /// Claude Code writes event-log entries.  A conversation message has
    /// `"message": {"role": "assistant"|"user", "content": [...]}` at the
    /// top level.  Metadata lines (`last-prompt`, `permission-mode`, etc.)
    /// are skipped.
    fn parse_line(line: &str) -> Result<Option<ParsedEntry>> {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| Error::Parse(format!("JSON parse error: {e}")))?;

        // Only process entries that have a "message" field with role/content
        let msg = match value.get("message") {
            Some(m) => m,
            None => return Ok(None), // metadata/attachment event
        };

        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Content is an array of blocks: text, tool_use, tool_result, thinking, etc.
        let content = msg.get("content");

        // Determine the top-level event type: "assistant" or "user"
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

        let (text, content_type, tool_use_id, tool_name, image_data) =
            match event_type {
                "assistant" => {
                    // Extract text from content blocks, collect tool_use as metadata
                    let mut full_text = String::new();
                    if let Some(serde_json::Value::Array(blocks)) = content {
                        for block in blocks {
                            match block.get("type").and_then(|v| v.as_str()) {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                        full_text.push_str(t);
                                        full_text.push('\n');
                                    }
                                }
                                Some("thinking") => {
                                    // Don't include thinking in forwarded text
                                }
                                Some("tool_use") => {
                                    // Record tool_use but don't forward
                                }
                                _ => {}
                            }
                        }
                    }
                    let text = full_text.trim().to_string();
                    if text.is_empty() {
                        return Ok(None);
                    }
                    (text, ContentType::Text, None, None, None)
                }
                "user" => {
                    // User messages and tool_results
                    let mut full_text = String::new();
                    if let Some(serde_json::Value::Array(blocks)) = content {
                        for block in blocks {
                            match block.get("type").and_then(|v| v.as_str()) {
                                Some("tool_result") => {
                                    let extracted = extract_tool_result_text(Some(block));
                                    full_text.push_str(&extracted.text);
                                }
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                        full_text.push_str(t);
                                        full_text.push('\n');
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    let text = full_text.trim().to_string();
                    if text.is_empty() {
                        return Ok(None);
                    }
                    (text, ContentType::Text, None, None, None)
                }
                _ => return Ok(None),
            };

        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(Some(ParsedEntry {
            role,
            text,
            content_type,
            tool_use_id,
            tool_name,
            timestamp,
            image_data,
        }))
    }
}

/// Extract text and images from tool_result content blocks.
struct ExtractedContent {
    text: String,
    images: Option<Vec<(String, Vec<u8>)>>,
}

/// Extract text from tool_result content blocks which have a different structure
/// than regular content blocks.
fn extract_tool_result_text(content: Option<&serde_json::Value>) -> ExtractedContent {
    let content = match content {
        Some(c) => c,
        None => {
            return ExtractedContent {
                text: String::new(),
                images: None,
            }
        }
    };

    match content {
        serde_json::Value::String(s) => ExtractedContent {
            text: s.clone(),
            images: None,
        },
        serde_json::Value::Array(blocks) => {
            let mut text = String::new();
            let mut images: Vec<(String, Vec<u8>)> = Vec::new();

            for block in blocks {
                let block_type = block.get("type").and_then(|v| v.as_str());
                match block_type {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            text.push_str(t);
                            text.push('\n');
                        }
                    }
                    Some("image") | Some("image_url") => {
                        let source = block.get("source");
                        if let Some(media_type) =
                            source.and_then(|s| s.get("media_type").and_then(|v| v.as_str()))
                        {
                            if let Some(data) = source
                                .and_then(|s| s.get("data").and_then(|v| v.as_str()))
                            {
                                if let Ok(bytes) = BASE64_STANDARD.decode(data) {
                                    images.push((media_type.to_string(), bytes));
                                }
                            }
                        }
                    }
                    Some("tool_result_block") => {
                        // Nested — extract from inner content
                        let inner = extract_tool_result_text(Some(block));
                        text.push_str(&inner.text);
                        if let Some(mut imgs) = inner.images {
                            images.append(&mut imgs);
                        }
                    }
                    _ => {
                        // Unknown block type, try json prettify
                        if let Ok(s) = serde_json::to_string_pretty(block) {
                            text.push_str(&s);
                            text.push('\n');
                        }
                    }
                }
            }

            let images = if images.is_empty() { None } else { Some(images) };
            ExtractedContent { text, images }
        }
        _ => ExtractedContent {
            text: content.to_string(),
            images: None,
        },
    }
}

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_line() {
        let line = r#"{"message": {"role": "user", "content": [{"type": "text", "text": "Hello, how are you?"}]}, "type": "user", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entry = JsonlParser::parse_line(line).unwrap().unwrap();
        assert_eq!(entry.role, "user");
        assert_eq!(entry.text, "Hello, how are you?");
        assert_eq!(entry.content_type, ContentType::Text);
    }

    #[test]
    fn test_parse_assistant_text_line() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "text", "text": "Hi there!"}]}, "type": "assistant", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entry = JsonlParser::parse_line(line).unwrap().unwrap();
        assert_eq!(entry.role, "assistant");
        assert_eq!(entry.text, "Hi there!");
        assert_eq!(entry.content_type, ContentType::Text);
    }

    #[test]
    fn test_parse_multi_line() {
        let data = r#"{"message": {"role": "user", "content": [{"type": "text", "text": "Hello"}]}, "type": "user"}
{"message": {"role": "assistant", "content": [{"type": "text", "text": "Hi!"}]}, "type": "assistant"}
"#;
        let entries = JsonlParser::parse_str(data).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_skip_metadata_lines() {
        let data = r#"{"type": "last-prompt", "leafUuid": "abc"}
{"type": "permission-mode", "permissionMode": "bypass"}
{"message": {"role": "user", "content": [{"type": "text", "text": "hello"}]}, "type": "user"}
"#;
        let entries = JsonlParser::parse_str(data).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "hello");
    }

    #[test]
    fn test_assistant_thinking_skipped() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "internal thoughts"}, {"type": "text", "text": "Response text"}]}, "type": "assistant", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entry = JsonlParser::parse_line(line).unwrap().unwrap();
        assert_eq!(entry.text, "Response text");
        assert_eq!(entry.content_type, ContentType::Text);
    }
}

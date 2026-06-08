use std::collections::HashMap;
use std::path::Path;

use atim_core::error::{Error, Result};
use atim_core::message::{ContentType, ParsedEntry};
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
        let mut tool_names: HashMap<String, String> = HashMap::new();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match Self::parse_line(line, &mut tool_names) {
                Ok(mut parsed) => entries.append(&mut parsed),
                Err(e) => {
                    tracing::warn!("Skipping malformed JSONL line {}: {e}", i + 1);
                }
            }
        }
        Ok(entries)
    }

    /// Read new data from a file starting at a given byte offset.
    ///
    /// Returns the parsed entries and the byte position of the last complete
    /// line. If the last line is incomplete (partial write by Claude Code),
    /// the returned offset stays at the start of that line so it will be
    /// re-read on the next poll.
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

        // Find end of last complete line in the read data.
        // If the file ends mid-line, we back up so the next poll
        // re-reads the incomplete line when it's fully written.
        // If no entries were produced (e.g. metadata-only lines),
        // we still advance past complete lines to avoid getting stuck.
        let new_offset = last_complete_line_offset(offset, &new_data, !entries.is_empty());

        Ok((entries, new_offset))
    }

    /// Parse a single JSONL line into zero or more `ParsedEntry`s.
    ///
    /// An assistant message can produce both a text entry and tool_use entries.
    /// Metadata lines produce nothing.
    ///
    /// `tool_names` is a mutable cache mapping `tool_use_id → tool_name` across
    /// multiple lines in a single parse batch. It lets tool_result entries know
    /// which tool produced them for smarter summaries.
    fn parse_line(
        line: &str,
        tool_names: &mut HashMap<String, String>,
    ) -> Result<Vec<ParsedEntry>> {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| Error::Parse(format!("JSON parse error: {e}")))?;

        let msg = match value.get("message") {
            Some(m) => m,
            None => return Ok(vec![]),
        };

        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let content = msg.get("content");
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(String::from);

        let mut entries = Vec::new();

        match event_type {
            "assistant" => {
                let mut full_text = String::new();
                let mut tool_entries = Vec::new();
                if let Some(serde_json::Value::Array(blocks)) = content {
                    for block in blocks {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                    full_text.push_str(t);
                                    full_text.push('\n');
                                }
                            }
                            Some("thinking") => {}
                            Some("tool_use") => {
                                let tool_name =
                                    block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                                let tool_use_id =
                                    block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                tool_names.insert(tool_use_id.to_string(), tool_name.to_string());
                                let input = block.get("input");
                                let summary = summarize_tool_use(tool_name, input);
                                // Preserve full input JSON for AskUserQuestion so the
                                // server can build interactive cards from the questions.
                                let raw_input = if tool_name == "AskUserQuestion" {
                                    input.map(|v| v.to_string())
                                } else {
                                    None
                                };
                                tool_entries.push(ParsedEntry {
                                    role: role.clone(),
                                    text: summary,
                                    content_type: ContentType::ToolUse,
                                    tool_use_id: Some(tool_use_id.to_string()),
                                    tool_name: Some(tool_name.to_string()),
                                    timestamp: timestamp.clone(),
                                    image_data: None,
                                    raw_input,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                // Text entry before tool_use entries (logical order)
                let text = full_text.trim().to_string();
                if !text.is_empty() {
                    entries.push(ParsedEntry {
                        role: role.clone(),
                        text,
                        content_type: ContentType::Text,
                        tool_use_id: None,
                        tool_name: None,
                        timestamp: timestamp.clone(),
                        image_data: None,
                        raw_input: None,
                    });
                }
                entries.extend(tool_entries);
            }
            "user" => {
                let mut full_text = String::new();
                if let Some(serde_json::Value::Array(blocks)) = content {
                    for block in blocks {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("tool_result") => {
                                let tuid = block.get("tool_use_id").and_then(|v| v.as_str());
                                let extracted = extract_tool_result_text(Some(block));
                                let tool_use_id = tuid.unwrap_or("");
                                let has_text = !extracted.text.trim().is_empty();
                                let has_images = extracted.images.is_some();
                                let tool_name = if !tool_use_id.is_empty() {
                                    tool_names.get(tool_use_id).map(String::as_str)
                                } else {
                                    None
                                };

                                if has_text {
                                    let summary = summarize_tool_result(&extracted.text, tool_name);
                                    entries.push(ParsedEntry {
                                        role: role.clone(),
                                        text: summary,
                                        content_type: ContentType::ToolResult,
                                        tool_use_id: Some(tool_use_id.to_string()),
                                        tool_name: tool_name.map(String::from),
                                        timestamp: timestamp.clone(),
                                        image_data: None,
                                        raw_input: None,
                                    });
                                }

                                if let Some(images) = extracted.images
                                    && !images.is_empty()
                                {
                                    entries.push(ParsedEntry {
                                        role: role.clone(),
                                        text: String::new(),
                                        content_type: ContentType::ToolResult,
                                        tool_use_id: Some(tool_use_id.to_string()),
                                        tool_name: None,
                                        timestamp: timestamp.clone(),
                                        image_data: Some(images),
                                        raw_input: None,
                                    });
                                }

                                if !has_text && !has_images {
                                    full_text.push_str(&extracted.text);
                                }
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
                if !text.is_empty() {
                    entries.push(ParsedEntry {
                        role: role.clone(),
                        text,
                        content_type: ContentType::Text,
                        tool_use_id: None,
                        tool_name: None,
                        timestamp,
                        image_data: None,
                        raw_input: None,
                    });
                }
            }
            _ => {}
        }

        Ok(entries)
    }
}

/// Generate a one-line summary of a tool_use block for display.
fn summarize_tool_use(tool_name: &str, input: Option<&serde_json::Value>) -> String {
    let icon = tool_icon(tool_name);
    let input = match input {
        Some(v) => v,
        None => return format!("{icon} {tool_name}"),
    };

    if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        return format!("{icon} {tool_name}: {path}");
    }
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        let truncated = crate::truncate_utf8(cmd, 47);
        return format!("{icon} {tool_name}: {truncated}");
    }
    if let Some(query) = input.get("query").and_then(|v| v.as_str()) {
        return format!("{icon} {tool_name}: {query}");
    }
    if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
        let truncated = crate::truncate_utf8(content, 47);
        return format!("{icon} {tool_name}: {truncated}");
    }

    format!("{icon} {tool_name}")
}

/// Map a tool name to a descriptive icon.
fn tool_icon(tool_name: &str) -> &'static str {
    match tool_name {
        "Read" | "ReadTool" | "FileReadTool" => "📖",
        "Edit" | "EditTool" | "TextEditTool" => "✏️",
        "Bash" | "BashTool" | "BashRuntime" => "💻",
        "Write" | "WriteTool" | "CreateTool" | "Create" | "FileWriteTool" => "📝",
        "Search" | "SearchTool" | "GrepTool" | "GlobTool" => "🔍",
        "ThinkTool" | "ThinkingTool" => "🤔",
        _ => "🔧",
    }
}

/// Generate a brief summary of tool_result content for inline editing.
///
/// When `tool_name` is known, produces tool-specific summaries:
/// - Read → "📄 Read N lines"
/// - Edit  → "✏️ Edited: +N −M lines" (diff additions/deletions)
/// - Bash  → "⚡ Completed (code N)" or "⚡ Completed (N lines)"
/// - Write/Create → "📝 Wrote N chars"
/// - Search/Grep  → "🔍 Found N matches"
/// - Default → "✅ Done (N lines, N chars)"
fn summarize_tool_result(text: &str, tool_name: Option<&str>) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "✅ Done (empty result)".into();
    }
    let line_count = text.lines().count();

    match tool_name {
        Some("Read") | Some("ReadTool") | Some("FileReadTool") => {
            // Include file content in the output (truncated for card display).
            // Strip the "N\t" line-number prefix that Claude Code adds.
            let cleaned: String = text
                .lines()
                .filter_map(|line| line.split_once('\t').map(|(_, rest)| rest).or(Some(line)))
                .collect::<Vec<_>>()
                .join("\n");
            let truncated = if cleaned.len() > 3000 {
                format!(
                    "{}…\n(truncated)",
                    &cleaned[..cleaned
                        .char_indices()
                        .nth(2997)
                        .map(|(i, _)| i)
                        .unwrap_or(cleaned.len())]
                )
            } else {
                cleaned
            };
            return format!("📄 Read {} lines\n```\n{}```", line_count, truncated);
        }
        Some("Edit") | Some("EditTool") | Some("TextEditTool") => {
            // Count additions (+) and deletions (-) in diff output
            let (adds, dels) = count_diff_changes(text);
            if adds > 0 || dels > 0 {
                return format!("✏️ Edited: +{adds} −{dels} lines");
            }
            return format!("✏️ Edited ({line_count} lines)");
        }
        Some("Bash") | Some("BashTool") | Some("BashRuntime") => {
            let exit = extract_exit_code(text);
            return match exit {
                Some(0) => format!("⚡ Completed (exit 0, {line_count} lines)"),
                Some(n) => format!("⚡ Failed (exit {n}, {line_count} lines)"),
                None => format!("⚡ Completed ({line_count} lines)"),
            };
        }
        Some("Write")
        | Some("WriteTool")
        | Some("CreateTool")
        | Some("Create")
        | Some("FileWriteTool") => {
            return format!("📝 Wrote {} chars", text.len());
        }
        Some("Search") | Some("SearchTool") | Some("GrepTool") | Some("GlobTool") => {
            let matches = count_search_matches(text);
            return format!("🔍 Found {matches} matches");
        }
        _ => {}
    }

    let char_count = text.len();
    if line_count <= 3 && char_count <= 120 {
        return format!("✅ {text}");
    }
    format!("✅ Done ({line_count} lines, {char_count} chars)")
}

/// Count `+` and `-` lines in a unified diff (ignoring `---`/`+++` headers and `@@` hunks).
fn count_diff_changes(text: &str) -> (usize, usize) {
    let mut adds = 0usize;
    let mut dels = 0usize;
    for line in text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            adds += 1;
        } else if line.starts_with('-') {
            dels += 1;
        }
    }
    (adds, dels)
}

/// Try to extract a numeric exit code from command output.
///
/// Common patterns: "[Exit N]", "[exit code N]", "Process exited with code N",
/// or a trailing line matching these.
fn extract_exit_code(text: &str) -> Option<i32> {
    for line in text.lines().rev() {
        let line = line.trim();
        // Match "[Exit 0]", "[Exit 1]", etc.
        if let Some(cap) = line.strip_prefix("[Exit ")
            && let Some(code_str) = cap.strip_suffix(']')
            && let Ok(code) = code_str.parse::<i32>()
        {
            return Some(code);
        }
        // Match "exit code N" or "exit code: N"
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

/// Count the number of matches found in search/grep output.
///
/// Heuristic: count lines that look like match results (contain `:` after a file path).
fn count_search_matches(text: &str) -> usize {
    let line_count = text.lines().count();
    // If output looks like grep matches (path:line:content), count match lines
    let colon_lines = text
        .lines()
        .filter(|l| l.contains(':') && !l.trim().is_empty())
        .count();
    if colon_lines > line_count / 2 {
        colon_lines
    } else {
        line_count
    }
}

/// Extract text and images from tool_result content blocks.
struct ExtractedContent {
    text: String,
    images: Option<Vec<(String, Vec<u8>)>>,
}

fn extract_tool_result_text(content: Option<&serde_json::Value>) -> ExtractedContent {
    let content = match content {
        Some(c) => c,
        None => {
            return ExtractedContent {
                text: String::new(),
                images: None,
            };
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
                            && let Some(data) =
                                source.and_then(|s| s.get("data").and_then(|v| v.as_str()))
                            && let Ok(bytes) = BASE64_STANDARD.decode(data)
                        {
                            images.push((media_type.to_string(), bytes));
                        }
                    }
                    Some("tool_result_block") => {
                        let inner = extract_tool_result_text(Some(block));
                        text.push_str(&inner.text);
                        if let Some(mut imgs) = inner.images {
                            images.append(&mut imgs);
                        }
                    }
                    _ => {
                        if let Ok(s) = serde_json::to_string_pretty(block) {
                            text.push_str(&s);
                            text.push('\n');
                        }
                    }
                }
            }

            let images = if images.is_empty() {
                None
            } else {
                Some(images)
            };
            ExtractedContent { text, images }
        }
        serde_json::Value::Object(_) => {
            // Tool_result block: extract the inner "content" field
            match content.get("content") {
                Some(serde_json::Value::String(s)) => ExtractedContent {
                    text: s.clone(),
                    images: None,
                },
                Some(inner @ serde_json::Value::Array(_)) => {
                    // Recurse into the Array arm for structured content
                    extract_tool_result_text(Some(inner))
                }
                Some(other) => ExtractedContent {
                    text: other.to_string(),
                    images: None,
                },
                None => ExtractedContent {
                    text: content.to_string(),
                    images: None,
                },
            }
        }
        _ => ExtractedContent {
            text: content.to_string(),
            images: None,
        },
    }
}

/// Given `start_offset` and the raw bytes read from that point, return the byte
/// position of the last **complete** line end. If the data ends mid-line (no
/// trailing `\n`), the returned offset is moved back to the start of that
/// incomplete line so it can be re-read when fully written.
///
/// `has_entries` signals whether `parse_str` successfully extracted at least
/// one entry from the data. When true and there's no trailing `\n`, the data
/// is treated as a complete line (valid JSON without line terminator) and we
/// advance past it. When false and no `\n`, data is incomplete — don't advance.
pub(crate) fn last_complete_line_offset(start_offset: u64, data: &[u8], has_entries: bool) -> u64 {
    if data.is_empty() {
        return start_offset;
    }
    if let Some(pos) = data.iter().rposition(|&b| b == b'\n') {
        // Found a newline — advance past it (safe for completed lines)
        start_offset + pos as u64 + 1
    } else if has_entries {
        // No newline but data was fully parsed — advance past complete content
        start_offset + data.len() as u64
    } else {
        // No newline and nothing was parsed — data is incomplete, stay
        start_offset
    }
}

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Helper: parse a single line with a fresh tool_names cache.
    fn parse_line(line: &str) -> Result<Vec<ParsedEntry>> {
        JsonlParser::parse_line(line, &mut HashMap::new())
    }

    #[test]
    fn test_parse_text_line() {
        let line = r#"{"message": {"role": "user", "content": [{"type": "text", "text": "Hello, how are you?"}]}, "type": "user", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].text, "Hello, how are you?");
        assert_eq!(entries[0].content_type, ContentType::Text);
    }

    #[test]
    fn test_parse_assistant_text_line() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "text", "text": "Hi there!"}]}, "type": "assistant", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].text, "Hi there!");
        assert_eq!(entries[0].content_type, ContentType::Text);
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
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "Response text");
        assert_eq!(entries[0].content_type, ContentType::Text);
    }

    #[test]
    fn test_tool_use_emits_separate_entry() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "text", "text": "Let me check that file."}, {"type": "tool_use", "id": "toolu_abc", "name": "Read", "input": {"file_path": "src/main.rs"}}]}, "type": "assistant", "uuid": "test-uuid", "timestamp": "2025-01-01T00:00:00Z"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::Text);
        assert_eq!(entries[0].text, "Let me check that file.");
        assert_eq!(entries[1].content_type, ContentType::ToolUse);
        assert_eq!(entries[1].tool_use_id.as_deref(), Some("toolu_abc"));
        assert_eq!(entries[1].tool_name.as_deref(), Some("Read"));
        assert!(entries[1].text.contains("src/main.rs"));
    }

    #[test]
    fn test_tool_use_summary_uses_file_path() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_xyz", "name": "ReadTool", "input": {"file_path": "crates/atim-parser/src/jsonl.rs"}}]}, "type": "assistant"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].text.contains("jsonl.rs"));
    }

    #[test]
    fn test_tool_result_emits_summary() {
        let line = r#"{"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_abc", "content": "Hello World"}]}, "type": "user"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolResult);
        assert_eq!(entries[0].tool_use_id.as_deref(), Some("toolu_abc"));
        assert!(entries[0].text.contains("Hello World"));
    }

    #[test]
    fn test_tool_result_multi_line_uses_count() {
        let line = r#"{"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_def", "content": "line1\nline2\nline3\nline4\nline5\n"}]}, "type": "user"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolResult);
        assert!(entries[0].text.contains("5 lines"));
    }

    #[test]
    fn test_tool_use_without_text_still_emits() {
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_only", "name": "Bash", "input": {"command": "cargo test"}}]}, "type": "assistant"}"#;
        let entries = parse_line(line).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert!(entries[0].text.contains("cargo test"));
    }

    #[test]
    fn test_smart_summary_read_shows_line_count() {
        let result = summarize_tool_result("fn main() {\n    println!(\"hi\");\n}\n", Some("Read"));
        assert!(result.contains("Read"));
        assert!(result.contains("3 lines"));
        assert!(!result.contains("Done"));
    }

    #[test]
    fn test_smart_summary_edit_shows_diff() {
        let text = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1,2 @@\n-old line\n+new line\n+another line\n";
        let result = summarize_tool_result(text, Some("Edit"));
        assert!(result.contains("Edited"));
        assert!(result.contains("+2"));
        assert!(result.contains("−1"));
    }

    #[test]
    fn test_smart_summary_bash_shows_exit_code() {
        let text = "build output\n[Exit 0]";
        let result = summarize_tool_result(text, Some("Bash"));
        assert!(result.contains("exit 0"));
    }

    #[test]
    fn test_smart_summary_bash_failed() {
        let text = "error: compilation failed\n[Exit 1]";
        let result = summarize_tool_result(text, Some("Bash"));
        assert!(result.contains("Failed"));
        assert!(result.contains("exit 1"));
    }

    #[test]
    fn test_smart_summary_write_shows_chars() {
        let result = summarize_tool_result("hello world\n", Some("Write"));
        assert!(result.contains("Wrote"));
        assert!(result.contains("11 chars"));
    }

    #[test]
    fn test_smart_summary_search_shows_matches() {
        // grep-style output lines
        let text = "src/main.rs:10:fn main()\nsrc/main.rs:20:    let x = 1\nsrc/lib.rs:5:pub fn test()\nsrc/lib.rs:15:    assert_eq!(1, 1)\n";
        let result = summarize_tool_result(text, Some("GrepTool"));
        assert!(result.contains("Found"));
        assert!(result.contains("4 matches"));
    }

    #[test]
    fn test_tool_use_caches_tool_name() {
        let mut cache = HashMap::new();
        let line = r#"{"message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_cache", "name": "Bash", "input": {"command": "echo hi"}}]}, "type": "assistant"}"#;
        let _ = JsonlParser::parse_line(line, &mut cache).unwrap();
        assert_eq!(cache.get("toolu_cache").map(String::as_str), Some("Bash"));
    }

    #[test]
    fn test_tool_result_gets_smart_summary_from_cached_name() {
        let data = r#"{"message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_smart", "name": "Read", "input": {"file_path": "src/main.rs"}}]}, "type": "assistant"}
{"message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_smart", "content": "fn main() {\n    println!(\"hello\");\n}\n"}]}, "type": "user"}
"#;
        let entries = JsonlParser::parse_str(data).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content_type, ContentType::ToolUse);
        assert_eq!(entries[1].content_type, ContentType::ToolResult);
        assert!(entries[1].text.contains("Read"));
        assert!(entries[1].text.contains("3 lines"));
    }

    #[test]
    fn test_tool_icon_mapping() {
        assert_eq!(tool_icon("Read"), "📖");
        assert_eq!(tool_icon("Bash"), "💻");
        assert_eq!(tool_icon("EditTool"), "✏️");
        assert_eq!(tool_icon("Write"), "📝");
        assert_eq!(tool_icon("GrepTool"), "🔍");
        assert_eq!(tool_icon("ThinkTool"), "🤔");
        assert_eq!(tool_icon("UnknownTool"), "🔧");
    }

    #[test]
    fn test_count_diff_changes() {
        let text = "--- a/file\n+++ b/file\n@@ -1 +1,2 @@\n-old\n+new\n+added\n";
        let (adds, dels) = count_diff_changes(text);
        assert_eq!(adds, 2);
        assert_eq!(dels, 1);
    }

    #[test]
    fn test_extract_exit_code() {
        assert_eq!(extract_exit_code("Build OK\n[Exit 0]"), Some(0));
        assert_eq!(extract_exit_code("Error\n[Exit 1]"), Some(1));
        assert_eq!(
            extract_exit_code("Process finished with exit code 42"),
            Some(42)
        );
        assert_eq!(extract_exit_code("No code here"), None);
    }

    #[test]
    fn test_last_complete_line_offset() {
        // Complete lines (newline-terminated)
        let data = b"line1\nline2\n";
        assert_eq!(
            super::last_complete_line_offset(0, data, false),
            12, // offset after "line1\nline2\n"
        );

        // Incomplete last line
        let data = b"line1\nline2\npartial";
        assert_eq!(
            super::last_complete_line_offset(0, data, false),
            12, // offset after "line1\nline2\n", skips "partial"
        );

        // Single complete line
        let data = b"line1\n";
        assert_eq!(super::last_complete_line_offset(0, data, false), 6);

        // Single incomplete line (no newline) with no entries
        let data = b"partial";
        assert_eq!(super::last_complete_line_offset(0, data, false), 0);

        // No newline but has entries — advance past content
        let data = b"complete_json_line_no_newline";
        assert_eq!(
            super::last_complete_line_offset(0, data, true),
            29, // data.len()
        );

        // Empty data
        let data = b"";
        assert_eq!(super::last_complete_line_offset(0, data, false), 0);

        // Non-zero offset
        let data = b"line2\npartial";
        assert_eq!(
            super::last_complete_line_offset(10, data, false),
            16, // 10 + len("line2\n")
        );
    }

    #[test]
    fn test_smart_summary_short_text_no_tool_name() {
        let result = summarize_tool_result("Hello World", None);
        assert!(result.contains("Hello World"));
        assert!(!result.contains("lines"));
    }

    #[test]
    fn test_smart_summary_empty_result() {
        let result = summarize_tool_result("", Some("Read"));
        assert!(result.contains("empty result"));
    }
}

pub mod codex_jsonl;
pub mod copilot_jsonl;
pub mod jsonl;
pub mod table;
pub mod terminal;

/// Truncate a string to at most `max_chars` characters (not bytes),
/// appending "…" if truncated. Safe for multi-byte UTF-8 strings.
pub fn truncate_utf8(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

/// Dispatch JSONL reading to the appropriate parser based on file path.
///
/// Paths containing `.copilot` use `CopilotJsonlParser`; paths under
/// `.codex/sessions` use `CodexJsonlParser`; everything else uses the
/// standard `JsonlParser` (Claude Code format).
pub async fn read_jsonl<P: AsRef<std::path::Path>>(
    path: P,
    offset: u64,
) -> atim_core::error::Result<(Vec<atim_core::message::ParsedEntry>, u64)> {
    let path_str = path.as_ref().to_string_lossy();
    if path_str.contains(".copilot") {
        copilot_jsonl::CopilotJsonlParser::read_new(path, offset).await
    } else if path_str.contains(".codex") {
        codex_jsonl::CodexJsonlParser::read_new(path.as_ref(), offset).await
    } else {
        jsonl::JsonlParser::read_new(path, offset).await
    }
}

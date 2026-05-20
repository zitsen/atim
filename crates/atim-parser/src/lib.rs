pub mod copilot_jsonl;
pub mod jsonl;
pub mod table;
pub mod terminal;

/// Dispatch JSONL reading to the appropriate parser based on file path.
///
/// Paths containing `.copilot` use `CopilotJsonlParser`; everything else
/// uses the standard `JsonlParser` (Claude Code format).
pub async fn read_jsonl<P: AsRef<std::path::Path>>(
    path: P,
    offset: u64,
) -> atim_core::error::Result<(Vec<atim_core::message::ParsedEntry>, u64)> {
    let path_str = path.as_ref().to_string_lossy();
    if path_str.contains(".copilot") {
        copilot_jsonl::CopilotJsonlParser::read_new(path, offset).await
    } else {
        jsonl::JsonlParser::read_new(path, offset).await
    }
}

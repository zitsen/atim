//! Persistence for files received over IM.
//!
//! Like inbound images, files are written under `<atim_dir>/files/` and the
//! agent receives a prompt referencing the path — binary content cannot cross
//! tmux send-keys.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};

/// How long a saved inbound file stays on disk before the next save sweeps it.
const INBOUND_FILE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Longest stored name (in characters) before the stem is truncated.
const MAX_NAME_LEN: usize = 120;

/// Turn an IM-supplied file name into a safe single path component.
///
/// Only the last path component survives — an IM client must never be able to
/// steer the write outside the files directory — and leading dots are stripped
/// so the result is never hidden or a `..` entry.
pub fn sanitize_file_name(file_name: &str) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        return "file".to_string();
    }
    if cleaned.chars().count() <= MAX_NAME_LEN {
        return cleaned;
    }
    let ext = Path::new(&cleaned)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let stem_len = MAX_NAME_LEN.saturating_sub(ext.chars().count());
    let stem: String = cleaned.chars().take(stem_len).collect();
    format!("{stem}{ext}")
}

/// Persist an inbound file under `<atim_dir>/files/` and return its path.
///
/// The file keeps its original name (sanitized) behind a timestamp prefix so
/// repeated uploads of the same name never collide. Older files are swept on
/// each save so the directory stays bounded without a background job.
pub async fn save_inbound_file(
    atim_dir: &Path,
    file_name: &str,
    data: &[u8],
) -> Result<PathBuf> {
    let dir = atim_dir.join("files");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::State(format!("create files dir: {e}")))?;

    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > INBOUND_FILE_TTL);
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("Failed to sweep old file {:?}: {e}", entry.path());
            }
        }
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = sanitize_file_name(file_name);
    let path = dir.join(format!("{nanos}_{name}"));
    tokio::fs::write(&path, data)
        .await
        .map_err(|e| Error::State(format!("write file: {e}")))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_file_name_rejects_paths() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("a/b\\c.txt"), "c.txt");
        assert_eq!(sanitize_file_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name(""), "file");
    }

    #[test]
    fn test_sanitize_file_name_keeps_cjk_and_truncates() {
        assert_eq!(sanitize_file_name("图标.svg"), "图标.svg");
        let long = format!("{}.svg", "长".repeat(200));
        let out = sanitize_file_name(&long);
        assert!(out.chars().count() <= MAX_NAME_LEN);
        assert!(out.ends_with(".svg"));
    }
}

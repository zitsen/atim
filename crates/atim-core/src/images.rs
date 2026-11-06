//! Persistence for images received over IM.
//!
//! Agents read images from disk, so every inbound image — a standalone photo
//! or one embedded in a rich-text message — is written under `<atim_dir>/images/`
//! and referenced by path in the prompt sent to the agent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};

/// How long a saved inbound image stays on disk before the next save sweeps it.
///
/// The agent normally reads the file within seconds of receiving the prompt;
/// the TTL is generous so a busy session or a restart can't lose the image.
pub const INBOUND_IMAGE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Recognise an image from its magic bytes, returning the extension the IM
/// platforms accept for image messages.
///
/// Returns `None` when the bytes are not a supported image — outbound files
/// use this to tell a photo from a document.
pub fn sniff_image_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if data.starts_with(b"GIF8") {
        Some("gif")
    } else if data.len() > 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Sniff the file extension from image magic bytes, falling back to the
/// advertised mime type.
///
/// Feishu reports every image as `image/png` even when the bytes are JPEG, so
/// the bytes win whenever they carry a recognizable signature.
pub fn image_ext(data: &[u8], mime_type: &str) -> &'static str {
    if let Some(ext) = sniff_image_ext(data) {
        ext
    } else if mime_type.contains("jpeg") || mime_type.contains("jpg") {
        "jpg"
    } else if mime_type.contains("gif") {
        "gif"
    } else if mime_type.contains("webp") {
        "webp"
    } else {
        "png"
    }
}

/// Persist an inbound image under `<atim_dir>/images/` and return its path.
///
/// The directory doubles as the handoff point to the agent. Each save sweeps
/// files that outlived [`INBOUND_IMAGE_TTL`] so the directory stays bounded
/// without a background job.
pub async fn save_inbound_image(atim_dir: &Path, data: &[u8], mime_type: &str) -> Result<PathBuf> {
    let dir = atim_dir.join("images");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| Error::State(format!("create images dir: {e}")))?;

    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > INBOUND_IMAGE_TTL);
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("Failed to sweep old image {:?}: {e}", entry.path());
            }
        }
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("img_{nanos}.{}", image_ext(data, mime_type)));
    tokio::fs::write(&path, data)
        .await
        .map_err(|e| Error::State(format!("write image: {e}")))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_ext_sniffs_magic_bytes_over_mime() {
        // Feishu labels every download image/png; JPEG magic must win.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend(std::iter::repeat_n(0u8, 16));
        assert_eq!(image_ext(&jpeg, "image/png"), "jpg");

        let mut png = vec![0x89, b'P', b'N', b'G'];
        png.extend(std::iter::repeat_n(0u8, 16));
        assert_eq!(image_ext(&png, "image/png"), "png");

        let mut riff = b"RIFF\x00\x00\x00\x00WEBVP".to_vec();
        riff[8..12].copy_from_slice(b"WEBP");
        assert_eq!(image_ext(&riff, ""), "webp");

        // Unknown bytes fall back to the mime type, then to png.
        assert_eq!(image_ext(&[0, 1, 2, 3], "image/gif"), "gif");
        assert_eq!(image_ext(&[0, 1, 2, 3], "text/plain"), "png");
    }

    #[test]
    fn test_sniff_image_ext_tells_photos_from_documents() {
        // `image_ext` defaults to png for unknown bytes; the outbound path
        // needs a sniff that can say "this is not an image".
        let mut png = vec![0x89, b'P', b'N', b'G'];
        png.extend(std::iter::repeat_n(0u8, 8));
        assert_eq!(sniff_image_ext(&png), Some("png"));
        assert_eq!(sniff_image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(sniff_image_ext(b"GIF89a"), Some("gif"));
        assert_eq!(
            sniff_image_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("webp")
        );
        assert_eq!(sniff_image_ext(b"PK\x03\x04"), None);
        assert_eq!(sniff_image_ext(b"%PDF-1.7"), None);
    }
}

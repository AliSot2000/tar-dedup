use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::db::types::ContentId;

/// Staged/tar member name: `{base64(sha1)}.{base64(size)}.{original_ext}`.
pub fn content_id_from_digest(digest: &[u8; 20], size: u64, rel_path: &Path) -> ContentId {
    let hash_part = URL_SAFE_NO_PAD.encode(digest);
    let size_part = URL_SAFE_NO_PAD.encode(size.to_le_bytes());
    let ext = original_extension(rel_path);
    ContentId(format!("{hash_part}.{size_part}{ext}"))
}

fn original_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn appends_original_extension() {
        let digest = [0u8; 20];
        let id = content_id_from_digest(&digest, 42, Path::new("docs/report.pdf"));
        assert!(id.0.ends_with(".pdf"));
    }
}

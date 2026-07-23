use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::db::types::{ContentId, FileId};
use crate::error::{Error, Result};

/// SHA-1 digest → 20 bytes → 27 chars URL-safe base64 (no pad).
const HASH_B64_LEN: usize = 27;
/// `u64` / `i64` → 8 bytes → 11 chars URL-safe base64 (no pad).
const U64_B64_LEN: usize = 11;

/// Staged/tar member name: `{hash_b64}.{fsize_b64}.{fid_b64}.ext`.
pub fn content_id_from_digest(
    digest: &[u8; 20],
    size: u64,
    file_id: FileId,
    rel_path: &Path,
) -> ContentId {
    let hash_part = URL_SAFE_NO_PAD.encode(digest);
    let size_part = URL_SAFE_NO_PAD.encode(size.to_le_bytes());
    let fid_part = URL_SAFE_NO_PAD.encode(file_id.0.to_le_bytes());
    let ext = original_extension(rel_path);
    ContentId(format!("{hash_part}.{size_part}.{fid_part}{ext}"))
}

/// Sparse rewrite basename: `sp.{content_id}`.
pub fn sparse_member_name(content_id: &ContentId) -> String {
    format!("sp.{}", content_id.0)
}

/// `stage_dir/sp.{content_id}`.
pub fn sparse_stage_path(stage_dir: &Path, content_id: &ContentId) -> PathBuf {
    stage_dir.join(sparse_member_name(content_id))
}

/// Parse `{hash_b64}.{fsize_b64}.{fid_b64}.ext` back into `(digest, size, file_id, extension)`.
pub fn parse_content_id(content_id: &str) -> Result<([u8; 20], u64, FileId, String)> {
    let ext = original_extension(Path::new(content_id));
    let stem = content_id
        .strip_suffix(&ext)
        .ok_or_else(|| Error::Config(format!("invalid content id: {content_id}")))?;

    // Fixed layout: base64 URL-safe alphabet includes `_`, so do not split on `_`.
    let expected_stem = HASH_B64_LEN + 1 + U64_B64_LEN + 1 + U64_B64_LEN;
    if stem.len() != expected_stem
        || stem.as_bytes()[HASH_B64_LEN] != b'.'
        || stem.as_bytes()[HASH_B64_LEN + 1 + U64_B64_LEN] != b'.'
    {
        return Err(Error::Config(format!("invalid content id: {content_id}")));
    }

    let hash_part = &stem[..HASH_B64_LEN];
    let size_part = &stem[HASH_B64_LEN + 1..HASH_B64_LEN + 1 + U64_B64_LEN];
    let fid_part = &stem[HASH_B64_LEN + 1 + U64_B64_LEN + 1..];

    let digest: [u8; 20] = URL_SAFE_NO_PAD
        .decode(hash_part)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Config(format!("invalid content id: {content_id}")))?;
    let size = u64::from_le_bytes(
        URL_SAFE_NO_PAD
            .decode(size_part)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Config(format!("invalid content id: {content_id}")))?,
    );
    let file_id = FileId(i64::from_le_bytes(
        URL_SAFE_NO_PAD
            .decode(fid_part)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Config(format!("invalid content id: {content_id}")))?,
    ));

    Ok((digest, size, file_id, ext))
}

fn original_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default()
}

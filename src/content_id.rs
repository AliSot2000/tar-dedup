use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use crate::db::types::ContentId;

/// Stable staged filename derived from sha1 digest and file size.
pub fn content_id_from_digest(digest: &[u8; 20], size: u64) -> ContentId {
    let hash_part = URL_SAFE_NO_PAD.encode(digest);
    let size_part = URL_SAFE_NO_PAD.encode(size.to_le_bytes());
    ContentId(format!("{hash_part}.{size_part}"))
}

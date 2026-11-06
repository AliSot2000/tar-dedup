use std::path::Path;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::common::files::original_extension;
use crate::db::types::{ContentId, FileId};
use crate::error::{Error, Result};

/// SHA-1 digest → 20 bytes → 27 chars URL-safe base64 (no pad).
const HASH_B64_LEN: usize = 27;
/// `u64` / `i64` → 8 bytes → 11 chars URL-safe base64 (no pad).
const U64_B64_LEN: usize = 11;

/// Staged/tar member name: `{hash_b64}.{fsize_b64}.{fid_b64}{ext}` — the
/// extension (which already carries its leading dot) is appended verbatim, so a
/// file with no extension ends the name at the `fid`. If no digest is present,
/// the 27 characters for the hash are filled with 27 `;` characters.
pub fn content_id_from_digest(
    digest: Option<&[u8; 20]>,
    size: u64,
    file_id: FileId,
    rel_path: &Path,
) -> ContentId {
    let hash_part = match digest {
        Some(dg) => URL_SAFE_NO_PAD.encode(dg),
        None => ";".repeat(HASH_B64_LEN),
    };
    let size_part = URL_SAFE_NO_PAD.encode(size.to_le_bytes());
    let fid_part = URL_SAFE_NO_PAD.encode(file_id.0.to_le_bytes());
    let ext = original_extension(rel_path);
    ContentId(format!("{hash_part}.{size_part}.{fid_part}{ext}"))
}

/// Sparse rewrite basename: `sp.{content_id}`.
pub fn sparse_member_name(content_id: &ContentId) -> String {
    format!("sp.{}", content_id.0)
}

/// Parse `{hash_b64}.{fsize_b64}.{fid_b64}{ext}` back into
/// `(digest, size, file_id, extension)`. The first three fields have fixed
/// base64url lengths (27 + 1 + 11 + 1 + 11 = 51 chars), so the extension is
/// simply everything past byte 51 — it already carries its leading dot (or is
/// empty), so a no-extension id never gets its `fid` misread as an extension.
pub fn parse_content_id(content_id: &str) -> Result<(Option<[u8; 20]>, u64, FileId, String)> {
    // Fixed layout: base64 URL-safe alphabet includes `_`, so do not split on `_`.
    let stem_end = HASH_B64_LEN + 1 + U64_B64_LEN + 1 + U64_B64_LEN;
    if content_id.len() < stem_end
        || content_id.as_bytes()[HASH_B64_LEN] != b'.'
        || content_id.as_bytes()[HASH_B64_LEN + 1 + U64_B64_LEN] != b'.'
    {
        return Err(Error::Config(format!("incorrect len - invalid content id: {content_id}")));
    }

    let hash_part = &content_id[..HASH_B64_LEN];
    let size_part = &content_id[HASH_B64_LEN + 1..HASH_B64_LEN + 1 + U64_B64_LEN];
    let fid_part = &content_id[HASH_B64_LEN + 1 + U64_B64_LEN + 1..stem_end];
    let ext = content_id[stem_end..].to_string();

    let digest: Option<[u8; 20]> = if hash_part == ";".repeat(HASH_B64_LEN) {
        None
    } else {
        let res: [u8; 20] = URL_SAFE_NO_PAD
            .decode(hash_part)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Config(format!("digest invalid - invalid content id: {content_id}")))?;
        Some(res)
    };
    let size = u64::from_le_bytes(
        URL_SAFE_NO_PAD
            .decode(size_part)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Config(format!("size invalid - invalid content id: {content_id}")))?,
    );
    let file_id = FileId(i64::from_le_bytes(
        URL_SAFE_NO_PAD
            .decode(fid_part)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Config(format!("file id invalid - invalid content id: {content_id}")))?,
    ));

    Ok((digest, size, file_id, ext))
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(path: &str) -> (Option<[u8; 20]>, u64, FileId, String) {
        let digest = Some([7u8; 20]);
        let cid = content_id_from_digest(digest.as_ref(), 1234, FileId(42), Path::new(path));
        parse_content_id(&cid.0).expect("round-trip parse")
    }

    #[test]
    fn roundtrip_with_extension() {
        let (digest, size, fid, ext) = roundtrip("data.txt");
        assert_eq!(digest, Some([7u8; 20]));
        assert_eq!(size, 1234);
        assert_eq!(fid, FileId(42));
        assert_eq!(ext, ".txt");
    }

    #[test]
    fn no_extension_does_not_misread_fid() {
        // Empty extension: `{hash}.{size}.{fid}` with no trailing dot. The old
        // `original_extension`-based parser grabbed the final `fid` as the
        // extension and then failed the length check.
        let (digest, size, fid, ext) = roundtrip("noext");
        assert_eq!(digest, Some([7u8; 20]));
        assert_eq!(size, 1234);
        assert_eq!(fid, FileId(42));
        assert_eq!(ext, "");
    }

    #[test]
    fn multi_dot_extension_kept_verbatim() {
        let (_, _, _, ext) = roundtrip("a.tar.gz");
        assert_eq!(ext, ".gz");
    }

    #[test]
    fn absent_digest_parses_to_none() {
        let cid = content_id_from_digest(None, 0, FileId(1), Path::new("x.txt"));
        let (digest, size, fid, ext) = parse_content_id(&cid.0).expect("parse");
        assert_eq!(digest, None);
        assert_eq!(size, 0);
        assert_eq!(fid, FileId(1));
        assert_eq!(ext, ".txt");
    }

    #[test]
    fn malformed_content_ids_rejected() {
        for bad in ["", "short", "abc.def.ghi.txt"] {
            assert!(parse_content_id(bad).is_err(), "must reject {bad}");
        }
    }
}

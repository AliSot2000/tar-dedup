//! Module contains the necessary functionality to get all covered extended attributes of a given file.

use crate::error::{FileStatError, FileStatResult};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use posix_acl::{PosixACL, Qualifier};
use selinux::SecurityContext;
use serde::{Deserialize, Serialize};
use serde_json::from_str as serdej_from_str;
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::path::Path;
use thiserror::Error;
use xattrs;
use xattrs::symlink_set_xattr;
use xattrs::types::{BString, ZString};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    Utf8,
    Base64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct XattrDump {
    encoding: Encoding,
    data: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum PosixQualifierParserError {
    #[error("unexpected qualifier string: {0:?}")]
    UnexpectedString(String),

    #[error("invalid identifier in {context:?}: {source}")]
    InvalidIdentifier {
        context: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

/// Fetch the user xattrs of a given path. Returns a json string
/// The json structure is:
/// ```json
/// {
///   // if utf8, the key/value is utf8 encoded
///   "encoding": "base64", // or "utf8" when debugging
///   "data": {
///     "base64key1": "base64value1",
///     "base64key2": "base64value1",
///   }
/// }
pub fn get_file_xattr(path: &Path) -> FileStatResult<String> {
    let printable_path = path.to_string_lossy();
    let user_xattr = xattrs::symlink_list_xattr(path).map_err(|e| FileStatError::Xattrs {
        path: path.to_path_buf(),
        source: e,
    })?;

    let mut data = HashMap::new();

    for key in user_xattr.keys() {
        let unpacked_key: ZString = match key {
            Ok(k) => k,
            Err(e) => {
                println!("Error Processing xattrs for {printable_path}: {e}");
                continue;
            }
        };

        let unpacked_val: BString = match user_xattr.get(unpacked_key.clone()) {
            Ok(k) => k,
            Err(e) => {
                println!("Error Processing xattrs for {printable_path}: {e}");
                continue;
            }
        };

        let enc_key = encode_for_db(unpacked_key.as_os_str());
        let enc_val = encode_for_db(unpacked_val.as_os_str());
        data.insert(enc_key, enc_val);
    }

    let dump = XattrDump {
        encoding: if cfg!(feature = "debug-force-utf8") {
            Encoding::Utf8
        } else {
            Encoding::Base64
        },
        data,
    };

    Ok(
        serde_json::to_string(&dump).map_err(|e| FileStatError::Json {
            path: path.to_path_buf(),
            source: e,
        })?,
    )
}

#[cfg(not(feature = "debug-force-utf8"))]
fn encode_for_db(target: &OsStr) -> String {
    STANDARD.encode(target.as_encoded_bytes())
}

#[cfg(feature = "debug-force-utf8")]
fn encode_for_db(target: &OsStr) -> String {
    match target.to_str() {
        None => {
            panic!("Implementation error: INVARIANT broken. \
                    Previous call tor .to_str() successful, failed now");
        },
        Some(s) => String::from(s),
    }
}

/// Function attempts to read the POSIX_ACLs of a given path. The retrieved ACLS are then stored
/// as a json string.
/// Since the qualifiers follow an enum, they can safely be encoded in utf8. The permissions as
/// u32 are also natively supported by json
/// ```json
/// {
///     "user-obj": int,
///     "group-obj": int,
///     "user:1000": int,
/// }
pub fn get_file_acl(path: &Path) -> FileStatResult<String> {
    let entries = PosixACL::read_acl(path)
        .map_err(|e| {FileStatError::PosixAcl {path: path.to_path_buf(), source: e}})?
        .entries();
    let mut undef_count = 0;

    let mut json_repr: HashMap<String, u32> = HashMap::new();

    for entry in entries {
        let ser_qual = if entry.qual == Qualifier::Undefined {
            // We add a number to the undefined entries since don't know how many there are to
            // prevent key collisions.
            let temp = format!("{}-{undef_count}", serialize_acl_qualifier(entry.qual));
            undef_count += 1;
            temp
        } else {
            serialize_acl_qualifier(entry.qual)
        };
        json_repr.insert(ser_qual, entry.perm);
    }
    Ok(
        serde_json::to_string(&json_repr).map_err(|e| FileStatError::Json {
            path: path.to_path_buf(),
            source: e,
        })?,
    )
}

fn serialize_acl_qualifier(qualifier: Qualifier) -> String {
    match qualifier {
        Qualifier::Undefined => "undefined".to_string(),
        Qualifier::UserObj => "user-obj".to_string(),
        Qualifier::GroupObj => "group-obj".to_string(),
        Qualifier::Other => "other".to_string(),
        Qualifier::Mask => "mask".to_string(),
        Qualifier::User(uid) => format!("user:{uid}").to_string(),
        Qualifier::Group(gid) => format!("group:{gid}").to_string(),
    }
}

fn deserialize_to_acl_qualifier(qualifier: &str) -> Result<Qualifier, PosixQualifierParserError> {
    match qualifier.split_once(":") {
        None => match qualifier {
            "undefined" => Ok(Qualifier::Undefined),
            "user-obj" => Ok(Qualifier::UserObj),
            "group-obj" => Ok(Qualifier::GroupObj),
            "other" => Ok(Qualifier::Other),
            "mask" => Ok(Qualifier::Mask),
            other => Err(PosixQualifierParserError::UnexpectedString(format!(
                "Given String {other} is not a valid Qualifier for PosixACLs"
            ))),
        },
        Some(("user", id)) => {
            let uid = id.parse::<u32>();
            match uid {
                Ok(u) => Ok(Qualifier::User(u)),
                Err(e) => Err(PosixQualifierParserError::InvalidIdentifier {
                    context: format!("user:{id}"),
                    source: e,
                }),
            }
        }
        Some(("group", id)) => {
            let gid = id.parse::<u32>();
            match gid {
                Ok(g) => Ok(Qualifier::Group(g)),
                Err(e) => Err(PosixQualifierParserError::InvalidIdentifier {
                    context: format!("group:{id}"),
                    source: e,
                }),
            }
        }
        Some((l, r)) => Err(PosixQualifierParserError::UnexpectedString(format!(
            "{l}:{r}"
        ))),
    }
}

/// Read SELinux Security Context, if exists.
pub fn get_file_selinux_data(path: &Path) -> FileStatResult<Vec<u8>> {
    let octx = SecurityContext::of_path(path, false, false)
        .map_err(|e| {FileStatError::SELinux {path: path.to_path_buf(), source: e}})?;
    match octx {
        None => Ok(Vec::new()),
        Some(ctx) => Ok(ctx.as_bytes().to_vec()),
    }
}

/// Function expects a json structure from `get_file_xattr`
pub fn set_file_xattrs(path: &Path, raw_xattr: &str) -> FileStatResult<()> {
    let dump: XattrDump = serde_json::from_str(raw_xattr).map_err(|e| FileStatError::Json {
        path: path.to_path_buf(),
        source: e,
    })?;

    match dump.encoding {
        Encoding::Utf8 => {
            for (k, v) in dump.data {
                // k, v are already valid UTF-8 strings, use directly
                symlink_set_xattr(path, k, v).map_err(|e| FileStatError::Xattrs {
                    path: path.to_path_buf(),
                    source: e,
                })?
            }
        }
        Encoding::Base64 => {
            for (k, v) in dump.data {
                let key_bytes =
                    STANDARD
                        .decode(&k)
                        .map_err(|e| FileStatError::Base64DecodingError {
                            path: path.to_path_buf(),
                            source: e,
                        })?;
                let val_bytes =
                    STANDARD
                        .decode(&v)
                        .map_err(|e| FileStatError::Base64DecodingError {
                            path: path.to_path_buf(),
                            source: e,
                        })?;
                // use key_bytes / val_bytes to actually call setxattr
                symlink_set_xattr(path, key_bytes, val_bytes).map_err(|e| {
                    FileStatError::Xattrs {
                        path: path.to_path_buf(),
                        source: e,
                    }
                })?
            }
        }
    }

    Ok(())
}

/// Function expects a json structure from `get_file_acl`.
/// The string is parsed, the ACL strucure rebuilt and lastly written to the given `path`
pub fn set_file_acl(path: &Path, raw_acl: &str) -> FileStatResult<()> {
    let parsed_json: HashMap<String, u32> = serdej_from_str(raw_acl)
        .map_err(|e| FileStatError::Json {
            path: path.to_path_buf(),
            source: e,
        })?;
    let mut new_acls = PosixACL::empty();

    // Rebuild ACL
    for (key, value) in parsed_json {
        // To deal with possible duplicate 'undefined' values, we append a -int suffix to
        // distinguish them to prevent key collisions in the json subject that stores the acl
        let qual_str = if let Some(_suffix) = key.strip_prefix("undefined-") {
            "undefined"
        } else {
            key.as_str()
        };

        let qualifier = deserialize_to_acl_qualifier(qual_str).map_err(|e| {
            FileStatError::PosixQualifierParser {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
        // Guard before `PosixACL::set`: an out-of-range perm makes `acl_add_perm`
        // fail and the crate's `check_return` panics instead of returning Err.
        if value & !0b111 != 0 {
            return Err(FileStatError::General {
                path: Some(path.to_path_buf()),
                message: format!("invalid ACL permission {value}"),
            });
        }
        new_acls.set(qualifier, value);
    }
    new_acls
        .write_acl(path)
        .map_err(|e| FileStatError::PosixAcl {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(())
}

/// Apply stored raw security context. WARNING: No validation is performed. Assumption is,
/// data originated from the above `get_file_selinux_data` and not anything else.
pub fn set_file_selinux_data(path: &Path, raw_ctx: &[u8]) -> FileStatResult<()> {
    let c_string = CString::new(raw_ctx).map_err(|e| FileStatError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    let parsed_ctx = SecurityContext::from_c_str(&c_string, true);
    parsed_ctx
        .set_for_path(&path, false, false)
        .map_err(|e| FileStatError::SELinux {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Testing
//--------------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- environment / support probes ------------------------------------------

    /// Per-test scratch dir. Honors `TAR_DEDUP_TEST_DIR` (a labelled filesystem on the
    /// VM — SELinux cannot be stored on tmpfs); otherwise a tempdir. The returned
    /// `TempDir` (when present) is kept alive by the caller.
    fn ctx(name: &str) -> (Option<tempfile::TempDir>, PathBuf) {
        match std::env::var_os("TAR_DEDUP_TEST_DIR") {
            Some(base) => {
                let base = PathBuf::from(base);
                std::fs::create_dir_all(&base).expect("create TAR_DEDUP_TEST_DIR");
                let dir = base.join(name);
                std::fs::create_dir_all(&dir).expect("create per-test dir");
                (None, dir)
            }
            None => {
                let td = tempfile::tempdir().expect("tempdir");
                let dir = td.path().join(name);
                std::fs::create_dir_all(&dir).expect("create per-test dir");
                (Some(td), dir)
            }
        }
    }

    /// Probe: can we store a user xattr here? Any failure means the fs lacks support.
    fn probe_xattr(dir: &Path) -> Result<(), String> {
        let probe = dir.join("probe_xattr");
        std::fs::write(&probe, b"x").expect("write probe");
        match symlink_set_xattr(probe.as_path(), "probe", b"v") {
            Ok(()) => Ok(()),
            Err(e) => Err(format!("{e}")),
        }
    }

    /// Probe: can we store a POSIX ACL here? (`write_acl` fails on fs without ACLs.)
    fn probe_acl(dir: &Path) -> Result<(), String> {
        let probe = dir.join("probe_acl");
        std::fs::write(&probe, b"x").expect("write probe");
        let mut acl = posix_acl::PosixACL::empty();
        acl.set(Qualifier::UserObj, 7);
        acl.set(Qualifier::GroupObj, 5);
        acl.set(Qualifier::Mask, 5);
        acl.set(Qualifier::Other, 0);
        match acl.write_acl(&probe) {
            Ok(()) => Ok(()),
            Err(e) => Err(format!("{e}")),
        }
    }

    /// Probe: can we read + re-apply a security context here? Reads whatever label a
    /// fresh file got (proves labels exist) and writes it back (proves privilege).
    fn probe_selinux(dir: &Path) -> Result<(), String> {
        let probe = dir.join("probe_selinux");
        std::fs::write(&probe, b"x").expect("write probe");
        let octx = SecurityContext::of_path(&probe, false, false).map_err(|e| format!("{e}"))?;
        match octx {
            None => Err("no security context (filesystem lacks labels)".to_string()),
            Some(ctx) => {
                let bytes = ctx.as_bytes().to_vec();
                set_file_selinux_data(&probe, &bytes).map_err(|e| format!("{e}"))?;
                Ok(())
            }
        }
    }

    // ---- small writers / decoders ----------------------------------------------

    fn write_user_xattr(path: &Path, name: &str, value: &[u8]) {
        symlink_set_xattr(path, name, value).expect("set user xattr");
    }

    fn write_acl(path: &Path, entries: &[(Qualifier, u32)]) {
        let mut acl = posix_acl::PosixACL::empty();
        for (q, p) in entries {
            acl.set(*q, *p);
        }
        acl.write_acl(path).expect("write acl");
    }

    fn parse_xattr_data(s: &str) -> HashMap<String, String> {
        serdej_from_str::<XattrDump>(s).expect("parse xattr dump").data
    }

    /// Decode a dump's base64 keys/values back to plain strings (handles the
    /// `debug-force-utf8` encoding too).
    fn decoded_xattr_map(s: &str) -> HashMap<String, Vec<u8>> {
        let dump: XattrDump = serdej_from_str(s).expect("parse xattr dump");
        match dump.encoding {
            Encoding::Utf8 => dump.data
                .into_iter()
                .map(|(k, v)| (k, v.into_bytes()))
                .collect(),
            Encoding::Base64 => dump.data
                .into_iter()
                .map(|(k, v)| {
                    let k = String::from_utf8(STANDARD.decode(&k).expect("decode key"))
                        .expect("utf8 key");
                    (k, STANDARD.decode(&v).expect("decode value"))
                })
                .collect(),
        }
    }

    fn parse_acl_map(s: &str) -> HashMap<String, u32> {
        serdej_from_str::<HashMap<String, u32>>(s).expect("parse acl map")
    }

    /// Variants of a security context to exhaust: base, + MLS level, + categories,
    /// + low-high range. Built from the default label so `user:role:type` are valid
    /// for the running policy.
    fn selinux_context_variants(default: &[u8]) -> Vec<Vec<u8>> {
        let s = std::str::from_utf8(default).expect("selinux context is utf8");
        let base: Vec<&str> = s.split(':').take(3).collect();
        let base = base.join(":");
        vec![
            default.to_vec(),
            format!("{base}:s0").into_bytes(),
            format!("{base}:s0:c0.c1023").into_bytes(),
            format!("{base}:s0-s15:c0.c1023").into_bytes(),
        ]
    }

    /// capture(f1) -> restore(f2) -> recapture(f2) byte equality for SELinux.
    fn assert_selinux_round_trip(dir: &Path, ctx: &[u8], idx: usize) {
        let f1 = dir.join(format!("a{idx}.bin"));
        let f2 = dir.join(format!("b{idx}.bin"));
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        set_file_selinux_data(&f1, ctx).expect("set context");
        let captured = get_file_selinux_data(&f1).expect("capture context");
        set_file_selinux_data(&f2, &captured).expect("restore context");
        assert_eq!(captured, get_file_selinux_data(&f2).expect("recapture context"));
    }

    // ---- xattr round trips ------------------------------------------------------

    #[test]
    fn xattr_round_trip_single_utf8() {
        let (_td, dir) = ctx("xattr_round_trip_single_utf8");
        if let Err(e) = probe_xattr(&dir) {
            eprintln!("skipped (unsupported): xattr: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        write_user_xattr(&f1, "meta.color", b"blue");

        let raw = get_file_xattr(&f1).expect("capture xattrs");
        set_file_xattrs(&f2, &raw).expect("restore xattrs");
        assert_eq!(parse_xattr_data(&raw), parse_xattr_data(&get_file_xattr(&f2).expect("recapture")));
    }

    #[test]
    fn xattr_round_trip_many_keys_and_binary_values() {
        let (_td, dir) = ctx("xattr_round_trip_many_keys");
        if let Err(e) = probe_xattr(&dir) {
            eprintln!("skipped (unsupported): xattr: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        write_user_xattr(&f1, "meta.k1", b"hello");
        write_user_xattr(&f1, "meta.k2", b"");
        write_user_xattr(&f1, "meta.k3", &[0x00, 0xff, 0x80, 0x41]);
        write_user_xattr(&f1, "meta.k4", &[0x7f; 512]);

        let raw = get_file_xattr(&f1).expect("capture xattrs");
        set_file_xattrs(&f2, &raw).expect("restore xattrs");
        assert_eq!(parse_xattr_data(&raw), parse_xattr_data(&get_file_xattr(&f2).expect("recapture")));
    }

    #[test]
    fn xattr_round_trip_both_encodings() {
        let (_td, dir) = ctx("xattr_round_trip_both_encodings");
        if let Err(e) = probe_xattr(&dir) {
            eprintln!("skipped (unsupported): xattr: {e}");
            return;
        }
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");

        // utf8 branch of set_file_xattrs.
        set_file_xattrs(&f, r#"{"encoding":"utf8","data":{"meta.t":"tag","meta.e":""}}"#)
            .expect("set utf8-encoded xattrs");
        assert_eq!(
            decoded_xattr_map(&get_file_xattr(&f).expect("capture")),
            HashMap::from([
                ("meta.t".to_string(), b"tag".to_vec()),
                ("meta.e".to_string(), Vec::new()),
            ]),
        );

        // base64 branch with a binary value (fresh file so the utf8 xattrs above
        // don't accumulate).
        let f2 = dir.join("b.bin");
        std::fs::write(&f2, b"x").expect("write");
        let key = STANDARD.encode("meta.bin");
        let val = STANDARD.encode([0x00u8, 0xff, 0x80, 0x41]);
        let json = serde_json::json!({ "encoding": "base64", "data": { key: val } }).to_string();
        set_file_xattrs(&f2, &json).expect("set base64-encoded xattrs");
        assert_eq!(
            decoded_xattr_map(&get_file_xattr(&f2).expect("capture")),
            HashMap::from([("meta.bin".to_string(), vec![0x00, 0xff, 0x80, 0x41])]),
        );
    }

    // ---- ACL round trips --------------------------------------------------------

    #[test]
    fn acl_round_trip_minimal_mode() {
        let (_td, dir) = ctx("acl_round_trip_minimal_mode");
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        write_acl(&f1, &[
            (Qualifier::UserObj, 6),
            (Qualifier::GroupObj, 4),
            (Qualifier::Other, 0),
        ]);

        let raw = get_file_acl(&f1).expect("capture acl");
        set_file_acl(&f2, &raw).expect("restore acl");
        assert_eq!(parse_acl_map(&raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
    }

    #[test]
    fn acl_round_trip_all_entry_types() {
        let (_td, dir) = ctx("acl_round_trip_all_entry_types");
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        // mask = union of group-class perms (5 | 6 | 4 = 7) so fix_mask is a no-op.
        write_acl(&f1, &[
            (Qualifier::UserObj, 7),
            (Qualifier::GroupObj, 5),
            (Qualifier::Other, 4),
            (Qualifier::Mask, 7),
            (Qualifier::User(1000), 6),
            (Qualifier::Group(2000), 4),
        ]);

        let raw = get_file_acl(&f1).expect("capture acl");
        set_file_acl(&f2, &raw).expect("restore acl");
        assert_eq!(parse_acl_map(&raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
    }

    #[test]
    fn acl_round_trip_repeated_named_users() {
        let (_td, dir) = ctx("acl_round_trip_repeated_named_users");
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        // union of group-class (6 | 4 | 2 = 6) -> mask 6.
        write_acl(&f1, &[
            (Qualifier::UserObj, 7),
            (Qualifier::GroupObj, 0),
            (Qualifier::Other, 0),
            (Qualifier::Mask, 6),
            (Qualifier::User(1000), 6),
            (Qualifier::User(1001), 4),
            (Qualifier::User(1002), 2),
        ]);

        let raw = get_file_acl(&f1).expect("capture acl");
        set_file_acl(&f2, &raw).expect("restore acl");
        assert_eq!(parse_acl_map(&raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
    }

    #[test]
    fn acl_round_trip_repeated_named_groups() {
        let (_td, dir) = ctx("acl_round_trip_repeated_named_groups");
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        // union of group-class (5 | 6 | 4 | 2 = 7) -> mask 7.
        write_acl(&f1, &[
            (Qualifier::UserObj, 7),
            (Qualifier::GroupObj, 5),
            (Qualifier::Other, 1),
            (Qualifier::Mask, 7),
            (Qualifier::Group(2000), 6),
            (Qualifier::Group(2001), 4),
            (Qualifier::Group(2002), 2),
        ]);

        let raw = get_file_acl(&f1).expect("capture acl");
        set_file_acl(&f2, &raw).expect("restore acl");
        assert_eq!(parse_acl_map(&raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
    }

    #[test]
    fn acl_round_trip_perm_combinations() {
        let (_td, dir) = ctx("acl_round_trip_perm_combinations");
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");
        // union of group-class (4 | 0..7 = 7) -> mask 7.
        write_acl(&f1, &[
            (Qualifier::UserObj, 7),
            (Qualifier::GroupObj, 4),
            (Qualifier::Other, 0),
            (Qualifier::Mask, 7),
            (Qualifier::User(1000), 0),
            (Qualifier::User(1001), 1),
            (Qualifier::User(1002), 2),
            (Qualifier::User(1003), 3),
            (Qualifier::User(1004), 4),
            (Qualifier::User(1005), 5),
            (Qualifier::User(1006), 6),
            (Qualifier::User(1007), 7),
        ]);

        let raw = get_file_acl(&f1).expect("capture acl");
        set_file_acl(&f2, &raw).expect("restore acl");
        assert_eq!(parse_acl_map(&raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
    }

    // ---- SELinux round trips ---------------------------------------------------

    #[test]
    fn selinux_round_trip_all_context_shapes() {
        let (_td, dir) = ctx("selinux_round_trip_shapes");
        if let Err(e) = probe_selinux(&dir) {
            eprintln!("skipped (unsupported): selinux: {e}");
            return;
        }
        let probe = dir.join("ctx_probe");
        std::fs::write(&probe, b"x").expect("write");
        let default = get_file_selinux_data(&probe).expect("get default context");
        for (i, variant) in selinux_context_variants(&default).into_iter().enumerate() {
            assert_selinux_round_trip(&dir, &variant, i);
        }
    }

    // ---- everything at once -----------------------------------------------------

    #[test]
    fn all_attributes_round_trip_on_single_file() {
        let (_td, dir) = ctx("all_attributes_at_once");
        if let Err(e) = probe_xattr(&dir) {
            eprintln!("skipped (unsupported): xattr: {e}");
            return;
        }
        if let Err(e) = probe_acl(&dir) {
            eprintln!("skipped (unsupported): acl: {e}");
            return;
        }
        let f1 = dir.join("a.bin");
        let f2 = dir.join("b.bin");
        std::fs::write(&f1, b"x").expect("write");
        std::fs::write(&f2, b"x").expect("write");

        // xattrs.
        write_user_xattr(&f1, "meta.t", b"tag");
        write_user_xattr(&f1, "meta.bin", &[0x00, 0xff, 0x01]);

        // ACL: all entry types with repeated named users/groups (mask = 7|...| = 7).
        write_acl(&f1, &[
            (Qualifier::UserObj, 7),
            (Qualifier::GroupObj, 5),
            (Qualifier::Other, 4),
            (Qualifier::Mask, 7),
            (Qualifier::User(1000), 6),
            (Qualifier::User(1001), 4),
            (Qualifier::Group(2000), 4),
            (Qualifier::Group(2001), 2),
        ]);

        // SELinux (best-effort; xattr + ACL must round-trip regardless).
        let selinux_capture = match get_file_selinux_data(&f1) {
            Ok(v) if !v.is_empty() => Some(v),
            Ok(_) => None,
            Err(e) => {
                eprintln!("skipped (unsupported): selinux: {e}");
                None
            }
        };

        let xattr_raw = get_file_xattr(&f1).expect("capture xattrs");
        let acl_raw = get_file_acl(&f1).expect("capture acl");

        set_file_xattrs(&f2, &xattr_raw).expect("restore xattrs");
        set_file_acl(&f2, &acl_raw).expect("restore acl");
        if let Some(ctx) = &selinux_capture {
            set_file_selinux_data(&f2, ctx).expect("restore selinux");
        }

        assert_eq!(parse_xattr_data(&xattr_raw), parse_xattr_data(&get_file_xattr(&f2).expect("recapture")));
        assert_eq!(parse_acl_map(&acl_raw), parse_acl_map(&get_file_acl(&f2).expect("recapture")));
        if let Some(ctx) = selinux_capture {
            assert_eq!(ctx, get_file_selinux_data(&f2).expect("recapture selinux"));
        }
    }

    // ---- malformed: SELinux -----------------------------------------------------

    #[test]
    fn selinux_malformed_interior_nul() {
        let (_td, dir) = ctx("selinux_malformed_nul");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        let res = set_file_selinux_data(&f, b"u:r:t\0s0");
        assert!(matches!(res, Err(FileStatError::Io { .. })), "got {res:?}");
    }

    #[test]
    fn selinux_malformed_empty() {
        let (_td, dir) = ctx("selinux_malformed_empty");
        if let Err(e) = probe_selinux(&dir) {
            eprintln!("skipped (unsupported): selinux: {e}");
            return;
        }
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        let res = set_file_selinux_data(&f, b"");
        assert!(res.is_err(), "got {res:?}");
    }

    #[test]
    fn selinux_malformed_context_loop() {
        let (_td, dir) = ctx("selinux_malformed_context_loop");
        if let Err(e) = probe_selinux(&dir) {
            eprintln!("skipped (unsupported): selinux: {e}");
            return;
        }
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        for bad in [
            "not-a-valid-context",
            "u:r",
            "u:r:t:x",
            ":::::",
            "u:role:type:s0:c0.c1023:extra",
        ] {
            let res = set_file_selinux_data(&f, bad.as_bytes());
            assert!(res.is_err(), "expected error for {bad:?}, got {res:?}");
        }
    }

    // ---- malformed: POSIX ACL ---------------------------------------------------

    #[test]
    fn acl_malformed_bad_uid() {
        let (_td, dir) = ctx("acl_malformed_bad_uid");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        let res = set_file_acl(&f, r#"{"user:abc":7}"#);
        assert!(matches!(res, Err(FileStatError::PosixQualifierParser { .. })), "got {res:?}");
    }

    #[test]
    fn acl_malformed_unknown_qualifier() {
        let (_td, dir) = ctx("acl_malformed_unknown_qualifier");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        let res = set_file_acl(&f, r#"{"bogus":7}"#);
        assert!(matches!(res, Err(FileStatError::PosixQualifierParser { .. })), "got {res:?}");
    }

    #[test]
    fn acl_malformed_not_a_map() {
        let (_td, dir) = ctx("acl_malformed_not_a_map");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        let res = set_file_acl(&f, "[1,2]");
        assert!(matches!(res, Err(FileStatError::Json { .. })), "got {res:?}");
    }

    #[test]
    fn acl_malformed_missing_mandatory() {
        let (_td, dir) = ctx("acl_malformed_missing_mandatory");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        // Fails in-memory in write_acl's validate() (before any fs write).
        let res = set_file_acl(&f, r#"{"user:1000":7}"#);
        assert!(matches!(res, Err(FileStatError::PosixAcl { .. })), "got {res:?}");
    }

    #[test]
    fn acl_malformed_repetition() {
        let (_td, dir) = ctx("acl_malformed_repetition");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        // Several entries, one bad: the bad qualifier surfaces regardless of
        // HashMap iteration order.
        let res = set_file_acl(&f, r#"{"user:abc":7,"group-obj":4,"other":0}"#);
        assert!(matches!(res, Err(FileStatError::PosixQualifierParser { .. })), "got {res:?}");
    }

    #[test]
    fn acl_malformed_perm_out_of_range() {
        let (_td, dir) = ctx("acl_malformed_perm_out_of_range");
        let f = dir.join("a.bin");
        std::fs::write(&f, b"x").expect("write");
        // Guarded before PosixACL::set (which would panic via check_return).
        let res = set_file_acl(&f, r#"{"user-obj":8}"#);
        assert!(matches!(res, Err(FileStatError::General { .. })), "got {res:?}");
    }
}

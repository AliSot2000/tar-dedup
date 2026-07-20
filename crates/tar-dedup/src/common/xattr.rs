//! Module contains the necessary functionality to get all covered extended attributes of a given file.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use posix_acl::{PosixACL, Qualifier};
use selinux::SecurityContext;
use serde::{Deserialize, Serialize};
use serde_json::from_str as serdej_from_str;
use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fmt::Display;
use std::io;
use std::path::Path;
use thiserror::Error;
use xattrs;
use xattrs::symlink_set_xattr;
use xattrs::types::{BString, ZString};
use crate::error::{FileStatError, FileStatResult};

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

// Encoding constants.
#[cfg(feature = "debug-force-utf8")]
const ENCODING: &str = "utf8";
#[cfg(not(feature = "debug-force-utf8"))]
const ENCODING: &str = "base64";

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
    let user_xattr = xattrs::symlink_list_xattr(path)
        .map_err(|e| {FileStatError::Xattrs{path: path.to_path_buf(), source: e}})?;

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
        serde_json::to_string(&dump)
        .map_err(|e| {FileStatError::Json {path: path.to_path_buf(), source: e}})?
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
        serde_json::to_string(&json_repr)
            .map_err(|e| {FileStatError::Json {path: path.to_path_buf(), source: e}})?
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
            let uid =  id.parse::<u32>();
            match uid {
                Ok(u) => Ok(Qualifier::User(u)),
                Err(e) => Err(PosixQualifierParserError::InvalidIdentifier {
                    context: format!("user:{id}"),
                    source: e,
                })
            }
        },
        Some(("group", id)) => {
            let gid = id.parse::<u32>();
            match gid {
                Ok(g) => Ok(Qualifier::Group(g)),
                Err(e) => Err(PosixQualifierParserError::InvalidIdentifier {
                    context: format!("group:{id}"),
                    source: e,
                })
            }
        }
        Some((l, r)) => Err(PosixQualifierParserError::UnexpectedString(
            format!("{l}:{r}"))),
    }
}


/// Read SELinux Security Context, if exists.
pub fn get_file_selinux_data(path: &Path) -> FileStatResult<Vec<u8>> {
    let octx = SecurityContext::of_path(
        path,
        false,
        false)
        .map_err(|e| {FileStatError::SELinux {path: path.to_path_buf(), source: e}})?;
    match octx {
        None => Ok(Vec::new()),
        Some(ctx) => Ok(ctx.as_bytes().to_vec())
    }
}

/// Function expects a json structure from `get_file_xattr`
pub fn set_file_xattrs(path: &Path, raw_xattr: &str) -> FileStatResult<()> {
    let dump: XattrDump = serde_json::from_str(raw_xattr)
        .map_err(|e| {FileStatError::Json {path: path.to_path_buf(), source: e}})?;

    match dump.encoding {
        Encoding::Utf8 => {
            for (k, v) in dump.data {
                // k, v are already valid UTF-8 strings, use directly
                symlink_set_xattr(path, k, v)
                    .map_err(|e| {FileStatError::Xattrs {
                        path: path.to_path_buf(),
                        source: e}})?
            }
        }
        Encoding::Base64 => {
            for (k, v) in dump.data {
                let key_bytes = STANDARD.decode(&k)
                    .map_err(|e| {FileStatError::Base64DecodinggError {
                        path: path.to_path_buf(),
                        source: e}})?;
                let val_bytes = STANDARD.decode(&v)
                    .map_err(|e| {FileStatError::Base64DecodinggError {
                        path: path.to_path_buf(),
                        source: e}})?;
                // use key_bytes / val_bytes to actually call setxattr
                symlink_set_xattr(path, key_bytes, val_bytes)
                    .map_err(|e| {FileStatError::Xattrs {
                        path: path.to_path_buf(),
                        source: e}})?
            }
        }
    }

    Ok(())
}

/// Function expects a json structure from `get_file_acl`.
/// The string is parsed, the ACL strucure rebuilt and lastly written to the given `path`
pub fn set_file_acl(path: &Path, raw_acl: &str) -> FileStatResult<()> {
    let parsed_json: HashMap<String, u32> = serdej_from_str(raw_acl)
        .map_err(|e| {FileStatError::Json {
            path: path.to_path_buf(),
            source: e,
        }})?;
    let mut new_acls = PosixACL::empty();

    // Rebuild ACL
    for (key, value) in parsed_json {
        // To deal with possible duplicate 'undefined' values, we append a -int suffix to
        // distinguish them to prevent key collisions in the json subject that stores the acl
        let qual_str = if let Some(suffix) = key.strip_prefix("undefined-") {
            "undefined"
        } else {
            key.as_str()
        };


        let qualifier = deserialize_to_acl_qualifier(qual_str)
            .map_err(|e| {FileStatError::PosixQualifierParser {
                path: path.to_path_buf(),
                source: e
            }})?;
        new_acls.set(qualifier, value);
    }
    new_acls.write_acl(path)
        .map_err(|e| {FileStatError::PosixAcl {path: path.to_path_buf(), source: e}})?;
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
    parsed_ctx.set_for_path(&path, false, false)
        .map_err(|e| {FileStatError::SELinux {path: path.to_path_buf(), source: e}})?;
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Testing
//--------------------------------------------------------------------------------------------------
// TODO!!!

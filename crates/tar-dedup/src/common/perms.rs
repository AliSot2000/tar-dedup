//! Owner / group identity maps: parsing (CLI + map files) and extraction-time resolution.
//!
//! Pure logic, no DB access. The maps are plain data ([`OwnerGroupPolicy`], [`IdentityMap`],
//! [`IdentitySpec`]) that can be serialized to the `meta` table; the persistence lives in
//! `db/meta.rs`. Reused by both the archive (persist) and extract (resolve) commands.
//!
//! Map-file grammar (GNU tar `--owner-map` / `--group-map`), symmetric for both fields:
//!   `FROM <ws>+ TO`, one entry per line; blank lines and `#`-to-end-of-line comments ignored.
//!   `FROM` / `TO` are each an [`IdentitySpec`]: `+UID`, `NAME`, or `NAME:UID`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One side of a map row, or an `--owner` / `--group` override value.
///
/// Grammar: `+UID` → [`Self::id`]; `NAME` → [`Self::name`]; `NAME:UID` → both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdentitySpec {
    pub name: Option<String>,
    pub id: Option<u32>,
}

impl IdentitySpec {
    /// Parse `+UID`, `NAME`, or `NAME:UID`.
    ///
    /// A bare all-digit name is accepted (as a name) with a warning that `+UID` was
    /// probably intended — the suggested "footgun guard".
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::Config("empty identity spec".into()));
        }
        if let Some(rest) = s.strip_prefix('+') {
            let id = rest.parse::<u32>().map_err(|_| {
                Error::Config(format!("invalid numeric suffix in identity spec `{s}`"))
            })?;
            return Ok(Self { name: None, id: Some(id) });
        }
        match s.split_once(":") {
            None => {
                if s.parse::<u32>().ok().is_some() {
                    tracing::warn!(
                        "identity spec `{s}` looks numeric; did you mean `+{s}` (a uid)?"
                    );
                }
                Ok(Self { name: Some(s.to_string()), id: None })
            }
            Some((name, id)) => {
                let name = if name.is_empty() { None } else { Some(name.to_string()) };
                let id = if id.is_empty() {
                    None
                } else {
                    Some(id.parse::<u32>().map_err(|_| {
                        Error::Config(format!("invalid id in identity spec `{s}`"))
                    })?)
                };
                if name.is_none() && id.is_none() {
                    return Err(Error::Config(format!(
                        "identity spec `{s}` must carry a name or an id"
                    )));
                }
                Ok(Self { name, id })
            }
        }
    }
}

/// A translation table. Values are the destination [`IdentitySpec`].
///
/// A source spec is inserted under both keys when it carries both a name and an id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMap {
    /// `source name → destination spec`.
    pub by_name: HashMap<String, IdentitySpec>,
    /// `source id → destination spec`.
    pub by_id: HashMap<u32, IdentitySpec>,
}

impl IdentityMap {
    /// Fold `from → to` entries into the table. `from` with a name populates [`Self::by_name`],
    /// `from` with an id populates [`Self::by_id`] (a `NAME:UID` source populates both).
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (IdentitySpec, IdentitySpec)>,
    {
        let mut map = Self::default();
        for (from, to) in entries {
            debug_assert!(
                from.name.is_some() || from.id.is_some(),
                "IdentitySpec source must carry a name or an id"
            );
            debug_assert!(
                to.name.is_some() || to.id.is_some(),
                "IdentitySpec destination must carry a name or an id"
            );
            if let Some(name) = &from.name {
                map.by_name.insert(name.clone(), to.clone());
            }
            if let Some(id) = &from.id {
                map.by_id.insert(*id, to.clone());
            }
        }
        map
    }
}

/// How the chosen identity is emitted during extraction. Extract-only runtime state;
/// never persisted to the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MapResolutionTarget {
    /// Always emit a numeric id. Errors on a name-only destination (parse/load when
    /// validation is enabled, warnings otherwise).
    Ids,
    /// Always emit a name. Falls back to override → db name; a missing db name is an
    /// error on apply (no numeric upgrade).
    Names,
    /// Emit a name first, fall back to an id. Default.
    #[default]
    NameId,
    /// Emit an id first, fall back to a name.
    IdName,
}

impl FromStr for MapResolutionTarget {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ids" => Ok(Self::Ids),
            "names" => Ok(Self::Names),
            "name-id" | "name_id" => Ok(Self::NameId),
            "id-name" | "id_name" => Ok(Self::IdName),
            other => Err(format!(
                "invalid --map-target `{other}` (expected ids, names, name-id, or id-name)"
            )),
        }
    }
}

impl std::fmt::Display for MapResolutionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl MapResolutionTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ids => "ids",
            Self::Names => "names",
            Self::NameId => "name-id",
            Self::IdName => "id-name",
        }
    }
}

/// The full owner/group policy: two tables plus overrides.
///
/// Any of the four may be present (mirrors tar's `--owner`/`--group`/`--owner-map`/
/// `--group-map`). Stored verbatim in the archive `meta` table and applied at extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGroupPolicy {
    pub owner_map: Option<IdentityMap>,
    pub group_map: Option<IdentityMap>,
    pub owner_override: Option<IdentitySpec>,
    pub group_override: Option<IdentitySpec>,
}

impl OwnerGroupPolicy {
    /// True when at least one of the four identity sources is present.
    pub fn at_least_one_present(&self) -> bool {
        self.owner_map.is_some()
            || self.group_map.is_some()
            || self.owner_override.is_some()
            || self.group_override.is_some()
    }
}

/// Which user/group policy applies during extraction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OwnerGroupSource {
    /// No mapping.
    #[default]
    None,
    /// Apply the policy stored in the archive.
    Stored,
    /// Apply an explicitly provided policy.
    Cli(OwnerGroupPolicy),
}

/// Which mode-change policy applies during extraction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModeSource {
    /// No mode changes.
    #[default]
    None,
    /// Apply the changes stored in the archive.
    Stored,
    /// Apply explicitly provided chmod-style changes (validated on the CLI).
    Cli(String),
}

/// Parse a GNU tar `--mode` changes string into a reusable [`file_mode::Mode`].
/// `Mode::empty()` + `set_str` yields a mask-based change set; `apply_to(file_mode)`
/// applies it per file.
///
/// Two forms are accepted, mirroring `tar` / `chmod`:
/// - symbolic changes, e.g. `u+rwx,go-rx`;
/// - a bare octal absolute mode, e.g. `0644` / `4755`, applied as a full set
///   (including setuid/setgid/sticky when a 4th digit is present).
///
/// The string is pre-validated against the chmod alphabet: `file-mode` panics
/// (abort in release builds) on an invalid permission char rather than returning
/// `Err`, so only fully-symbolic or fully-octal strings reach it.
pub fn parse_mode_changes(changes: &str) -> Result<file_mode::Mode> {
    // TODO symbolic validation should soon be unnecessary.
    let symbolic = changes
        .chars()
        .all(|c| matches!(c, 'u' | 'g' | 'o' | 'a' | 'r' | 'w' | 'x' | 'X' | 's' | 't' | '+' | '-' | '=' | ','));
    let octal = !changes.is_empty() && changes.chars().all(|c| c.is_ascii_digit());
    if !symbolic && !octal {
        return Err(Error::Config(format!(
            "invalid --mode `{changes}`: unexpected character (expected a chmod \
             symbolic string like `u+rwx,go-rx` or a bare octal mode like `0644`)"
        )));
    }
    let mut mode = file_mode::Mode::empty();
    if octal {
        let value = u32::from_str_radix(changes, 8).map_err(|_| {
            Error::Config(format!(
                "invalid --mode `{changes}`: not a valid octal mode"
            ))
        })?;
        if value > 0o7777 {
            return Err(Error::Config(format!(
                "invalid --mode `{changes}`: mode out of range (maximum is `7777`)"
            )));
        }
        // GNU bare octal is an absolute set; file-mode wants the `=` operator,
        // which also round-trips setuid/setgid/sticky in the leading digit.
        mode.set_str(&format!("={changes}"))
            .map_err(|e| Error::Config(format!("invalid --mode `{changes}`: {e}")))?;
    } else {
        mode.set_str(changes)
            .map_err(|e| Error::Config(format!("invalid --mode `{changes}`: {e}")))?;
    }
    Ok(mode)
}

/// Parse owner/group CLI args into a policy. Syntax-only: no `--map-target` /
/// `--same-owner` gating happens here. Shared by archive and extract.
pub fn parse_owner_group_args(
    owner: Option<&str>,
    owner_file: Option<&Path>,
    group: Option<&str>,
    group_file: Option<&Path>,
) -> Result<Option<OwnerGroupPolicy>> {
    let owner_override = owner.map(IdentitySpec::parse).transpose()?;
    let group_override = group.map(IdentitySpec::parse).transpose()?;

    // Parse owner-map, group-map. An empty map file yields `None`.
    let owner_map = parse_map_file(owner_file)?;
    let group_map = parse_map_file(group_file)?;

    let policy = OwnerGroupPolicy { owner_map, group_map, owner_override, group_override };
    if !policy.at_least_one_present() {
        return Ok(None);
    }

    Ok(Some(policy))
}

/// Parse a file and build an IdentityMap
fn parse_map_file(map_file: Option<&Path>) -> Result<Option<IdentityMap>> {
    Ok(map_file
        .map(read_map_file_entries)
        .transpose()?
        .and_then(|entries| {
            if entries.is_empty() {
                None
            } else {
                Some(IdentityMap::from_entries(entries))
            }
        }))
}

fn read_map_file_entries(path: &Path) -> Result<Vec<(IdentitySpec, IdentitySpec)>> {
    let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    parse_map_lines(&text)
}

/// Split a map file into `from → to` pairs: symmetric `FROM <ws>+ TO`, blank lines and
/// `#`-to-EOL comments ignored.
pub fn parse_map_lines(text: &str) -> Result<Vec<(IdentitySpec, IdentitySpec)>> {
    let mut out: Vec<(IdentitySpec, IdentitySpec)> = Vec::new();
    for line in text.lines() {
        // Remove comment
        let line = strip_comment(line).trim();
        // Remove empty lines
        if line.is_empty() {
            continue;
        }
        // Actual parsing of from, to
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 {
            return Err(Error::Config(format!(
                "owner/group map line must be `FROM <ws> TO`, got: `{line}`"
            )));
        }
        let from = IdentitySpec::parse(&fields[0])?;
        let to = IdentitySpec::parse(&fields[1])?;
        out.push((from, to));
    }
    Ok(out)
}

/// Drop `#`-to-end-of-line comments (`#` introduced anywhere in the line).
fn strip_comment(line: &str) -> &str {
    match line.split_once("#") {
        Some((pre, _)) => pre,
        None => line,
    }
}

/// True when `--same-owner` should apply by default (the extracting process is root).
/// Windows has no root concept; always `false`.
#[cfg(unix)]
pub fn infer_same_owner() -> bool {
    use nix::unistd::Uid;
    Uid::effective().is_root()
}

#[cfg(not(unix))]
pub fn infer_same_owner() -> bool {
    false
}

/// Apply `--validate-maps`: hard-error on destinations that the target mode cannot emit.
/// Without validation this is the per-load warning path.
pub fn validate_for_mode(
    policy: &OwnerGroupPolicy,
    target: MapResolutionTarget,
) -> Result<()> {
    let empty_map = IdentityMap::default();
    let targets = [
        ("owner", policy.owner_map.as_ref().unwrap_or(&empty_map), &policy.owner_override),
        ("group", policy.group_map.as_ref().unwrap_or(&empty_map), &policy.group_override),
    ];
    for (label, map, ovr) in targets {
        match target {
            MapResolutionTarget::Ids => {
                let ovr_id_not_present = ovr
                    .as_ref()
                    .map(|s| s.id.is_none())
                    .unwrap_or(false);
                // Raise error, if the id is missing.
                if ovr_id_not_present {
                    return Err(Error::Config(format!(
                        "--map-target=ids requires the {label} override to carry a numeric id"
                    )))
                }
            }
            MapResolutionTarget::Names if ovr
                .as_ref()
                .map(|s| s.name.is_none())
                .unwrap_or(false) => {

                return Err(Error::Config(format!(
                    "--map-target=names requires the {label} override to carry a name"
                )))
            }
            _ => {}
        }
        // The override, if present, must also resolve on this host.
        if let Some(n) = ovr.as_ref().and_then(|s| s.name.as_ref()) {
            check_name_exists(label, n)?;
        }

        let map_label = format!("{label}-map");
        // Check by name section of OwnerGroupPolicy
        for (_src, map_dst) in &map.by_name {
            check_dst(&map_label, map_dst, target)?;
        }
        // Check by id section of the OwnerGroupPolicy
        for (_src, map_dst) in &map.by_id {
            check_dst(&map_label, map_dst, target)?;
        }
    }
    Ok(())
}

/// Check that a given destination identity spec exists for a given --map-target.
/// check id is present for Ids, and check that name is present for Names
fn check_dst(label: &str, dst: &IdentitySpec, target: MapResolutionTarget) -> Result<()> {
    match target {
        MapResolutionTarget::Ids if dst.id.is_none() => Err(Error::Config(format!(
            "--map-target=ids requires numeric destinations; {label} entry has only a name"
        ))),
        MapResolutionTarget::Names if dst.name.is_none() => Err(Error::Config(format!(
            "--map-target=names requires named destinations; {label} entry has only an id"
        ))),
        _ => Ok(()),
    }?;
    if let Some(n) = &dst.name {
        check_name_exists(label, n)?;
    }
    Ok(())
}

/// `--validate-maps`: verify a target name resolves on the extraction host.
fn check_name_exists(label: &str, name: &String) -> Result<()> {
    // The map-dst path passes `owner-map` / `group-map`; strip the suffix so the
    // owner/group dispatch below matches.
    let kind = label.strip_suffix("-map").unwrap_or(label);
    let exists = match kind {
        "owner" => lookup_uid(name.as_ref()).is_some(),
        "group" => lookup_gid(name.as_ref()).is_some(),
        _ => false, // defensive: only "owner"/"group" (and "-map" forms) callers exist
    };
    if exists {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "--validate-maps: {label} name `{name}` does not exist on the extraction host"
        )))
    }
}

/// Resolve the extraction `(uid, gid)` for one entry.
///
/// Returns `(None, None)` when nothing should be applied (leave the file owned by the
/// extracting process). See the module docs for the per-target chains.
pub fn resolve_owner_group(
    src_uid: Option<u32>,
    src_gid: Option<u32>,
    src_username: Option<&str>,
    src_groupname: Option<&str>,
    policy: &OwnerGroupPolicy,
    target: MapResolutionTarget,
    same_owner: bool,
) -> Result<(Option<u32>, Option<u32>)> {
    let empty_map = IdentityMap::default();
    let owner = resolve_identity(
        src_username, src_uid,
        policy.owner_map.as_ref().unwrap_or(&empty_map), policy.owner_override.as_ref(),
        target,
        same_owner,
        lookup_uid,
    )?;
    let group = resolve_identity(
        src_groupname, src_gid,
        policy.group_map.as_ref().unwrap_or(&empty_map), policy.group_override.as_ref(),
        target,
        same_owner,
        lookup_gid,
    )?;
    Ok((owner, group))
}

fn resolve_identity(
    src_name: Option<&str>,
    src_id: Option<u32>,
    map: &IdentityMap,
    ovr: Option<&IdentitySpec>,
    target: MapResolutionTarget,
    same_owner: bool,
    lookup: fn(&str) -> Option<u32>,
) -> Result<Option<u32>> {
    // Step 1: pick the identity spec to emit. Matches map by name then by uid.
    let resolved_src_id = src_id.and_then(|i| map.by_id.get(&i));
    let resolved_target: Option<&IdentitySpec> = src_name
        .as_ref()
        .and_then(|n| map.by_name.get(*n))
        .or_else(|| resolved_src_id);

    // Step 2: resolve per target mode. Each chain applies the map override
    // first, then the `--owner`/`--group` override, then — only under
    // `--same-owner` — the archived (db) identity. A `None` from a chain means
    // "leave ownership unset" and must not be resurrected here.
    Ok(match target {
        MapResolutionTarget::Ids => {
            resolve_ids(resolved_target, src_id, ovr, same_owner)
        },
        MapResolutionTarget::Names => {
            resolve_names(resolved_target, src_name, ovr, same_owner, lookup)
        }
        MapResolutionTarget::NameId => {
            resolve_name_id(resolved_target, src_name, src_id, ovr, same_owner, lookup)
        },
        MapResolutionTarget::IdName => {
            resolve_id_name(resolved_target, src_name, src_id, ovr, same_owner, lookup)
        },
    })
}

/// `ids`: map.id → override.id → (same_owner ? db_id : None).
fn resolve_ids(
    chosen: Option<&IdentitySpec>,
    db_id: Option<u32>,
    ovr: Option<&IdentitySpec>,
    same_owner: bool,
) -> Option<u32> {
    let ovr_id = ovr.as_ref().and_then(|s| s.id);
    let db_res_id = if same_owner {
        db_id
    } else {
        None
    };
    chosen
        .as_ref()
        .and_then(|s| s.id) // Id from Map
        .or_else(|| ovr_id) // Id from override
        .or_else(|| db_res_id) // Id from db (same-owner only)
}

/// `names`: map.name → override.name → (same_owner ? db_name : None).
/// Missing/unresolvable db name ends in `None` (no numeric upgrade).
fn resolve_names(
    chosen: Option<&IdentitySpec>,
    db_name: Option<&str>,
    ovr: Option<&IdentitySpec>,
    same_owner: bool,
    lookup: fn(&str) -> Option<u32>,
) -> Option<u32> {
    let ovr_name = ovr.as_ref().and_then(|s| s.name.clone());
    let db_map_name = if same_owner {
        db_name.map(|s| s.to_string())
    } else {
        None
    };
    let eff_name: Option<String> = chosen
        .as_ref()
        .and_then(|s| s.name.clone()) // Name from map
        .or_else(|| ovr_name) // Name from override
        .or_else(|| db_map_name); // Name from db (same-owner only)
    eff_name.and_then(|n| lookup(n.as_ref()))
}

/// `name-id`:
///   --no-same-owner: map.name → override.name → map.id → override.id → (nothing)
///   --same-owner:    map.name → override.name → db.name → map.id → override.id → db.uid → (apply)
fn resolve_name_id(
    chosen: Option<&IdentitySpec>,
    db_name: Option<&str>,
    db_id: Option<u32>,
    ovr: Option<&IdentitySpec>,
    same_owner: bool,
    lookup: fn(&str) -> Option<u32>,
) -> Option<u32> {
    resolve_names(chosen, db_name, ovr, same_owner, lookup)
        .or_else(|| resolve_ids(chosen, db_id, ovr, same_owner))
}

/// `id-name`: the mirror of `name-id`.
/// Id first: map.id → override.id → (same_owner ? db.id). Fall back to the name chain.
fn resolve_id_name(
    chosen: Option<&IdentitySpec>,
    db_name: Option<&str>,
    db_id: Option<u32>,
    ovr: Option<&IdentitySpec>,
    same_owner: bool,
    lookup: fn(&str) -> Option<u32>,
) -> Option<u32> {
    resolve_ids(chosen, db_id, ovr, same_owner)
        .or_else(|| resolve_names(chosen, db_name, ovr, same_owner, lookup))
}

/// Name → numeric id, if the username exists on this system.
#[cfg(unix)]
fn lookup_uid(name: &str) -> Option<u32> {
    use nix::unistd::User;
    User::from_name(name).ok().flatten().map(|u| u.uid.as_raw())
}

#[cfg(not(unix))]
fn lookup_uid(_name: &str) -> Option<u32> {
    None
}

/// Name → numeric id, if the groupname exists on this system.
#[cfg(unix)]
fn lookup_gid(name: &str) -> Option<u32> {
    use nix::unistd::Group;
    Group::from_name(name).ok().flatten().map(|g| g.gid.as_raw())
}

#[cfg(not(unix))]
fn lookup_gid(_name: &str) -> Option<u32> {
    None
}

// -------------------------------------------------------------------------------------------------
// Testing
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // -- platform-independent fixtures ------------------------------------------------

    /// Write `contents` to a fresh temp file, returning (dir, path).
    fn temp_map_file(contents: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("map.txt");
        std::fs::write(&path, contents).expect("write map file");
        (dir, path)
    }

    fn spec(name: Option<&str>, id: Option<u32>) -> IdentitySpec {
        IdentitySpec {
            name: name.map(|s| s.to_string()),
            id,
        }
    }

    fn empty_policy() -> OwnerGroupPolicy {
        OwnerGroupPolicy::default()
    }

    fn policy_with_map(map: IdentityMap, label: &str) -> OwnerGroupPolicy {
        let mut p = empty_policy();
        if label == "owner" {
            p.owner_map = Some(map);
        } else {
            p.group_map = Some(map);
        }
        p
    }

    // -- unix-only fixtures (needed for host lookups) --------------------------------

    #[cfg(unix)]
    fn current_user() -> (u32, String) {
        use nix::unistd::{Uid, User};
        let uid = Uid::current().as_raw();
        let name = User::from_uid(Uid::from_raw(uid))
            .ok()
            .flatten()
            .expect("current user must resolve in passwd")
            .name;
        (uid, name)
    }

    #[cfg(unix)]
    fn current_group() -> (u32, String) {
        use nix::unistd::{Gid, Group};
        let gid = Gid::current().as_raw();
        let name = Group::from_gid(Gid::from_raw(gid))
            .ok()
            .flatten()
            .expect("current group must resolve in group db")
            .name;
        (gid, name)
    }

    /// A name that is guaranteed not to resolve in the passwd/group db.
    #[cfg(unix)]
    fn nonexistent_user_name() -> String {
        use nix::unistd::{User, Uid};
        let mut i = 0u32;
        loop {
            let candidate = format!("nouser-{}-{}", std::process::id(), i);
            if User::from_name(candidate.as_ref()).ok().flatten().is_none() {
                return candidate;
            }
            i += 1;
        }
    }

    #[cfg(unix)]
    fn nonexistent_group_name() -> String {
        use nix::unistd::{Group, Gid};
        let mut i = 0u32;
        loop {
            let candidate = format!("nogroup-{}-{}", std::process::id(), i);
            if Group::from_name(candidate.as_ref()).ok().flatten().is_none() {
                return candidate;
            }
            i += 1;
        }
    }

    // ================================================================================
    // Suite 1 — IdentitySpec::parse
    // ================================================================================

    #[test]
    fn identity_spec_parse_uid() {
        let s = IdentitySpec::parse("+42").expect("+uid");
        assert_eq!(s, IdentitySpec { name: None, id: Some(42) });
        let s = IdentitySpec::parse("+0").expect("+0");
        assert_eq!(s, IdentitySpec { name: None, id: Some(0) });
    }

    #[test]
    fn identity_spec_parse_name() {
        let s = IdentitySpec::parse("alice").expect("name");
        assert_eq!(s, IdentitySpec { name: Some("alice".to_string()), id: None });
    }

    #[test]
    fn identity_spec_parse_both() {
        let s = IdentitySpec::parse("alice:42").expect("name:uid");
        assert_eq!(s, IdentitySpec { name: Some("alice".to_string()), id: Some(42) });
        let s = IdentitySpec::parse(":42").expect(":uid");
        assert_eq!(s, IdentitySpec { name: None, id: Some(42) });
        let s = IdentitySpec::parse("alice:").expect("name:");
        assert_eq!(s, IdentitySpec { name: Some("alice".to_string()), id: None });
    }

    #[test]
    fn identity_spec_parse_rejects_none_none() {
        assert!(IdentitySpec::parse(":").is_err());
        assert!(IdentitySpec::parse("").is_err());
        assert!(IdentitySpec::parse("   ").is_err());
    }

    #[test]
    fn identity_spec_parse_rejects_bad_numeric() {
        assert!(IdentitySpec::parse("+abc").is_err());
        assert!(IdentitySpec::parse("+").is_err());
        assert!(IdentitySpec::parse("alice:abc").is_err());
    }

    #[test]
    fn identity_spec_parse_numeric_name_warns_but_parses() {
        // A bare all-digit string is a name (footgun guard emits a warning).
        let s = IdentitySpec::parse("007").expect("name");
        assert_eq!(s, IdentitySpec { name: Some("007".to_string()), id: None });
    }

    // ================================================================================
    // Suite 2 — MapResolutionTarget
    // ================================================================================

    #[test]
    fn map_target_from_str_valid() {
        assert_eq!(
            MapResolutionTarget::from_str("ids").unwrap(),
            MapResolutionTarget::Ids);
        assert_eq!(
            MapResolutionTarget::from_str("names").unwrap(),
            MapResolutionTarget::Names
        );
        assert_eq!(
            MapResolutionTarget::from_str("name-id").unwrap(),
            MapResolutionTarget::NameId
        );
        assert_eq!(
            MapResolutionTarget::from_str("name_id").unwrap(),
            MapResolutionTarget::NameId
        );
        assert_eq!(
            MapResolutionTarget::from_str("id-name").unwrap(),
            MapResolutionTarget::IdName
        );
        assert_eq!(
            MapResolutionTarget::from_str("id_name").unwrap(),
            MapResolutionTarget::IdName
        );
    }

    #[test]
    fn map_target_from_str_invalid() {
        assert!(MapResolutionTarget::from_str("").is_err());
        assert!(MapResolutionTarget::from_str("aösdkjfösldkfj").is_err());
        assert!(MapResolutionTarget::from_str("ids ").is_err());
    }

    #[test]
    fn map_target_as_str_roundtrip() {
        let targets = [
            MapResolutionTarget::Ids,
            MapResolutionTarget::Names,
            MapResolutionTarget::NameId,
            MapResolutionTarget::IdName
        ];
        for t in targets {
            assert_eq!(
                MapResolutionTarget::from_str(t.as_str()).unwrap(),
                t
            );
        }
    }

    #[test]
    fn map_target_serde_roundtrip() {
        let target = [
            MapResolutionTarget::Ids,
            MapResolutionTarget::Names,
            MapResolutionTarget::NameId,
            MapResolutionTarget::IdName
        ];
        for t in target {
            let json = serde_json::to_string(&t)
                .expect("serialize");
            let back = serde_json::from_str::<MapResolutionTarget>(&json)
                .expect("deserialize");
            assert_eq!(back, t);
        }
    }

    // ================================================================================
    // Suite 3 — OwnerGroupPolicy serde + at_least_one_present
    // ================================================================================

    #[test]
    fn policy_at_least_one_present() {
        assert!(!empty_policy().at_least_one_present());

        let p = policy_with_map(IdentityMap::default(), "owner");
        assert!(p.at_least_one_present());

        let mut p = empty_policy();
        p.owner_override = Some(spec(Some("alice"), None));
        assert!(p.at_least_one_present());

        let mut p = empty_policy();
        p.group_override = Some(spec(None, Some(42)));
        assert!(p.at_least_one_present());
    }

    #[test]
    fn policy_serde_roundtrip_full() {
        let mut p = empty_policy();
        p.owner_map = Some(IdentityMap::from_entries([
            (spec(Some("alice"), None), spec(Some("bob"), Some(1000))),
        ]));
        p.group_map = Some(IdentityMap::from_entries([
            (spec(None, Some(10)), spec(None, Some(2000))),
        ]));
        p.owner_override = Some(spec(Some("root"), None));
        p.group_override = Some(spec(None, Some(0)));

        let json = serde_json::to_string(&p)
            .expect("serialize");
        let back = serde_json::from_str::<OwnerGroupPolicy>(&json)
            .expect("deserialize");
        assert_eq!(back, p);
        assert_eq!(back.owner_map.as_ref().unwrap().by_name.len(), 1);
        assert_eq!(back.group_map.as_ref().unwrap().by_id.len(), 1);
    }

    #[test]
    fn policy_serde_roundtrip_empty() {
        let p = empty_policy();
        let json = serde_json::to_string(&p)
            .expect("serialize");
        let back = serde_json::from_str::<OwnerGroupPolicy>(&json)
            .expect("deserialize");
        assert_eq!(back, p);
        assert!(!back.at_least_one_present());
    }

    #[test]
    fn policy_serde_roundtrip_partial() {
        let mut p = empty_policy();
        p.owner_map = Some(IdentityMap::from_entries([
            (spec(Some("u1"), Some(1)), spec(Some("u2"), None)),
        ]));
        let json = serde_json::to_string(&p)
            .expect("serialize");
        let back = serde_json::from_str::<OwnerGroupPolicy>(&json)
            .expect("deserialize");
        assert_eq!(back, p);
        assert!(back.group_map.is_none());
        assert!(back.owner_override.is_none());
        assert!(back.group_override.is_none());
    }

    #[test]
    fn policy_all_none_from_parse_is_none() {
        // An all-None policy can not be produced by parse_owner_group_args.
        assert_eq!(
            parse_owner_group_args(None, None, None, None).expect("parse"),
            None
        );
    }

    // ================================================================================
    // Suite 4 — parse_owner_group_args
    // ================================================================================

    #[test]
    fn parse_args_owner_only() {
        let p = parse_owner_group_args(Some("alice"), None, None, None)
            .expect("parse")
            .expect("Some");
        assert_eq!(p.owner_override, Some(spec(Some("alice"), None)));
        assert!(p.group_override.is_none());
        assert!(p.owner_map.is_none());
        assert!(p.group_map.is_none());
    }

    #[test]
    fn parse_args_group_only() {
        let p = parse_owner_group_args(None, None, Some("staff"), None)
            .expect("parse")
            .expect("Some");
        assert_eq!(p.group_override, Some(spec(Some("staff"), None)));
        assert!(p.owner_override.is_none());
        assert!(p.owner_map.is_none());
        assert!(p.group_map.is_none());
    }

    #[test]
    fn parse_args_owner_map_only() {
        let (_dir, path) = temp_map_file("alice bob\n+42 +1000\n");
        let p = parse_owner_group_args(None, Some(&path), None, None)
            .expect("parse")
            .expect("Some");
        let om = p.owner_map.expect("owner_map Some");
        assert_eq!(om.by_name.len(), 1);
        assert_eq!(om.by_id.len(), 1);
        assert!(p.group_map.is_none());
        assert!(p.owner_override.is_none());
    }

    #[test]
    fn parse_args_group_map_only() {
        let (_dir, path) = temp_map_file("staff +2000\n");
        let p = parse_owner_group_args(None, None, None, Some(&path))
            .expect("parse")
            .expect("Some");
        let gm = p.group_map.expect("group_map Some");
        assert_eq!(gm.by_name.len(), 1);
        assert_eq!(gm.by_name[&String::from("staff")], spec(None, Some(2000)));
        assert!(p.owner_map.is_none());
        assert!(p.group_override.is_none());
    }

    #[test]
    fn parse_args_all_combined() {
        let (_d1, owner_file) = temp_map_file("alice bob\n");
        let (_d2, group_file) = temp_map_file("staff +2000\n");
        let p = parse_owner_group_args(
            Some("ceo"),
            Some(&owner_file),
            Some("ops"),
            Some(&group_file),
        )
        .expect("parse")
        .expect("Some");
        assert!(p.owner_map.is_some());
        assert!(p.group_map.is_some());
        assert_eq!(p.owner_override, Some(spec(Some("ceo"), None)));
        assert_eq!(p.group_override, Some(spec(Some("ops"), None)));
    }

    #[test]
    fn parse_args_none_is_none() {
        assert_eq!(parse_owner_group_args(None, None, None, None).expect("parse"), None);
    }

    #[test]
    fn parse_args_empty_map_file_is_none() {
        let (_dir, path) = temp_map_file("   \n# comment\n\n");
        let p = parse_owner_group_args(None, Some(&path), None, None)
            .expect("parse");
        assert_eq!(p, None);
    }

    #[test]
    fn parse_args_comments_and_blank_lines() {
        let (_dir, path) = temp_map_file("\n# leading comment\n alice bob \n\n+42 +1000 # trailing\n");
        let p = parse_owner_group_args(None, Some(&path), None, None)
            .expect("parse")
            .expect("Some");
        let om = p.owner_map.expect("map");
        assert_eq!(om.by_name.len(), 1);
        assert_eq!(om.by_id.len(), 1);
        // `alice bob` → by_name keyed on the source name.
        assert_eq!(om.by_name[&String::from("alice")], spec(Some("bob"), None));
        // `+42 +1000` keys by_id on the *source* id (42); dst is the `+1000`.
        assert_eq!(om.by_id[&42], spec(None, Some(1000)));
    }

    #[test]
    fn parse_args_name_uid_both_sides() {
        // A `NAME:UID NAME:UID` line populates BOTH by_name and by_id on the
        // source, and the dst carries both name and id.
        let (_dir, path) = temp_map_file("alice:42 bob:7\n");
        let p = parse_owner_group_args(None, Some(&path), None, None)
            .expect("parse")
            .expect("Some");
        let om = p.owner_map.expect("map");
        assert_eq!(om.by_name.len(), 1);
        assert_eq!(om.by_id.len(), 1);
        let expected_dst = spec(Some("bob"), Some(7));
        assert_eq!(om.by_name[&String::from("alice")], expected_dst);
        assert_eq!(om.by_id[&42], expected_dst);
    }

    #[test]
    fn parse_args_errors() {
        // `--owner=:` → (None, None) is rejected.
        let err = parse_owner_group_args(Some(":"), None, None, None)
            .expect_err("owner :");
        assert!(matches!(err, Error::Config(_)));
        // `--owner=+abc`.
        let err = parse_owner_group_args(Some("+abc"), None, None, None)
            .expect_err("+abc");
        assert!(matches!(err, Error::Config(_)));
        // map file with one field only.
        let (_d1, one_field) = temp_map_file("just-one\n");
        let err = parse_owner_group_args(None, Some(&one_field), None, None)
            .expect_err("one field");
        assert!(matches!(err, Error::Config(_)));
        // map line where `to` is `:` (both empty).
        let (_d2, bad_to) = temp_map_file("alice :\n");
        let err = parse_owner_group_args(None, Some(&bad_to), None, None)
            .expect_err("bad to");
        assert!(matches!(err, Error::Config(_)));
        // map line with invalid numeric in to.
        let (_d3, bad_num) = temp_map_file("alice +xyz\n");
        let err = parse_owner_group_args(None, Some(&bad_num), None, None)
            .expect_err("bad numeric");
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn parse_args_map_plus_override_is_some() {
        // A partial (map only, override none) still yields Some.
        let (_dir, path) = temp_map_file("alice bob\n");
        let p = parse_owner_group_args(None, Some(&path), None, None)
            .expect("parse")
            .expect("Some");
        assert!(p.at_least_one_present());
    }

    // ================================================================================
    // Suite 5 — validate_for_mode  (unix-gated: needs host lookup)
    // ================================================================================

    #[cfg(unix)]
    #[test]
    fn validate_ids_requires_id_override() {
        let mut p = empty_policy();
        p.owner_override = Some(spec(Some("alice"), None)); // name-only override
        let err = validate_for_mode(&p, MapResolutionTarget::Ids)
            .expect_err("ids name ovr");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_ids_accepts_id_override() {
        let (uid, _) = current_user();
        let mut p = empty_policy();
        p.owner_override = Some(spec(None, Some(uid))); // id-only, exists
        validate_for_mode(&p, MapResolutionTarget::Ids)
            .expect("ids + id override ok");
    }

    #[cfg(unix)]
    #[test]
    fn validate_ids_requires_id_dst() {
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(Some("alice"), None)), // dst name-only
        ]);
        let p = policy_with_map(map, "owner");
        let err = validate_for_mode(&p, MapResolutionTarget::Ids)
            .expect_err("ids name dst");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_ids_accepts_id_dst() {
        let (uid, _) = current_user();
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(None, Some(uid))),
        ]);
        let p = policy_with_map(map, "owner");
        validate_for_mode(&p, MapResolutionTarget::Ids).expect("ids + id dst ok");
    }

    #[cfg(unix)]
    #[test]
    fn validate_names_requires_name_override() {
        let mut p = empty_policy();
        p.owner_override = Some(spec(None, Some(42))); // id-only override
        let err = validate_for_mode(&p, MapResolutionTarget::Names)
            .expect_err("names id ovr");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_names_accepts_name_override() {
        let (_, name) = current_user();
        let mut p = empty_policy();
        p.owner_override = Some(spec(Some(&name), None));
        validate_for_mode(&p, MapResolutionTarget::Names)
            .expect("names + name ovr ok");
    }

    #[cfg(unix)]
    #[test]
    fn validate_names_requires_name_dst() {
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(None, Some(42))), // dst id-only
        ]);
        let p = policy_with_map(map, "owner");
        let err = validate_for_mode(&p, MapResolutionTarget::Names)
            .expect_err("names id dst");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_names_accepts_name_dst() {
        let (_, name) = current_user();
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(Some(&name), None)),
        ]);
        let p = policy_with_map(map, "owner");
        validate_for_mode(&p, MapResolutionTarget::Names)
            .expect("names + name dst ok");
    }

    #[cfg(unix)]
    #[test]
    fn validate_host_name_exists() {
        let (_, name) = current_user();
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(Some(&name), None)),
        ]);
        let p = policy_with_map(map, "owner");
        validate_for_mode(&p, MapResolutionTarget::NameId)
            .expect("existing name ok");
    }

    #[cfg(unix)]
    #[test]
    fn validate_host_name_missing_errors() {
        let missing = nonexistent_user_name();
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(Some(&missing), None)),
        ]);
        let p = policy_with_map(map, "owner");
        let err = validate_for_mode(&p, MapResolutionTarget::NameId)
            .expect_err("missing name");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_host_missing_override_errors() {
        let missing = nonexistent_user_name();
        let mut p = empty_policy();
        p.owner_override = Some(spec(Some(&missing), None));
        let err = validate_for_mode(&p, MapResolutionTarget::Names)
            .expect_err("missing ovr name");
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn validate_group_host_name() {
        let (_, name) = current_group();
        let map = IdentityMap::from_entries([
            (spec(Some("src"), None), spec(Some(&name), None)),
        ]);
        let p = policy_with_map(map, "group");
        validate_for_mode(&p, MapResolutionTarget::Names)
            .expect("existing group name ok");
    }

    // ================================================================================
    // Suite 6 — resolve_owner_group full matrix  (unix-gated)
    // ================================================================================

    #[cfg(unix)]
    #[test]
    fn resolve_ids_no_map_no_override() {
        let (uid, name) = current_user();

        // no-same-owner: leave unset
        let r = resolve_case(Some(uid), Some(&name), IdentityMap::default(), None,
                             MapResolutionTarget::Ids, false).expect("ok");
        assert_eq!(r, None);

        // same-owner: db uid applies
        let r = resolve_case(Some(uid), Some(&name), IdentityMap::default(), None,
                             MapResolutionTarget::Ids, true).expect("ok");
        assert_eq!(r, Some(uid));
    }

    /// One row of the resolution matrix. `same_owner && !db` uses absent db identity.
    fn resolve_case(
        src_uid: Option<u32>,
        src_name: Option<&str>,
        map: IdentityMap,
        ovr_arg: Option<&IdentitySpec>,
        target: MapResolutionTarget,
        same_owner: bool,
    ) -> Result<Option<u32>> {
        let mut p = empty_policy();
        p.owner_map = if map.by_id.is_empty() && map.by_name.is_empty() { None }
                      else { Some(map) };
        p.owner_override = ovr_arg.map(|s| s.clone());
        resolve_identity(
            src_name, src_uid,
            p.owner_map.as_ref().unwrap_or(&IdentityMap::default()),
            p.owner_override.as_ref(),
            target,
            same_owner,
            lookup_uid,
        )
    }

    #[cfg(unix)]
    #[test]
    fn resolve_ids_same_owner_no_db() {
        // src has no uid in the row; same_owner + no db → None.
        let empty = IdentityMap::default();
        let r = resolve_case(None, None, empty, None,
                             MapResolutionTarget::Ids, true).expect("ok");
        assert_eq!(r, None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_names_no_map_no_override() {
        let (uid, name) = current_user();

        // !same_owner → None
        let r = resolve_case(Some(uid), Some(&name), IdentityMap::default(), None,
                             MapResolutionTarget::Names, false).expect("ok");
        assert_eq!(r, None);

        // same_owner + db name resolves → db uid
        let r = resolve_case(Some(uid), Some(&name), IdentityMap::default(), None,
                             MapResolutionTarget::Names, true).expect("ok");
        assert_eq!(r, Some(uid));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_names_same_owner_unresolvable_db_name() {
        let missing = nonexistent_user_name();
        let empty = IdentityMap::default();
        let r = resolve_case(None, Some(&missing), empty, None,
                             MapResolutionTarget::Names, true).expect("ok");
        assert_eq!(r, None);
    }

    // TODO: Potentially add more tests with unittests.
    #[cfg(unix)]
    #[test]
    fn resolve_name_id_map_then_override_then_db() {
        let (uid, name) = current_user();

        // map has a name dst; resolver should pick it.
        let map = IdentityMap::from_entries([
            (spec(Some(&name), None), spec(Some(&name), None)),
        ]);
        let r = resolve_case(Some(uid), Some(&name), map, None,
                             MapResolutionTarget::NameId, true).expect("ok");
        assert_eq!(r, Some(uid));

        // map dst name unresolvable → override name (existing) → db.
        let missing = nonexistent_user_name();
        let map = IdentityMap::from_entries([
            (spec(Some(&name), None), spec(Some(&missing), None)),
        ]);
        let ovr = spec(Some(&name), None);
        let r = resolve_case(Some(uid), Some(&name), map, Some(&ovr),
                             MapResolutionTarget::NameId, true).expect("ok");
        assert_eq!(r, Some(uid));

        // override id used when name fails entirely under same_owner.
        let map2 = IdentityMap::from_entries([
            (spec(Some(&name), None), spec(Some(&missing), None)),
        ]);
        let ovr2 = spec(None, Some(uid));
        let r = resolve_case(Some(uid), Some(&name), map2, Some(&ovr2),
                             MapResolutionTarget::NameId, true).expect("ok");
        assert_eq!(r, Some(uid));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_id_name_map_then_override_then_db() {
        let (uid, name) = current_user();
        // map dst id-only; IdName should pick the id first.
        let map = IdentityMap::from_entries([
            (spec(Some(&name), None), spec(None, Some(uid))),
        ]);
        let r = resolve_case(Some(uid), Some(&name), map, None,
                             MapResolutionTarget::IdName, true).expect("ok");
        assert_eq!(r, Some(uid));

        // map dst name-only, override id; IdName picks override id before name.
        let map2 = IdentityMap::from_entries([
            (spec(Some(&name), None), spec(Some(&name), None)),
        ]);
        let ovr2 = spec(None, Some(uid));
        let r = resolve_case(Some(uid), Some(&name), map2, Some(&ovr2),
                             MapResolutionTarget::IdName, true).expect("ok");
        assert_eq!(r, Some(uid));
    }

    // ================================================================================
    // Suite 7 — symbolic mode changes (`--mode`)
    // ================================================================================

    fn apply(changes: &str, mode: u32) -> u32 {
        parse_mode_changes(changes).expect("parse").apply_to(mode)
    }

    #[test]
    fn mode_parse_errors_map_to_config() {
        assert!(matches!(parse_mode_changes(""), Err(Error::Config(_))));
        assert!(matches!(
            parse_mode_changes("u+zz"),
            Err(Error::Config(_))
        ));
        assert!(matches!(
            parse_mode_changes("not-a-mode"),
            Err(Error::Config(_))
        ));
        assert!(parse_mode_changes("u+rwx,go-rx").is_ok());
    }

    #[test]
    fn mode_arithmetic_preserves_type_bits() {
        // Regular file (S_IFREG 0o100000) keeps type bits through the transform.
        assert_eq!(apply("u+x", 0o100644), 0o100744);
        assert_eq!(apply("o=rw", 0o100755), 0o100756);
        assert_eq!(apply("a-rwx", 0o100755), 0o100000);
        // Directory (S_IFDIR 0o040000); `u+w` only touches the user class.
        assert_eq!(apply("u+w", 0o040555), 0o040755);
    }

#[test]
    fn mode_x_follows_directory_search_semantics() {
        // Regular file already without execute: `a+X` adds nothing.
        assert_eq!(apply("a+X", 0o100644), 0o100644);
        // Regular file that already has some execute bit: file-mode treats X as
        // dir-only (`dir_mask`), so unlike GNU chmod it does NOT spread x to other
        // classes here. Documented as a known deviation from GNU tar semantics.
        assert_eq!(apply("a+X", 0o100700), 0o100700);
        // Directory: X sets search bits (execute) for all classes.
        assert_eq!(apply("a+X", 0o040644), 0o040755);
    }

    #[test]
    fn mode_mask_only_changes_bits() {
        // Unrelated bits untouched; only the named classes change.
        assert_eq!(apply("u+w", 0o100455), 0o100655);
        assert_eq!(apply("go-r", 0o100777), 0o100733);
    }

    #[test]
    fn mode_bare_octal_sets_absolute_mode() {
        // GNU tar accepts a bare octal mode as an absolute set; type bits survive.
        assert_eq!(apply("0644", 0o100755), 0o100644);
        assert_eq!(apply("755", 0o100600), 0o100755);
        // 4-digit octal includes special bits (setuid here).
        assert_eq!(apply("4755", 0o100644), 0o104755);
        assert_eq!(apply("0", 0o100777), 0o100000);
    }

    #[test]
    fn mode_bare_octal_parse_errors() {
        // 8/9 are not octal digits.
        assert!(matches!(parse_mode_changes("09"), Err(Error::Config(_))));
        // Beyond the 4-digit special+rwx range.
        assert!(matches!(parse_mode_changes("77777"), Err(Error::Config(_))));
        assert!(parse_mode_changes("7777").is_ok());
    }
}
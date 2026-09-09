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
    let exists = match label {
        "owner" => lookup_uid(name.as_ref()).is_some(),
        "group" => lookup_gid(name.as_ref()).is_some(),
        _ => false, // defensive: only "owner"/"group" callers exist
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
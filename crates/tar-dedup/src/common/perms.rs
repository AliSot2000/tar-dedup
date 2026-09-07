//! Owner / group identity maps: parsing (CLI + map files) and extraction-time resolution.
//!
//! Pure logic, no DB access. The maps are plain data ([`OwnerGroupPolicy`], [`IdentityMap`],
//! [`PairSpec`]) that can be serialized to the `meta` table; the persistence lives in
//! `db/meta.rs`. Reused by both the archive (persist) and extract (resolve) commands.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One identity whose name/id maps to a target (`from:to` pair, GNU tar `--owner-map` style).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMap {
    /// `source name → target string` (target may be a name or a numeric id).
    pub by_name: HashMap<String, String>,
    /// `source id → target string` (target may be a name or a numeric id).
    pub by_id: HashMap<u32, String>,
}

impl IdentityMap {
    /// Fold `from:to` entries into the map. A numeric `from` goes into [`Self::by_id`], anything
    /// else into [`Self::by_name`]. Entries are validated to be non-empty.
    pub fn from_entries<I>(entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut map = Self::default();
        for (from, to) in entries {
            let from = from.trim();
            let to = to.trim();
            if from.is_empty() || to.is_empty() {
                return Err(Error::Config(format!(
                    "owner/group map entry must be `from:to`, got `{from}:{to}`"
                )));
            }
            if let Ok(id) = from.parse::<u32>() {
                map.by_id.insert(id, to.to_string());
            } else {
                map.by_name.insert(from.to_string(), to.to_string());
            }
        }
        Ok(map)
    }

    fn is_empty(&self) -> bool {
        self.by_name.is_empty() && self.by_id.is_empty()
    }
}

/// A `NAME`, `UID`, or `NAME:UID` override (GNU tar `--owner` / `--group`).
///
/// When either side is empty it is inferred from the other (`NAME` alone → numeric id resolved
/// at apply time, `UID` alone → name only for display).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairSpec {
    pub name: Option<String>,
    pub id: Option<u32>,
}

impl PairSpec {
    /// Parse `NAME`, `UID`, or `NAME:UID`.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::Config("empty `--owner`/`--group` value".into()));
        }
        match s.split_once(':') {
            None => {
                if let Ok(id) = s.parse::<u32>() {
                    Ok(Self { name: None, id: Some(id) })
                } else {
                    Ok(Self { name: Some(s.to_string()), id: None })
                }
            }
            Some((name, id)) => {
                let name = if name.is_empty() { None } else { Some(name.to_string()) };
                let id = if id.is_empty() {
                    None
                } else {
                    Some(id.parse::<u32>().map_err(|_| {
                        Error::Config(format!("invalid `--owner`/`--group` id: `{id}`"))
                    })?)
                };
                Ok(Self { name, id })
            }
        }
    }
}

/// The full owner/group policy: two tables plus optional force-all overrides.
///
/// Stored verbatim in the archive `meta` table and applied at extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGroupPolicy {
    pub owner: IdentityMap,
    pub group: IdentityMap,
    pub owner_override: Option<PairSpec>,
    pub group_override: Option<PairSpec>,
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

/// Parse owner/group CLI args into a policy (shared by archive and extract).
///
/// Reads the map files (I/O is done here), so nothing else needs to touch the filesystem.
pub fn build_owner_group_policy(
    owner: Option<&str>,
    owner_map: Option<&Path>,
    group: Option<&str>,
    group_map: Option<&Path>,
) -> Result<Option<OwnerGroupPolicy>> {
    let owner_override = owner.map(PairSpec::parse).transpose()?;
    let group_override = group.map(PairSpec::parse).transpose()?;

    let owner_entries = owner_map.map(read_map_file).transpose()?.unwrap_or_default();
    let group_entries = group_map.map(read_map_file).transpose()?.unwrap_or_default();

    let owner = IdentityMap::from_entries(owner_entries)?;
    let group = IdentityMap::from_entries(group_entries)?;

    if owner.is_empty() && group.is_empty() && owner_override.is_none() && group_override.is_none()
    {
        return Ok(None);
    }

    Ok(Some(OwnerGroupPolicy { owner, group, owner_override, group_override }))
}

fn read_map_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    Ok(parse_map_lines(&text))
}

/// Split a map file into non-empty `from`/`to` pairs (GNU tar `--owner-map` format,
/// `from:to` per line; blank lines and `#` comments skipped).
pub fn parse_map_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

/// Resolve the extraction `(uid, gid)` for one entry from the given policy.
///
/// Order per identity: force-all override → name table → id table → passthrough.
pub fn resolve_owner_group(
    uid: Option<u32>,
    gid: Option<u32>,
    username: Option<&str>,
    groupname: Option<&str>,
    policy: &OwnerGroupPolicy,
) -> (Option<u32>, Option<u32>) {
    let owner = resolve_identity(
        username,
        uid,
        &policy.owner,
        policy.owner_override.as_ref(),
        lookup_uid,
    );
    let group = resolve_identity(
        groupname,
        gid,
        &policy.group,
        policy.group_override.as_ref(),
        lookup_gid,
    );
    (owner, group)
}

fn resolve_identity(
    name: Option<&str>,
    id: Option<u32>,
    map: &IdentityMap,
    override_spec: Option<&PairSpec>,
    lookup: fn(&str) -> Option<u32>,
) -> Option<u32> {
    if let Some(spec) = override_spec {
        if let Some(v) = spec.id.or_else(|| spec.name.as_deref().and_then(lookup)) {
            return Some(v);
        }
    }
    if let Some(n) = name {
        if let Some(target) = map.by_name.get(n) {
            if let Some(v) = target.parse::<u32>().ok().or_else(|| lookup(target)) {
                return Some(v);
            }
        }
    }
    if let Some(i) = id {
        if let Some(target) = map.by_id.get(&i) {
            if let Some(v) = target.parse::<u32>().ok().or_else(|| lookup(target)) {
                return Some(v);
            }
        }
    }
    id
}

/// Name → numeric id, if the name exists on this system.
#[cfg(unix)]
fn lookup_uid(name: &str) -> Option<u32> {
    use nix::unistd::User;
    User::from_name(name).ok().flatten().map(|u| u.uid.as_raw())
}

#[cfg(not(unix))]
fn lookup_uid(_name: &str) -> Option<u32> {
    None
}

#[cfg(unix)]
fn lookup_gid(name: &str) -> Option<u32> {
    use nix::unistd::Group;
    Group::from_name(name).ok().flatten().map(|g| g.gid.as_raw())
}

#[cfg(not(unix))]
fn lookup_gid(_name: &str) -> Option<u32> {
    None
}
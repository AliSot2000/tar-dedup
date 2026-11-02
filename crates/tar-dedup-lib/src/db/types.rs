use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::db::flags::{FileFlags, OutTreeFlags, SourceFlags};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutTreeId(pub i64);

#[derive(Debug, Clone)]
pub struct OutTreeRecord {
    pub id: OutTreeId,
    pub abs_path: PathBuf,
    pub file_id: Option<FileId>,
    pub flags: OutTreeFlags,
    pub canonical_id: Option<OutTreeId>,
}

#[derive(Debug, Clone)]
pub struct NewOutTreeRow {
    pub abs_path: PathBuf,
    pub file_id: Option<FileId>,
    pub flags: OutTreeFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExclusionId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentId(pub String);

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: FileId,
    pub abs_path: PathBuf,
    pub ext: String,
    pub size: u64,
    pub sha1: Option<[u8; 20]>,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub ctime: Option<DateTime<Utc>>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub username: Option<String>,
    pub groupname: Option<String>,
    pub mode: Option<u32>,
    pub ftype: FileType,
    pub device_id: Option<u64>,
    pub inode_id: Option<u64>,
    pub major: Option<u64>,
    pub minor: Option<u64>,
    pub xattrs: Option<String>,
    pub posix_acl: Option<String>,
    pub selinux_ctx: Option<Vec<u8>>,
    pub win_perm: Option<String>,
    pub link_dst: Option<PathBuf>,
    pub include_reason_archive: Option<ExclusionId>,
    pub exclude_reason_archive: Option<ExclusionId>,
    pub include_reason_extract: Option<ExclusionId>,
    pub exclude_reason_extract: Option<ExclusionId>,
    pub canonical_id: Option<FileId>,
    pub flags: FileFlags,
    pub phase: FilePhase,
    pub new_name: Option<String>,
}

/// Narrow file row for pipeline stages that do not need ownership/xattr payloads.
#[derive(Debug, Clone)]
pub struct StrippedRecord {
    pub id: FileId,
    pub abs_path: PathBuf,
    /// Original on-disk bytes (None -> same as `abs_path`).
    pub abs_path_raw: Option<PathBuf>,
    pub ext: String,
    pub size: u64,
    pub sha1: Option<[u8; 20]>,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub ctime: Option<DateTime<Utc>>,
    pub ftype: FileType,
    pub device_id: Option<u64>,
    pub inode_id: Option<u64>,

    pub canonical_id: Option<FileId>,
    pub flags: FileFlags,
    pub phase: FilePhase,
    pub new_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewFileRecord {
    pub abs_path: PathBuf,
    /// Original on-disk bytes (None -> same as `abs_path`); stored as `abs_path_raw`.
    pub abs_path_raw: Option<PathBuf>,
    pub ext: String,
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub ctime: Option<DateTime<Utc>>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
    pub ftype: Option<FileType>,
    pub xattrs: Option<String>,
    pub posix_acl: Option<String>,
    pub selinux_ctx: Option<Vec<u8>>,
    pub win_perm: Option<String>,
    pub link_dst: Option<PathBuf>,
    /// Original symlink-target bytes (doc-only; None -> no raw differs).
    pub link_dst_raw: Option<PathBuf>,
    pub device_id: Option<u64>,
    pub inode_id: Option<u64>,
    pub major: Option<u64>,
    pub minor: Option<u64>,
}

/// Enum represents all possible targets a symlink can have. `Unknown` is for dangling links that
/// could not be resolved. Destination is to be understood as the transitive closure of any given
/// length of symlinks i.e. A -> B -> C -> D Will lead to A,B,C all having the same link type even
/// though only C points to LinkType and A,B point to symlinks themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    File,
    Directory,
    Socket,
    FIFO,
    CharacterDevice,
    BlockDevice,
    Dangling,
    Cycle,
    /// Emitted if an error is encountered while accessing the file type.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Socket,
    FIFO,
    CharacterDevice,
    BlockDevice,
    Symlink(LinkType),
    /// Emitted if an error is encountered while accessing the file type.
    Unknown,
}

impl LinkType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::FIFO => "fifo",
            Self::CharacterDevice => "char_dev",
            Self::BlockDevice => "block_dev",
            Self::Dangling => "dangling",
            Self::Cycle => "cycle",
            Self::Socket => "socket",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "file" => Ok(Self::File),
            "dir" => Ok(Self::Directory),
            "fifo" => Ok(Self::FIFO),
            "char_dev" => Ok(Self::CharacterDevice),
            "block_dev" => Ok(Self::BlockDevice),
            "dangling" => Ok(Self::Dangling),
            "cycle" => Ok(Self::Cycle),
            "socket" => Ok(Self::Socket),
            "unknown" => Ok(Self::Unknown),
            other => Err(crate::error::Error::Other(anyhow::anyhow!(
                "unknown LinkType: {other}"
            ))),
        }
    }
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::FIFO => "fifo",
            Self::CharacterDevice => "char_dev",
            Self::BlockDevice => "block_dev",
            Self::Socket => "socket",
            Self::Unknown => "unknown",
            Self::Symlink(LinkType::File) => "symlink_file",
            Self::Symlink(LinkType::Directory) => "symlink_dir",
            Self::Symlink(LinkType::Socket) => "symlink_socket",
            Self::Symlink(LinkType::FIFO) => "symlink_fifo",
            Self::Symlink(LinkType::CharacterDevice) => "symlink_char_dev",
            Self::Symlink(LinkType::BlockDevice) => "symlink_block_dev",
            Self::Symlink(LinkType::Dangling) => "symlink_dangling",
            Self::Symlink(LinkType::Cycle) => "symlink_cycle",
            Self::Symlink(LinkType::Unknown) => "symlink_unknown",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "file" => Ok(Self::File),
            "dir" => Ok(Self::Directory),
            "fifo" => Ok(Self::FIFO),
            "char_dev" => Ok(Self::CharacterDevice),
            "block_dev" => Ok(Self::BlockDevice),
            "socket" => Ok(Self::Socket),
            "unknown" => Ok(Self::Unknown),
            // All Symlink variants.
            "symlink_file" => Ok(Self::Symlink(LinkType::File)),
            "symlink_dir" => Ok(Self::Symlink(LinkType::Directory)),
            "symlink_socket" => Ok(Self::Symlink(LinkType::Socket)),
            "symlink_fifo" => Ok(Self::Symlink(LinkType::FIFO)),
            "symlink_char_dev" => Ok(Self::Symlink(LinkType::CharacterDevice)),
            "symlink_block_dev" => Ok(Self::Symlink(LinkType::BlockDevice)),
            "symlink_dangling" => Ok(Self::Symlink(LinkType::Dangling)),
            "symlink_cycle" => Ok(Self::Symlink(LinkType::Cycle)),
            "symlink_unknown" => Ok(Self::Symlink(LinkType::Unknown)),
            other => Err(crate::error::Error::Other(anyhow::anyhow!(
                "unknown FileType: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePhase {
    // Archive pipeline - archive everything in the given directory.
    /// File metadata has been captured (fsize, permissions, path)
    Inventoried,
    /// File Content was scanned for deduplication and sparsification
    Hashed,
    /// File inclusion/exclusion calculation is done
    Filtered,
    /// Based on the
    Deduped,
    /// Create sparse copies of files with holes.
    Sparsified,
    /// Symlink created in stage
    Staged,
    /// Files added to tar archived and compressed with given compression algorithm.
    Archived,

    // Extract pipeline — restore everything to rel_path inside the archive
    /// Ready to restore (payload may already be in extract cache).
    Unarchived,
    /// Filtered
    ExtractFiltered,
    /// An optional check to make sure there was no file corruption on the way.
    Rehashed,
    /// Any DirEntry successfully restored at destination.
    AtDestination,
    /// Permissions of the file were restored/attempted to restore.
    PermissionsRestored,
}

impl FilePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inventoried => "inventoried",
            Self::Hashed => "hashed",
            Self::Filtered => "filtered",
            Self::Deduped => "deduped",
            Self::Sparsified => "sparsified",
            Self::Staged => "staged",
            Self::Archived => "archived",

            Self::Unarchived => "unarchived",
            Self::ExtractFiltered => "extract_filtered",
            Self::Rehashed => "rehashed",
            Self::AtDestination => "at_destination",
            Self::PermissionsRestored => "permissions_restored",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "inventoried" => Ok(Self::Inventoried),
            "hashed" => Ok(Self::Hashed),
            "filtered" => Ok(Self::Filtered),
            "deduped" => Ok(Self::Deduped),
            "sparsified" => Ok(Self::Sparsified),
            "staged" => Ok(Self::Staged),
            "archived" => Ok(Self::Archived),

            "unarchived" => Ok(Self::Unarchived),
            "extract_filtered" => Ok(Self::ExtractFiltered),
            "rehashed" => Ok(Self::Rehashed),
            "at_destination" => Ok(Self::AtDestination),
            "permissions_restored" => Ok(Self::PermissionsRestored),
            other => Err(crate::error::Error::Config(format!(
                "unknown file phase: {other}"
            ))),
        }
    }

    pub fn is_archive_phase(self) -> bool {
        matches!(
            self,
            Self::Inventoried
                | Self::Hashed
                | Self::Filtered
                | Self::Deduped
                | Self::Sparsified
                | Self::Staged
                | Self::Archived
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupKey {
    pub sha1: [u8; 20],
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ArchiveSession {
    pub id: i64,
    pub archive_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FilterExpression {
    pub id: i64,
    pub from: String,
    pub line: Option<u64>,
    pub expression: String,
}

impl FilterExpression {
    pub fn is_internal(&self) -> bool {
        self.from == "internal"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceRecord {
    pub id: i64,
    pub source: String,
    pub abs_path: PathBuf,
    pub original_path: PathBuf,
    pub abs_path_raw: Option<PathBuf>,
    pub original_path_raw: Option<PathBuf>,
    pub line: i64,
    pub flags: SourceFlags,
}

/// `out_tree` embed in a sidecar manifest: the reverse-mapping used when a
/// target filename was re-randomized on sidecar collision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidecarOutTree {
    /// `out_tree.id` of the placed target.
    pub id: i64,
    /// Absolute path the entry was placed at (extraction target).
    pub target_path: PathBuf,
}

/// Metadata snapshot for a placed entry, written as a JSON sidecar next to the
/// target (`unarchive/permissions.rs`). Down-converted from a [`FileRecord`]
/// with no extra DB fetch. `sha1` is the lowercase-hex digest, `ftype` the
/// stable [`FileType::as_str`] string, `selinux` the stored context (lossy UTF-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidecarEntry {
    pub id: i64,
    pub abs_path: PathBuf,
    pub ext: String,
    pub size: u64,
    pub sha1: Option<String>,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub ctime: Option<DateTime<Utc>>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub username: Option<String>,
    pub groupname: Option<String>,
    pub mode: Option<u32>,
    pub ftype: String,
    pub device_id: Option<u64>,
    pub inode: Option<u64>,
    pub major: Option<u64>,
    pub minor: Option<u64>,
    pub xattrs: Option<String>,
    pub posix_acl: Option<String>,
    pub selinux: Option<String>,
    pub win_perm: Option<String>,
    pub link_dst: Option<PathBuf>,
    pub new_name: Option<String>,
    /// Out-tree reverse mapping (the placed target); filled by the writer.
    pub out_tree: SidecarOutTree,
}

impl From<&FileRecord> for SidecarEntry {
    fn from(r: &FileRecord) -> Self {
        Self {
            id: r.id.0,
            abs_path: r.abs_path.clone(),
            ext: r.ext.clone(),
            size: r.size,
            sha1: r.sha1.map(hex_sha1),
            mtime: r.mtime,
            atime: r.atime,
            ctime: r.ctime,
            uid: r.uid,
            gid: r.gid,
            username: r.username.clone(),
            groupname: r.groupname.clone(),
            mode: r.mode,
            ftype: r.ftype.as_str().to_string(),
            device_id: r.device_id,
            inode: r.inode_id,
            major: r.major,
            minor: r.minor,
            xattrs: r.xattrs.clone(),
            posix_acl: r.posix_acl.clone(),
            selinux: r
                .selinux_ctx
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            win_perm: r.win_perm.clone(),
            link_dst: r.link_dst.clone(),
            new_name: r.new_name.clone(),
            // Placeholder: the writer replaces this with the real out_tree.
            out_tree: SidecarOutTree { id: r.id.0, target_path: r.abs_path.clone() },
        }
    }
}

impl SidecarEntry {
    /// Consume and attach the real out-tree mapping (the placed target),
    /// returning the populated copy. `From<&FileRecord>` starts the embed at
    /// the file row's own id/path as a placeholder; consuming the value
    /// guarantees no stale embed can be aliased afterwards.
    pub fn with_out_tree(mut self, id: OutTreeId, target_path: &Path) -> Self {
        self.out_tree = SidecarOutTree { id: id.0, target_path: target_path.to_path_buf() };
        self
    }
}

fn hex_sha1(digest: [u8; 20]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

//==================================================================================================
// Testing
//==================================================================================================

#[test]
fn link_type_as_str_roundtrip() {
    let kinds = [
        LinkType::File,
        LinkType::Directory,
        LinkType::Socket,
        LinkType::FIFO,
        LinkType::CharacterDevice,
        LinkType::BlockDevice,
        LinkType::Dangling,
        LinkType::Cycle,
        LinkType::Unknown,
    ];

    for kind in kinds {
        assert_eq!(LinkType::parse(kind.as_str()).expect("parse"), kind);
    }
}

#[test]
fn sidecar_from_file_record_maps_all_fields() {
    use chrono::TimeZone;
    let t = Utc.timestamp_opt(1_600_000_000, 42).unwrap();
    let rec = FileRecord {
        id: FileId(7),
        abs_path: PathBuf::from("/home/alice/docs/report.pdf"),
        ext: "pdf".into(),
        size: 1234,
        sha1: Some([0xAB; 20]),
        mtime: Some(t),
        atime: Some(t),
        ctime: None,
        uid: Some(1000),
        gid: Some(100),
        username: Some("alice".into()),
        groupname: Some("users".into()),
        mode: Some(0o640),
        ftype: FileType::File,
        device_id: Some(1),
        inode_id: Some(99),
        major: None,
        minor: None,
        xattrs: Some("{}".into()),
        posix_acl: Some(r#"{"user-obj":7}"#.into()),
        selinux_ctx: Some(b"system_u:object_r:etc_t:s0".to_vec()),
        win_perm: None,
        link_dst: None,
        include_reason_archive: Some(ExclusionId(1)),
        exclude_reason_archive: Some(ExclusionId(-1)),
        include_reason_extract: None,
        exclude_reason_extract: None,
        canonical_id: Some(FileId(7)),
        flags: FileFlags::default(),
        phase: FilePhase::Rehashed,
        new_name: Some("renamed".into()),
    };
    let sid = SidecarEntry::from(&rec);
    assert_eq!(sid.id, 7);
    assert_eq!(sid.abs_path, rec.abs_path);
    assert_eq!(sid.ext, "pdf");
    assert_eq!(sid.size, 1234);
    assert_eq!(sid.sha1.as_deref(), Some("abababababababababababababababababababab"));
    assert_eq!(sid.mtime, Some(t));
    assert_eq!(sid.atime, Some(t));
    assert_eq!(sid.ctime, None);
    assert_eq!(sid.uid, Some(1000));
    assert_eq!(sid.gid, Some(100));
    assert_eq!(sid.username.as_deref(), Some("alice"));
    assert_eq!(sid.groupname.as_deref(), Some("users"));
    assert_eq!(sid.mode, Some(0o640));
    assert_eq!(sid.ftype, "file", "stable FileType::as_str string");
    assert_eq!(sid.device_id, Some(1));
    assert_eq!(sid.inode, Some(99));
    assert_eq!(sid.major, None);
    assert_eq!(sid.minor, None);
    assert_eq!(sid.xattrs.as_deref(), Some("{}"));
    assert_eq!(sid.posix_acl.as_deref(), Some(r#"{"user-obj":7}"#));
    assert_eq!(sid.selinux.as_deref(), Some("system_u:object_r:etc_t:s0"));
    assert_eq!(sid.win_perm, None);
    assert_eq!(sid.link_dst, None);
    assert_eq!(sid.new_name.as_deref(), Some("renamed"));
    // Out-tree embed starts at the file's own identity as a placeholder.
    assert_eq!(sid.out_tree.id, 7);
    assert_eq!(sid.out_tree.target_path, rec.abs_path);
}

#[test]
fn sidecar_symlink_renders_type_and_target() {
    let rec = FileRecord {
        id: FileId(3),
        abs_path: PathBuf::from("/data/link"),
        ext: String::new(),
        size: 0,
        sha1: None,
        mtime: None,
        atime: None,
        ctime: None,
        uid: None,
        gid: None,
        username: None,
        groupname: None,
        mode: None,
        ftype: FileType::Symlink(LinkType::File),
        device_id: None,
        inode_id: None,
        major: None,
        minor: None,
        xattrs: None,
        posix_acl: None,
        selinux_ctx: None,
        win_perm: None,
        link_dst: Some(PathBuf::from("/data/target")),
        include_reason_archive: None,
        exclude_reason_archive: None,
        include_reason_extract: None,
        exclude_reason_extract: None,
        canonical_id: Some(FileId(3)),
        flags: FileFlags::default(),
        phase: FilePhase::Rehashed,
        new_name: None,
    };
    let sid = SidecarEntry::from(&rec);
    assert_eq!(sid.ftype, "symlink_file");
    assert_eq!(sid.link_dst.as_deref(), Some(Path::new("/data/target")));
    assert_eq!(sid.sha1, None);
}

#[test]
fn sidecar_attach_out_tree_overrides_placeholder() {
    let rec = FileRecord {
        id: FileId(11),
        abs_path: PathBuf::from("/orig/name.txt"),
        ext: "txt".into(),
        size: 0,
        sha1: None,
        mtime: None,
        atime: None,
        ctime: None,
        uid: None,
        gid: None,
        username: None,
        groupname: None,
        mode: None,
        ftype: FileType::File,
        device_id: None,
        inode_id: None,
        major: None,
        minor: None,
        xattrs: None,
        posix_acl: None,
        selinux_ctx: None,
        win_perm: None,
        link_dst: None,
        include_reason_archive: None,
        exclude_reason_archive: None,
        include_reason_extract: None,
        exclude_reason_extract: None,
        canonical_id: Some(FileId(11)),
        flags: FileFlags::default(),
        phase: FilePhase::Rehashed,
        new_name: None,
    };
    let sid = SidecarEntry::from(&rec).with_out_tree(
        OutTreeId(42),
        Path::new("/out/placed/name_1.txt"),
    );
    assert_eq!(sid.out_tree.id, 42);
    assert_eq!(
        sid.out_tree.target_path,
        PathBuf::from("/out/placed/name_1.txt")
    );
}

#[test]
fn file_type_as_str_roundtrip() {
    let kinds = [
        FileType::File,
        FileType::Directory,
        FileType::Socket,
        FileType::FIFO,
        FileType::CharacterDevice,
        FileType::BlockDevice,
        FileType::Unknown,
        FileType::Symlink(LinkType::File),
        FileType::Symlink(LinkType::Directory),
        FileType::Symlink(LinkType::Socket),
        FileType::Symlink(LinkType::FIFO),
        FileType::Symlink(LinkType::CharacterDevice),
        FileType::Symlink(LinkType::BlockDevice),
        FileType::Symlink(LinkType::Dangling),
        FileType::Symlink(LinkType::Cycle),
        FileType::Symlink(LinkType::Unknown),
    ];

    for kind in kinds {
        assert_eq!(FileType::parse(kind.as_str()).expect("parse"), kind);
    }
}

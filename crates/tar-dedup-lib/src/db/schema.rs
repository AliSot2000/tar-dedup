use rusqlite::Connection;

use crate::error::{Result, ToPanic};

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).to_panic()?;
    Ok(())
}

pub const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS filter_reason_archive (
    id     INTEGER PRIMARY KEY CHECK (id > -9223372036854775807), -- rule >= 0 exclude, < 0 include rule
    source TEXT NOT NULL, -- e.g. --exclude=some-\regex-pattern or --exclude-from=path-to-file, line
    line   INTEGER, -- for include/exclude arguments this is the index --exclude=<ptrn>:0 --exclude=<other-ptrn>:1
    expression TEXT NOT NULL -- actual regex expression to match against
);

CREATE TABLE IF NOT EXISTS filter_reason_extract (
    id     INTEGER PRIMARY KEY CHECK (id > -9223372036854775807), -- rule >= 0 exclude, < 0 include rule
    source TEXT NOT NULL, -- e.g. --exclude=some-\regex-pattern or --exclude-from=path-to-file, line
    line   INTEGER, -- for include/exclude arguments this is the index --exclude=<ptrn>:0 --exclude=<other-ptrn>:1
    expression TEXT NOT NULL -- actual regex expression to match against
);

CREATE TABLE IF NOT EXISTS source (
    -- id = 0 is the reserved dummy absolute-root source (see its INSERT
    -- below); real rows are id > 0.
    id INTEGER PRIMARY KEY CHECK (id >= 0),
    source TEXT NOT NULL, --from-file or --input-dir
    abs_path TEXT NOT NULL,
    original_path TEXT NOT NULL,
    abs_path_raw BLOB NOT NULL,
    original_path_raw BLOB NOT NULL,
    line INTEGER,
    flags INTEGER NOT NULL DEFAULT 0,
    UNIQUE (source, abs_path_raw, line)
);
CREATE INDEX IF NOT EXISTS idx_source_dirs ON source(abs_path) WHERE (flags & 1) != 0;

CREATE TABLE IF NOT EXISTS files (
    -- id = 0 is the reserved marker for a symlink target that does not exist
    -- (see its INSERT below); real rows are id > 0.
    id            INTEGER PRIMARY KEY CHECK (id >= 0),
    abs_path      TEXT NOT NULL,
    abs_path_raw  BLOB NOT NULL UNIQUE,
    ext           TEXT NOT NULL,

    -- File Attributes
    size          INTEGER NOT NULL,
    ftype         TEXT NOT NULL,
    sha1          BLOB,
    mtime         TEXT,
    atime         TEXT,
    ctime         TEXT,
    uid           INTEGER,
    username      TEXT,
    gid           INTEGER,
    groupname     TEXT,
    mode          INTEGER,
    inode         INTEGER,
    dev           INTEGER,
    major         INTEGER,
    minor         INTEGER,
    new_name      TEXT, --used to store name transformations.

    -- Extended File Attributes
    xattr         TEXT,
    acl           TEXT,
    selinux       BLOB,
    win_perm      TEXT,   -- Windows attribute/DACL dump; NULL on unix
    link_dst      TEXT,
    link_dst_raw  BLOB,   -- original symlink-target bytes (doc-only), NULL otherwise

    -- Filtering
    include_reason_archive   INTEGER REFERENCES filter_reason_archive(id) DEFAULT 0 CHECK (include_reason_archive <= 0),
    exclude_reason_archive   INTEGER REFERENCES filter_reason_archive(id) DEFAULT 0 CHECK (exclude_reason_archive >= 0),
    include_reason_extract   INTEGER REFERENCES filter_reason_extract(id) DEFAULT 0 CHECK (include_reason_extract <= 0),
    exclude_reason_extract   INTEGER REFERENCES filter_reason_extract(id) DEFAULT 0 CHECK (exclude_reason_extract >= 0),

    -- Internal Stuff
    sparse_count   INTEGER DEFAULT 0,
    canonical_id   INTEGER REFERENCES files(id),
    -- The `files.id` this symlink resolves to (NULL for non-symlinks); when the
    -- target does not exist in the tree it points at the reserved id=0 marker.
    link_dst_id    INTEGER REFERENCES files(id),
    phase          TEXT NOT NULL DEFAULT 'inventoried',
    -- The archive session this row's payload was appended in (NULL = not yet
    -- appended in a finalized/non-truncated session). Replaces `AppendedPath`.
    session_id     INTEGER REFERENCES archive_sessions(id),
    flags          INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_files_sha1_size ON files(size, sha1);
CREATE INDEX IF NOT EXISTS idx_files_canonical ON files(canonical_id);
CREATE INDEX IF NOT EXISTS idx_files_phase ON files(phase);
CREATE INDEX IF NOT EXISTS idx_files_abs_path ON files(abs_path);

CREATE TABLE IF NOT EXISTS ref (
    source_id INTEGER NOT NULL REFERENCES source(id),
    file_id   INTEGER NOT NULL REFERENCES files(id),
    PRIMARY KEY (source_id, file_id)
);
CREATE INDEX IF NOT EXISTS idx_ref_file ON ref(file_id);

CREATE TABLE IF NOT EXISTS out_tree (
    -- id = 0 is the reserved marker for a symlink target that does not exist
    -- (see its INSERT below); real rows are id > 0.
    id       INTEGER PRIMARY KEY CHECK (id >= 0),
    canonical_id INTEGER REFERENCES out_tree(id),
    -- The `out_tree.id` this symlink resolves to (NULL for non-symlinks); when
    -- the target does not exist in the tree it points at the reserved id=0 marker.
    link_dst_id INTEGER REFERENCES out_tree(id),
    abs_path TEXT NOT NULL UNIQUE,
    file_id  INTEGER REFERENCES files(id), --first row that resolved to this out_tree_row
    flags    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_out_tree_file ON out_tree(file_id) WHERE file_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_out_tree_abs_path ON out_tree(abs_path);

CREATE TABLE IF NOT EXISTS ref_out (
    out_id    INTEGER NOT NULL REFERENCES out_tree(id),
    source_id INTEGER NOT NULL REFERENCES source(id),
    -- The `files` row this out_tree row was materialized from. Deliberately no
    -- PRIMARY KEY: with `--no-strict-separation` / transforms several files can
    -- map onto one out_tree row (a clobber), and every mapped file must be
    -- observable so `clobber_statistics` can count the conflicts.
    file_id   INTEGER NOT NULL REFERENCES files(id)
);
CREATE INDEX IF NOT EXISTS idx_ref_out_source ON ref_out(source_id);
CREATE INDEX IF NOT EXISTS idx_ref_out_out ON ref_out(out_id);

CREATE TABLE IF NOT EXISTS errors (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    file_id INTEGER REFERENCES files(id),
    out_tree_id INTEGER REFERENCES out_tree(id),
    abs_path TEXT,
    error_msg TEXT NOT NULL,
    error_type TEXT,
    phase TEXT NOT NULL,
    error_misc TEXT, --present for everything you want to store but don't know what it is going to
    --be like. Probably an enum with content and json serde.
    error_datetime TEXT NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_errors_id ON errors(id);
CREATE INDEX IF NOT EXISTS idx_errors_time ON errors(error_datetime);
CREATE INDEX IF NOT EXISTS idx_errors_file_id ON errors(file_id) WHERE file_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_errors_out_tree_id ON errors(out_tree_id) WHERE out_tree_id IS NOT NULL;

-- New tables (currently not implemented)
CREATE TABLE IF NOT EXISTS shards (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    sha1 BLOB NOT NULL,
    size INTEGER NOT NULL CHECK (size >= 0),
    source INTEGER NOT NULL REFERENCES files(id)
);
CREATE INDEX IF NOT EXISTS idx_shard_id ON shards(id);
CREATE INDEX IF NOT EXISTS idx_shard_sha ON shards(sha1);

CREATE TABLE IF NOT EXISTS file_shards (
    file_id INTEGER NOT NULL REFERENCES files(id),
    file_idx INTEGER NOT NULL DEFAULT 0,
    sha1 BLOB NOT NULL,
    offset INTEGER NOT NULL CHECK (offset >= 0),
    size INTEGER NOT NULL CHECK (size >= 0),
    flags INTEGER DEFAULT 0,
    shard_id INTEGER NOT NULL REFERENCES shards(id),
    UNIQUE (file_id, file_idx)
);
CREATE INDEX IF NOT EXISTS idx_file_shards_id_idx ON file_shards(file_id, file_idx);
CREATE INDEX IF NOT EXISTS idx_file_shards_sha1 ON file_shards(sha1);
CREATE INDEX IF NOT EXISTS idx_file_shards_shard_id ON file_shards(shard_id);

-- finalized:
-- 0 = open/interrupted,
-- 1 = stream closed successfully,
-- 2 = recovered/aborted
CREATE TABLE IF NOT EXISTS archive_sessions (
    id             INTEGER PRIMARY KEY CHECK (id > 0),
    archive_offset INTEGER NOT NULL DEFAULT 0,
    finalized      INTEGER NOT NULL DEFAULT 0,
    started_at     TEXT NOT NULL,
    finished_at    TEXT
);

-- Add dummy rows
INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) VALUES (0, 'internal', NULL, '.*');
INSERT OR IGNORE INTO filter_reason_extract (id, source, line, expression) VALUES (0, 'internal', NULL, '.*');

-- Reserved id=0 files row: the target of a symlink that does not exist in the
-- tree. link_dst_id = 0 means no resolvable target, distinct from NULL (not a
-- symlink). Not a file; filtered out of count_entries (id > 0).
INSERT OR IGNORE INTO files (id, abs_path, abs_path_raw, ext, size, ftype, phase) VALUES (0, '', X'', '', 0, 'unknown', 'inventoried');

-- Reserved id=0 out_tree row: an output-tree symlink target that does not exist
-- in the tree. link_dst_id = 0 means no resolvable target, distinct from NULL
-- (not a symlink). Not a file; excluded from the content projections.
INSERT OR IGNORE INTO out_tree (id, abs_path, file_id, flags) VALUES (0, '', NULL, 0);

-- Reserved id=0 source row: the dummy absolute-root source under which the
-- `--absolute` placement pass records every out_tree→files mapping in `ref_out`
-- (absolute names carry no real input-dir source). Source reads gate on
-- `id_gt_zero()` so this sentinel never surfaces as a real source.
INSERT OR IGNORE INTO source
    (id, source, abs_path, original_path, abs_path_raw, original_path_raw, line, flags)
    VALUES (0, 'absolute-root', '/', '/', X'2F', X'2F', NULL, 0);
";

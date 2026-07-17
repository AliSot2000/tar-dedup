use rusqlite::Connection;

use crate::error::Result;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY,
    rel_path      TEXT NOT NULL UNIQUE,

    -- File Attributes
    size          INTEGER NOT NULL,
    sha1          BLOB,
    mtime         TEXT,
    atime         TEXT,
    ctime         TEXT,
    uid           INTEGER,
    username      TEXT,
    gid           INTEGER,
    groupname     TEXT,
    mode          INTEGER,

    -- Extended File Attributes
    xattr         TEXT,
    acl           TEXT,
    selinux       BLOB,

    -- Extract Data
    extract_mtime TEXT,
    extract_atime TEXT,
    extract_ctime TEXT,

    -- Internal Stuff
    sparse_count  INTEGER,
    excluded      INTEGER,
    canonical_id  INTEGER REFERENCES files(id),
    phase         TEXT NOT NULL DEFAULT 'inventoried',
    snapshot_archived INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_files_sha1_size ON files(sha1, size);
CREATE INDEX IF NOT EXISTS idx_files_canonical ON files(canonical_id);
CREATE INDEX IF NOT EXISTS idx_files_phase ON files(phase);

CREATE TABLE IF NOT EXISTS archive_sessions (
    id             INTEGER PRIMARY KEY,
    stream_index   INTEGER NOT NULL,
    bytes_in       INTEGER NOT NULL DEFAULT 0,
    bytes_out      INTEGER NOT NULL DEFAULT 0,
    archive_offset INTEGER NOT NULL DEFAULT 0,
    finalized      INTEGER NOT NULL DEFAULT 0,
    started_at     TEXT NOT NULL,
    finished_at    TEXT
);
";

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

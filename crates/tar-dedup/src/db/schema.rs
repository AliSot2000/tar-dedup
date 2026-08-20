use rusqlite::Connection;

use crate::error::Result;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS filter_reason (
    id     INTEGER PRIMARY KEY CHECK (id > -9223372036854775807), -- rule >= 0 exclude, < 0 include rule
    source TEXT NOT NULL, -- e.g. --exclude=some-\regex-pattern or --exclude-from=path-to-file, line
    line   INTEGER, -- for include/exclude arguments this is the index --exclude=<ptrn>:0 --exclude=<other-ptrn>:1
    expression TEXT NOT NULL -- actual regex expression to match against
);

CREATE TABLE IF NOT EXISTS source (
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL,
    abs_path TEXT NOT NULL,
    original_path TEXT,
    line INTEGER,
    UNIQUE (source, path, line)
);

CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY,
    abs_path      TEXT NOT NULL UNIQUE,
    ext           TEXT NOT NULL,

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
    ftype         TEXT,
    inode         INTEGER,
    dev           INTEGER,
    new_name      TEXT, --used to store name transformations.

    -- Extended File Attributes
    xattr         TEXT,
    acl           TEXT,
    selinux       BLOB,
    link_dst      TEXT,

    -- Internal Stuff
    sparse_count   INTEGER DEFAULT 0,
    include_reason INTEGER REFERENCES filter_reason(id) DEFAULT 0,
    exclude_reason INTEGER REFERENCES filter_reason(id) DEFAULT 0,
    source_id      INTEGER REFERENCES source(id), --relevant to entry point (e.g. cross tree starts with links not recorded)
    canonical_id   INTEGER REFERENCES files(id),
    phase          TEXT NOT NULL DEFAULT 'inventoried',
    flags          INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_files_sha1_size ON files(sha1, size);
CREATE INDEX IF NOT EXISTS idx_files_canonical ON files(canonical_id);
CREATE INDEX IF NOT EXISTS idx_files_phase ON files(phase);
CREATE INDEX IF NOT EXISTS idx_files_abs_path ON files(abs_path);

-- finalized:
-- 0 = open/interrupted,
-- 1 = stream closed successfully,
-- 2 = recovered/aborted
CREATE TABLE IF NOT EXISTS archive_sessions (
    id             INTEGER PRIMARY KEY,
    archive_offset INTEGER NOT NULL DEFAULT 0,
    finalized      INTEGER NOT NULL DEFAULT 0,
    started_at     TEXT NOT NULL,
    finished_at    TEXT
);

-- Add dummy row
INSERT INTO filter_reason (id, source, line, expression) VALUES (0, 'internal', NULL, '*');
";

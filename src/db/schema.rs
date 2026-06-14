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
    size          INTEGER NOT NULL,
    sha1          BLOB,
    mtime         INTEGER,
    atime         INTEGER,
    uid           INTEGER,
    gid           INTEGER,
    mode          INTEGER,
    canonical_id  INTEGER REFERENCES files(id),
    phase         TEXT NOT NULL DEFAULT 'inventoried'
);

CREATE INDEX IF NOT EXISTS idx_files_sha1_size ON files(sha1, size);
CREATE INDEX IF NOT EXISTS idx_files_canonical ON files(canonical_id);
CREATE INDEX IF NOT EXISTS idx_files_phase ON files(phase);

CREATE TABLE IF NOT EXISTS archive_sessions (
    id            INTEGER PRIMARY KEY,
    stream_index  INTEGER NOT NULL,
    bytes_in      INTEGER NOT NULL DEFAULT 0,
    bytes_out     INTEGER NOT NULL DEFAULT 0,
    finalized     INTEGER NOT NULL DEFAULT 0,
    started_at    TEXT NOT NULL,
    finished_at   TEXT
);

CREATE TABLE IF NOT EXISTS archive_entries (
    id            INTEGER PRIMARY KEY,
    file_id       INTEGER NOT NULL REFERENCES files(id),
    session_id    INTEGER NOT NULL REFERENCES archive_sessions(id),
    tar_path      TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_archive_entries_status ON archive_entries(status);
";

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

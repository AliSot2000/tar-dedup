use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelinePhase {
    Inventory,
    Hash,
    Filter,
    Dedup,
    Sparsify,
    Stage,
    Archive,
    Done,
}

impl PipelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Hash => "hash",
            Self::Filter => "filter",
            Self::Dedup => "dedup",
            Self::Sparsify => "sparsify",
            Self::Stage => "stage",
            Self::Archive => "archive",
            Self::Done => "done",
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Inventory => Some(Self::Hash),
            Self::Hash => Some(Self::Filter),
            Self::Filter => Some(Self::Dedup),
            Self::Dedup => Some(Self::Sparsify),
            Self::Sparsify => Some(Self::Stage),
            Self::Stage => Some(Self::Archive),
            Self::Archive => Some(Self::Done),
            Self::Done => None,
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "inventory" => Ok(Self::Inventory),
            "hash" => Ok(Self::Hash),
            "filter" => Ok(Self::Filter),
            "dedup" => Ok(Self::Dedup),
            "sparsify" => Ok(Self::Sparsify),
            "stage" => Ok(Self::Stage),
            "archive" => Ok(Self::Archive),
            "done" => Ok(Self::Done),
            other => Err(crate::error::Error::Config(format!(
                "unknown pipeline phase: {other}"
            ))),
        }
    }
}

/// Extract pipeline driver phase (persisted in meta as `extract_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractPipelinePhase {
    ScanTar,
    Rehash,
    Place,
    Permissions,
    Cleanup,
    Done,
}

impl ExtractPipelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScanTar => "scan_tar",
            Self::Rehash => "rehash",
            Self::Place => "place",
            Self::Permissions => "permissions",
            Self::Cleanup => "cleanup",
            Self::Done => "done",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "scan_tar" => Ok(Self::ScanTar),
            "rehash" => Ok(Self::Rehash),
            "place" => Ok(Self::Place),
            "permissions" => Ok(Self::Permissions),
            "cleanup" => Ok(Self::Cleanup),
            "done" => Ok(Self::Done),
            other => Err(crate::error::Error::Config(format!(
                "unknown extract pipeline phase: {other}"
            ))),
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::ScanTar => Some(Self::Rehash),
            Self::Rehash => Some(Self::Place),
            Self::Place => Some(Self::Permissions),
            Self::Permissions => Some(Self::Cleanup),
            Self::Cleanup => Some(Self::Done),
            Self::Done => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractRuntimeState {
    pub phase: ExtractPipelinePhase,
    pub snapshots_ingested: u32,
}

impl ExtractRuntimeState {
    pub fn new() -> Self {
        Self {
            phase: ExtractPipelinePhase::ScanTar,
            snapshots_ingested: 0,
        }
    }
}

impl Default for ExtractRuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub snapshot_taken_at: DateTime<Utc>,
    pub phase: PipelinePhase,
    pub max_workers: usize,
}

impl RuntimeState {
    pub fn new(max_workers: usize) -> Self {
        Self {
            snapshot_taken_at: Utc::now(),
            phase: PipelinePhase::Inventory,
            max_workers,
        }
    }
}

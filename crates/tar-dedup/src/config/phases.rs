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

    pub fn next(self, eager_filter: bool) -> Option<Self> {
        match (self, eager_filter) {
            (Self::Inventory, true) => Some(Self::Filter),
            (Self::Inventory, false) => Some(Self::Hash),
            (Self::Hash, true) => Some(Self::Dedup),
            (Self::Hash, false) => Some(Self::Filter),
            (Self::Filter, true) => Some(Self::Hash),
            (Self::Filter, false) => Some(Self::Dedup),
            (Self::Dedup, _) => Some(Self::Sparsify),
            (Self::Sparsify, _) => Some(Self::Stage),
            (Self::Stage, _) => Some(Self::Archive),
            (Self::Archive, _) => Some(Self::Done),
            (Self::Done, _) => None,
        }
    }

    /// Phase ordinal used for the global bar anchor (`index * table_size`).
    pub fn index(self) -> u64 {
        match self {
            Self::Inventory => 0,
            Self::Hash => 1,
            Self::Filter => 2,
            Self::Dedup => 3,
            Self::Sparsify => 4,
            Self::Stage => 5,
            Self::Archive => 6,
            Self::Done => 7,
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
    Filter,
    Rehash,
    PlacementPrologue,
    Place,
    Permissions,
    Cleanup,
    Done,
}

impl ExtractPipelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScanTar => "scan_tar",
            Self::Filter => "filter",
            Self::Rehash => "rehash",
            Self::PlacementPrologue => "placement_prologue",
            Self::Place => "place",
            Self::Permissions => "permissions",
            Self::Cleanup => "cleanup",
            Self::Done => "done",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "scan_tar" => Ok(Self::ScanTar),
            "filter" => Ok(Self::Filter),
            "rehash" => Ok(Self::Rehash),
            "placement_prologue" => Ok(Self::PlacementPrologue),
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
            Self::ScanTar => Some(Self::Filter),
            Self::Filter => Some(Self::Rehash),
            Self::Rehash => Some(Self::PlacementPrologue),
            Self::PlacementPrologue => Some(Self::Place),
            Self::Place => Some(Self::Permissions),
            Self::Permissions => Some(Self::Cleanup),
            Self::Cleanup => Some(Self::Done),
            Self::Done => None,
        }
    }

    /// Phase ordinal used for the global bar anchor (`index * table_size`).
    /// `Cleanup` is index 6 of a 6-element pipeline, so it snaps the global to
    /// 100% (no per-element work of its own).
    pub fn index(self) -> u64 {
        match self {
            Self::ScanTar => 0,
            Self::Filter => 1,
            Self::Rehash => 2,
            Self::PlacementPrologue => 3,
            Self::Place => 4,
            Self::Permissions => 5,
            Self::Cleanup => 6,
            Self::Done => 7,
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

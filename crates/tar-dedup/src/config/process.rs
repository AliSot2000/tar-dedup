use serde::{Deserialize, Serialize};

use crate::cli::ExitAfterStageArg;
use crate::common::defaults::{BATCH_SIZE, DEFAULT_AUTO_FLUSH_LIMIT, DRAIN_CHUNK, FEED_CHUNK, OUT_CAPACITY, WORK_CAPACITY};
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::Validate;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupSettings {
    /// if true, keep the database.
    pub keep_db: bool,
    /// if true, keep the stage directory (.astage / .estage)
    pub keep_stage: bool,
}

impl CleanupSettings {
    pub fn from_flags(keep_db: bool, keep_stage: bool) -> Self {
        Self { keep_db, keep_stage }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitAfterStage {
    Inventory,
    Hash,
    Filter,
    Dedup,
    Sparsify,
    Stage,
    Archive,
    Cleanup,
}

impl From<ExitAfterStageArg> for ExitAfterStage {
    fn from(arg: ExitAfterStageArg) -> Self {
        match arg {
            ExitAfterStageArg::Inventory => Self::Inventory,
            ExitAfterStageArg::Hash => Self::Hash,
            ExitAfterStageArg::Filter => Self::Filter,
            ExitAfterStageArg::Dedup => Self::Dedup,
            ExitAfterStageArg::Sparsify => Self::Sparsify,
            ExitAfterStageArg::Stage => Self::Stage,
            ExitAfterStageArg::Archive => Self::Archive,
            ExitAfterStageArg::Cleanup => Self::Cleanup,
        }
    }
}

impl ExitAfterStage {
    /// Pipeline phase whose successful completion triggers exit (`None` = run through cleanup).
    pub fn stop_after_phase(self) -> Option<super::phases::PipelinePhase> {
        use super::phases::PipelinePhase;
        match self {
            Self::Inventory => Some(PipelinePhase::Inventory),
            Self::Hash => Some(PipelinePhase::Hash),
            Self::Filter => Some(PipelinePhase::Filter),
            Self::Dedup => Some(PipelinePhase::Dedup),
            Self::Sparsify => Some(PipelinePhase::Sparsify),
            Self::Stage => Some(PipelinePhase::Stage),
            Self::Archive => Some(PipelinePhase::Archive),
            Self::Cleanup => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessOptions {
    pub start_policy: StartPolicy,
    /// Compute-bound workers (compression, pure-db rayon pools).
    pub jobs: usize,
    /// I/O-bound workers (sparsify, dedup, place pools).
    pub io_jobs: usize,
    /// Number of error drafts buffered before an auto-flush to the database.
    pub auto_flush_limit: u64,
    /// Rows pulled from the database per phase iteration.
    pub batch_size: u64,
    /// Items in the producer/consumer work channel.
    pub work_capacity: usize,
    /// Items in the producer/consumer result channel.
    pub out_capacity: usize,
    /// Rows pulled from the database per feed round.
    pub feed_chunk: usize,
    /// Outcomes committed per transaction.
    pub drain_chunk: usize,
    pub fail_fast: bool,
    pub no_errors: bool,
    /// Force: continue even if the tool cannot guarantee the integrity of the data.
    pub force: bool,
    /// Lossily convert non-UTF-8 paths (`to_string_lossy`, never skip); the
    /// original bytes are kept for fs access and inspection.
    /// Mutually exclusive with `--force`.
    pub relax_utf8: bool,
    /// Which progress bars are drawn.
    pub progress_mode: crate::cli::ProgressMode,
    pub cleanup: CleanupSettings,
    pub exit_after_stage: Option<ExitAfterStage>,
}

impl Default for ProcessOptions {
    fn default() -> Self {
        Self {
            start_policy: StartPolicy::Create,
            jobs: num_cpus::get(),
            io_jobs: num_cpus::get(),
            auto_flush_limit: DEFAULT_AUTO_FLUSH_LIMIT,
            batch_size: BATCH_SIZE,
            work_capacity: WORK_CAPACITY,
            out_capacity: OUT_CAPACITY,
            feed_chunk: FEED_CHUNK,
            drain_chunk: DRAIN_CHUNK,
            fail_fast: false,
            no_errors: false,
            force: false,
            relax_utf8: false,
            progress_mode: crate::cli::ProgressMode::PerThread,
            cleanup: CleanupSettings { keep_db: false, keep_stage: false },
            exit_after_stage: None,
        }
    }
}

impl ProcessOptions {
    /// Workers for stages that are both compute- and io-bound (hash, rehash).
    pub fn effective_jobs(&self) -> usize {
        self.jobs.min(self.io_jobs)
    }
}

impl Validate for ProcessOptions {
    fn validate(&self) -> Result<()> {
        // INFO: Kept in case I made a mistake. But force shoud not have anything to do with this.
        // if self.relax_utf8 && self.force {
        //     return Err(Error::Config(
        //         "--relax-utf8 is mutually exclusive with --force".into(),
        //     ));
        // }
        for (value, name) in [
            (self.auto_flush_limit, "--auto-flush-limit"),
            (self.batch_size, "--batch-size"),
            (self.work_capacity as u64, "--work-capacity"),
            (self.out_capacity as u64, "--out-capacity"),
            (self.feed_chunk as u64, "--feed-chunk"),
            (self.drain_chunk as u64, "--drain-chunk"),
        ] {
            if value == 0 {
                return Err(Error::Config(format!("{name} must be greater than 0")));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeOverrides {
    pub jobs: Option<usize>,
    pub io_jobs: Option<usize>,
    pub exit_after_stage: Option<ExitAfterStage>,
}

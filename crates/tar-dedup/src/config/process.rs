use crate::cli::ExitAfterStageArg;
use crate::common::start::StartPolicy;

#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupSettings {
    pub keep_db: bool,
    pub keep_stage: bool,
}

impl CleanupSettings {
    pub fn from_flags(keep_db: bool, keep_stage: bool) -> Self {
        Self { keep_db, keep_stage }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct ProcessOptions {
    pub start_policy: StartPolicy,
    pub jobs: usize,
    pub fail_fast: bool,
    pub no_errors: bool,
    pub cleanup: CleanupSettings,
    pub exit_after_stage: Option<ExitAfterStage>,
}

#[derive(Debug, Clone, Default)]
pub struct ResumeOverrides {
    pub jobs: Option<usize>,
    pub exit_after_stage: Option<ExitAfterStage>,
}

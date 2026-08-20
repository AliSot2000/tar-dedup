//! Shared Create / Fresh / Resume start policy for archive and extract.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPolicy {
    /// New run; error if incomplete work or a finished product already exists.
    Create,
    /// Wipe work and start from the beginning.
    Fresh,
    /// Require incomplete work; error if absent.
    Resume,
}

impl StartPolicy {
    pub fn create_or_fresh(fresh: bool) -> Self {
        if fresh {
            Self::Fresh
        } else {
            Self::Create
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkPresence {
    /// No usable work database / incomplete state.
    Absent,
    /// Saved state exists and is not finished.
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductPresence {
    /// No finished product short-circuit (extract always; archive when file missing/invalid).
    Absent,
    /// Finished archive present (valid footer) or an archive file already exists.
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    RunFresh,
    Resume,
}

pub fn resolve_start(
    policy: StartPolicy,
    work: WorkPresence,
    product: ProductPresence,
) -> Result<StartAction> {
    match policy {
        StartPolicy::Fresh => Ok(StartAction::RunFresh),
        StartPolicy::Resume => match work {
            WorkPresence::Incomplete => Ok(StartAction::Resume),
            WorkPresence::Absent => Err(Error::Config(
                "no incomplete work to resume (`resume` requires an existing work directory)"
                    .into(),
            )),
        },
        StartPolicy::Create => match (work, product) {
            (WorkPresence::Incomplete, _) => Err(Error::Config(
                "incomplete work already exists; use `resume --work-dir` to continue or `--fresh` to start over"
                    .into(),
            )),
            (WorkPresence::Absent, ProductPresence::Finished) => Err(Error::Config(
                "output already exists; use `--fresh` to replace it"
                    .into(),
            )),
            (WorkPresence::Absent, ProductPresence::Absent) => Ok(StartAction::RunFresh),
        },
    }
}

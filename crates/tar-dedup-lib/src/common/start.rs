//! Shared Create / Fresh / Resume start policy for archive and extract.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
        if fresh { Self::Fresh } else { Self::Create }
    }
}

/// State of the work database at start time.
///
/// `Absent` = no usable work db (none exists, or it carries no extract-phase
/// meta — e.g. a fresh bypass product's baked catalog, which is not yet an
/// extract work-db). `Incomplete` = saved state present and not finished.
/// `Done` = saved state present and finished (a completed run whose work-db
/// survives, e.g. a retained bypass/keep-stage product).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkPresence {
    /// No usable work database / incomplete state.
    Absent,
    /// Saved state exists and is not finished.
    Incomplete,
    /// Saved state exists and is finished.
    Done,
}

/// Whether a finished product already exists.
///
/// Passed as `Some(...)` on the archive side, where the product is a concrete
/// artifact (a footer-bearing tar, or a bypass product dir holding the baked
/// catalog) whose presence is verifiable before starting. On the extract side
/// there is no finished-archive artifact to probe, so the caller passes `None`
/// and the finished state is instead conveyed through `WorkPresence::Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProductPresence {
    /// No finished product short-circuit.
    Absent,
    /// Finished archive present (valid footer) or an archive file already exists.
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartAction {
    RunFresh,
    Resume,
}

/// Decide how a run should start from the requested policy and the current
/// work/product state. `product` is `Some` on archive (a concrete artifact to
/// probe), `None` on extract.
pub fn resolve_start(
    policy: StartPolicy,
    work: WorkPresence,
    product: Option<ProductPresence>)
    -> Result<StartAction> {

    match policy {
        StartPolicy::Fresh => Ok(StartAction::RunFresh),
        StartPolicy::Resume => match work {
            WorkPresence::Incomplete => Ok(StartAction::Resume),
            WorkPresence::Absent => Err(Error::Config(
                "no incomplete work to resume (`resume` requires an existing work directory)"
                    .into(),
            )),
            WorkPresence::Done => Err(Error::Config(
                "no incomplete work to resume. (work exists but is already done)"
                    .into(),
            )),
        },
        StartPolicy::Create => match (work, product) {
            (WorkPresence::Incomplete, _) => Err(Error::Config(
                "incomplete work already exists; use `resume --work-dir` to continue or `--fresh` to start over"
                    .into(),
            )),
            (WorkPresence::Done, _) => Err(Error::Config(
                "work already finished; use `--fresh` to start over"
                    .into(),
            )),
            (WorkPresence::Absent, Some(ProductPresence::Finished)) => Err(Error::Config(
                "output already exists; use `--fresh` to replace it"
                    .into(),
            )),
            (WorkPresence::Absent, Some(ProductPresence::Absent))
            | (WorkPresence::Absent, None) => Ok(StartAction::RunFresh),
        },
    }
}
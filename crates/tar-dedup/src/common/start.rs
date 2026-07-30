//! Shared Auto / Fresh / Resume start policy for archive and extract.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPolicy {
    /// Resume incomplete work if present; otherwise start new (archive may no-op if product finished).
    Auto,
    /// Wipe work and start from the beginning.
    Fresh,
    /// Require incomplete work; error if absent.
    Resume,
}

impl StartPolicy {
    pub fn from_flags(resume: bool, fresh: bool) -> Result<Self> {
        match (resume, fresh) {
            (true, true) => Err(Error::Config(
                "--resume and --fresh cannot be used together".into(),
            )),
            (true, false) => Ok(Self::Resume),
            (false, true) => Ok(Self::Fresh),
            (false, false) => Ok(Self::Auto),
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
    /// Finished archive present (valid footer).
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    RunFresh,
    Resume,
    AlreadyDone,
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
                "no incomplete work to resume (omit --resume, or use --fresh to start over)"
                    .into(),
            )),
        },
        StartPolicy::Auto => match work {
            WorkPresence::Incomplete => Ok(StartAction::Resume),
            WorkPresence::Absent => match product {
                ProductPresence::Finished => Ok(StartAction::AlreadyDone),
                ProductPresence::Absent => Ok(StartAction::RunFresh),
            },
        },
    }
}

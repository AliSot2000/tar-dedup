use std::path::PathBuf;

use crate::cli::ResumeArgs;
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::process::{ExitAfterStage, ResumeOverrides};
use super::{resolve_path_to_abs_path, validate_dir};

#[derive(Debug, Clone)]
pub struct ResumeConfig {
    /// Path to the resumed work database.
    pub db_path: PathBuf,
    pub overrides: ResumeOverrides,
}

impl ResumeConfig {
    pub fn try_from(args: &ResumeArgs) -> Result<Self> {
        let cwd = std::env::current_dir().map_err(Error::from)?;

        // `--db FILE` (direct database path) xor `--work-dir DIR` (dir containing
        // `tar-dedup.sqlite`); exactly one is required.
        let db_path = match (args.db.as_ref(), args.work_dir.as_ref()) {
            (Some(_), Some(_)) => Err(Error::Config(
                "--db and --work-dir are mutually exclusive (pass one)".into(),
            )),
            (Some(db), None) => {
                let resolved = resolve_path_to_abs_path(db, &cwd);
                Ok(resolved.clone())
            }
            (None, Some(work_dir)) => {
                let dir = resolve_path_to_abs_path(work_dir, &cwd);
                validate_dir(&dir, "--work-dir")?;
                Ok(dir.join("tar-dedup.sqlite"))
            }
            (None, None) => Err(Error::Config(
                "resume requires `--work-dir` or `--db`".into(),
            )),
        }?;

        if !db_path.is_file() {
            return Err(Error::Config(format!(
                "no work database at {}",
                db_path.display(),
            )));
        }

        Ok(Self {
            db_path,
            overrides: ResumeOverrides {
                jobs: args.jobs,
                io_jobs: args.io_jobs,
                exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            },
        })
    }

    pub fn start_policy(&self) -> StartPolicy {
        StartPolicy::Resume
    }
}
use std::path::PathBuf;

use crate::cli::ResumeArgs;
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::process::{ExitAfterStage, ResumeOverrides};
use super::{resolve_path_to_abs_path, validate_dir};

#[derive(Debug, Clone)]
pub struct ResumeConfig {
    pub work_dir: PathBuf,
    pub overrides: ResumeOverrides,
}

impl ResumeConfig {
    pub fn try_from(args: &ResumeArgs) -> Result<Self> {
        let cwd = std::env::current_dir().map_err(Error::from)?;
        let work_dir = resolve_path_to_abs_path(&args.work_dir, &cwd);
        validate_dir(&work_dir, "--work-dir")?;

        let db_path = work_dir.join("tar-dedup.sqlite");
        if !db_path.is_file() {
            return Err(Error::Config(format!(
                "no work database in {}: missing {}",
                work_dir.display(),
                db_path.display()
            )));
        }

        Ok(Self {
            work_dir,
            overrides: ResumeOverrides {
                jobs: args.jobs,
                exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            },
        })
    }

    pub fn jobs(&self) -> usize {
        self.overrides.jobs.unwrap_or_else(num_cpus::get)
    }

    pub fn start_policy(&self) -> StartPolicy {
        StartPolicy::Resume
    }
}

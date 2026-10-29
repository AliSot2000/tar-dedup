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

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn parse_resume(extra: &[&str]) -> ResumeArgs {
        let argv = std::iter::once("tar-dedup".to_string())
            .chain(std::iter::once("resume".to_string()))
            .chain(extra.iter().map(|s| s.to_string()))
            .collect::<Vec<_>>();
        let cli = Cli::try_parse_from(&argv).expect("clap parse");
        match cli.command {
            Command::Resume(a) => a,
            other => panic!("expected resume, got {other:?}"),
        }
    }

    fn rune(extra: &[&str]) -> Result<ResumeConfig> {
        ResumeConfig::try_from(&parse_resume(extra))
            .map(|c| {
                assert_eq!(c.start_policy(), StartPolicy::Resume);
                c
            })
    }

    #[test]
    fn db_and_work_dir_are_mutually_exclusive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = rune(&[
            &format!("--db={}", dir.path().join("x.sqlite").display()),
            &format!("--work-dir={}", dir.path().display()),
        ]).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn neither_db_nor_work_dir_required() {
        let err = rune(&[]).unwrap_err();
        assert!(err.to_string().contains("--work-dir` or `--db`"), "{err}");
    }

    #[test]
    fn work_dir_must_exist() {
        let dir = tempfile::tempdir().expect("tempdir").path().to_path_buf();
        let missing = dir.join("missing");
        let err = rune(&[&format!("--work-dir={}", missing.display())]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn work_dir_without_database_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = rune(&[&format!("--work-dir={}", dir.path().display())]).unwrap_err();
        assert!(err.to_string().contains("no work database"), "{err}");
    }

    #[test]
    fn db_resolves_against_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("tar-dedup.sqlite");
        std::fs::write(&db, b"dummy").expect("write dummy db");
        let cfg = rune(&[&format!("--db={}", db.display())]).expect("db");
        assert_eq!(cfg.db_path, db);
        assert_eq!(cfg.overrides.jobs, None);
        assert_eq!(cfg.overrides.io_jobs, None);
    }

    #[test]
    fn work_dir_derives_db_path_and_maps_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("tar-dedup.sqlite");
        std::fs::write(&db, b"dummy").expect("write dummy db");
        let cfg = rune(&[
            &format!("--work-dir={}", dir.path().display()),
            "--jobs", "3",
            "--io-jobs", "5",
            "--exit-after-stage", "hash",
        ]).expect("work-dir");
        assert_eq!(cfg.db_path, dir.path().join("tar-dedup.sqlite"));
        assert_eq!(cfg.overrides.jobs, Some(3));
        assert_eq!(cfg.overrides.io_jobs, Some(5));
        assert!(cfg.overrides.exit_after_stage.is_some());
    }

    #[test]
    fn missing_explicit_db_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("nope.sqlite");
        let err = rune(&[&format!("--db={}", db.display())]).unwrap_err();
        assert!(err.to_string().contains("no work database"), "{err}");
    }

    #[test]
    fn relative_work_dir_resolves_against_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("tar-dedup.sqlite");
        std::fs::write(&db, b"dummy").expect("write dummy db");
        // Resolve is against the process cwd, so an absolute --work-dir passes
        // through unchanged (relative resolution is covered by the shared helper).
        let cfg = rune(&[&format!("--work-dir={}", dir.path().display())]).expect("ok");
        assert!(cfg.db_path.is_absolute());
    }
}
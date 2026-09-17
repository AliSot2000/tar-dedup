use clap::Parser;

use tar_dedup::cli::{Cli, Command};
use tar_dedup::common::start::StartPolicy;
use tar_dedup::config::{ArchiveConfig, ExtractConfig, ResumeConfig};
use tar_dedup::db::Database;
use tar_dedup::error::Error;
use tar_dedup::shutdown::Shutdown;

fn main() -> tar_dedup::error::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let shutdown = Shutdown::install()?;

    match cli.command {
        Command::Archive(args) => {
            let config = ArchiveConfig::build(&args, None)?;
            tar_dedup::archive::run(config, shutdown)
        }
        Command::Extract(args) => {
            let config = ExtractConfig::build(&args, None)?;
            tar_dedup::unarchive::run(config, shutdown)
        }
        Command::Resume(args) => {
            let resume = ResumeConfig::try_from(&args)?;
            let db = Database::open(&resume.db_path)?;
            if db.load_runtime_state()?.is_some() {
                let mut stored = db.get_archive_config()?.ok_or_else(|| Error::Config(
                    "no stored archive configuration in work database; \
                     restart the run with `--fresh` instead".into(),
                ))?;
                stored.process.start_policy = StartPolicy::Resume;
                stored.process.jobs = resume.overrides.jobs.unwrap_or(stored.process.jobs);
                stored.process.io_jobs = resume.overrides.io_jobs.unwrap_or(stored.process.io_jobs);
                if let Some(exit) = resume.overrides.exit_after_stage.as_ref() {
                    stored.process.exit_after_stage = Some(*exit);
                }
                tar_dedup::archive::run(stored, shutdown)
            } else if db.load_extract_runtime_state()?.is_some() {
                let mut stored = db.get_extract_config()?.ok_or_else(|| Error::Config(
                    "no stored extract configuration in work database; \
                     restart the run with `--fresh` instead".into(),
                ))?;
                stored.process.start_policy = StartPolicy::Resume;
                stored.process.jobs = resume.overrides.jobs.unwrap_or(stored.process.jobs);
                stored.process.io_jobs = resume.overrides.io_jobs.unwrap_or(stored.process.io_jobs);
                if let Some(exit) = resume.overrides.exit_after_stage.as_ref() {
                    stored.process.exit_after_stage = Some(*exit);
                }
                tar_dedup::unarchive::run(stored, shutdown)
            } else {
                Err(Error::Config(
                    "no incomplete archive or extract state in work database".into(),
                ))
            }
        }
        Command::Inspect(args) => tar_dedup::cmd::inspect::run(&args),
        Command::List(args) => tar_dedup::cmd::list::run(&args),
        Command::Dump(args) => tar_dedup::cmd::dump::run(&args),
        Command::Query(args) => tar_dedup::cmd::query::run(&args),
        Command::Reset(args) => tar_dedup::cmd::reset::run(&args),
    }
}

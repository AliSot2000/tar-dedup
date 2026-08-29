use clap::Parser;

use tar_dedup::cli::{Cli, Command};
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
            let config = ArchiveConfig::try_from(&args)?;
            tar_dedup::archive::run(config, shutdown)
        }
        Command::Extract(args) => {
            let config = ExtractConfig::try_from(&args)?;
            tar_dedup::unarchive::run(config, shutdown)
        }
        Command::Resume(args) => {
            let resume = ResumeConfig::try_from(&args)?;
            let work_dir = resume.work_dir.clone();
            let jobs = resume.jobs();
            let exit_after_stage = resume.overrides.exit_after_stage;
            let db = Database::open(&work_dir.join("tar-dedup.sqlite"))?;
            if db.load_runtime_state()?.is_some() {
                let config = ArchiveConfig::for_resume(work_dir, jobs, exit_after_stage);
                tar_dedup::archive::run(config, shutdown)
            } else if db.load_extract_runtime_state()?.is_some() {
                let config = ExtractConfig::for_resume(work_dir, jobs);
                tar_dedup::unarchive::run(config, shutdown)
            } else {
                Err(Error::Config(
                    "no incomplete archive or extract state in work directory".into(),
                ))
            }
        }
    }
}

use clap::Parser;

use tar_dedup::cli::{Cli, Command};
use tar_dedup::config::Config;
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
            let config = Config::from_archive_args(&args)?;
            tar_dedup::archive::run(config, shutdown)
        }
        Command::Extract(args) => {
            let config = Config::from_extract_args(&args)?;
            tar_dedup::unarchive::run(config, shutdown)
        }
        Command::Resume(args) => {
            // TODO separate file.
            let config = Config::from_resume_args(&args)?;
            let db = Database::open(&config.db_path())?;
            if db.load_runtime_state()?.is_some() {
                tar_dedup::archive::run(config, shutdown)
            } else if db.load_extract_runtime_state()?.is_some() {
                tar_dedup::unarchive::run(config, shutdown)
            } else {
                Err(Error::Config(
                    "no incomplete archive or extract state in work directory".into(),
                ))
            }
        }
    }
}

use clap::Parser;

use tar_dedup::cli::{Cli, Command};
use tar_dedup::config::Config;
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
            tar_dedup::pipeline::run_archive(config, shutdown)
        }
        Command::Extract(args) => {
            let config = Config::from_extract_args(&args)?;
            tar_dedup::pipeline::run_extract(config, shutdown)
        }
    }
}

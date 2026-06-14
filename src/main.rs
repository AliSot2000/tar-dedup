mod cli;
mod compression;
mod config;
mod content_id;
mod db;
mod error;
mod pipeline;
mod progress;
mod shutdown;
mod tar_writer;
mod workers;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::shutdown::Shutdown;

fn main() -> error::Result<()> {
    // Setup for debugging I guess
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let shutdown = Shutdown::install()?;

    match cli.command {
        Command::Archive(args) => {
            let config = Config::from_archive_args(&args)?;
            pipeline::run_archive(config, shutdown)
        }
        Command::Extract(args) => {
            let config = Config::from_extract_args(&args)?;
            pipeline::run_extract(&config, &db::Database::open(&config.output_dir.join("snapshot.sqlite"))?)
        }
    }
}

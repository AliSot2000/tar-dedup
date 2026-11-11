use clap::{CommandFactory, Parser};

use tar_dedup_lib::cli::{Cli, Command};
use tar_dedup_lib::common::start::StartPolicy;
use tar_dedup_lib::config::{ArchiveConfig, ExtractConfig, ResumeConfig, resolve_log_flags};
use tar_dedup_lib::db::Database;
use tar_dedup_lib::error::Error;
use tar_dedup_lib::shutdown::Shutdown;

fn main() -> tar_dedup_lib::error::Result<()> {
    let cli = Cli::parse();
    let shutdown = Shutdown::install()?;
    let (log_long_time, log_target) = resolve_log_flags(
        cli.log_format, cli.log_short_time, cli.log_long_time,
        cli.log_no_target, cli.log_target,
    );
    tar_dedup_lib::progress::init_tracing(cli.log_level, log_long_time, log_target, cli.log_time.as_deref());

    match cli.command {
        Command::Archive(args) => {
            let config = ArchiveConfig::build(&args, None)?;
            tar_dedup_lib::archive::run(config, shutdown)
        }
        Command::Extract(args) => {
            let config = ExtractConfig::build(&args, None)?;
            tar_dedup_lib::unarchive::run(config, shutdown)
        }
        Command::Resume(args) => {
            let resume = ResumeConfig::try_from(&args)?;
            let db = Database::open(&resume.db_path)?;
            if db.load_archive_runtime_state()?.is_some() {
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
                tar_dedup_lib::archive::run(stored, shutdown)
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
                tar_dedup_lib::unarchive::run(stored, shutdown)
            } else {
                Err(Error::Config(
                    "no incomplete archive or extract state in work database".into(),
                ))
            }
        }
        Command::Inspect(args) => tar_dedup_lib::cmd::inspect::run(&args),
        Command::List(args) => tar_dedup_lib::cmd::list::run(&args),
        Command::Dump(args) => tar_dedup_lib::cmd::dump::run(&args),
        Command::Query(args) => tar_dedup_lib::cmd::query::run(&args),
        Command::Reset(args) => tar_dedup_lib::cmd::reset::run(&args),
        Command::Check(args) => tar_dedup_lib::cmd::check::run(&args, &shutdown),
        Command::Completions(args) => {
            let shell: clap_complete::Shell = args.shell.into();
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "tar-dedup", &mut std::io::stdout());
            Ok(())
        }
    }
}

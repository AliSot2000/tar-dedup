//! `check` — verify an archive's footer integrity.

use crate::archive_footer::{self, IntegrityVerdict};
use crate::cli::CheckArgs;
use crate::config::resolve_path_to_abs_path;
use crate::error::Result;
use crate::progress::ProgressBar;
use crate::shutdown::Shutdown;

fn exit_code(verdict: IntegrityVerdict) -> i32 {
    match verdict {
        IntegrityVerdict::Ok => 0,
        IntegrityVerdict::NoFooter => 2,
        IntegrityVerdict::InvalidFooter => 3,
        IntegrityVerdict::ContentMismatch => 4,
        IntegrityVerdict::DatabaseMismatch => 5,
        IntegrityVerdict::BothMismatch => 6,
    }
}

/// Check the footer of `args.archive`; prints a verdict (unless `--quiet`) and
/// exits with the mapped code. Filesystem failures propagate as `Err` (exit 1).
pub fn run(args: &CheckArgs, shutdown: &Shutdown) -> Result<()> {
    let cwd = std::env::current_dir().map_err(crate::error::Error::from)?;
    let archive = resolve_path_to_abs_path(&args.archive, &cwd);

    let bar = if args.quiet {
        None
    } else {
        let pb = ProgressBar::new(0);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("{msg} [{bar:40}] {percent:>3}%")
                .expect("valid progress template")
                .progress_chars("=>-"),
        );
        Some(pb)
    };

    let verdict = archive_footer::verify_footer(&archive, shutdown, bar.as_ref())?;

    if let Some(pb) = bar.as_ref() {
        pb.finish_and_clear();
    }
    if !args.quiet {
        let msg = match verdict {
            IntegrityVerdict::Ok => "ok: footer content and database hashes match".to_string(),
            IntegrityVerdict::NoFooter => "no footer".to_string(),
            IntegrityVerdict::InvalidFooter => "invalid footer format".to_string(),
            IntegrityVerdict::ContentMismatch => "hash mismatch of content".to_string(),
            IntegrityVerdict::DatabaseMismatch => "hash mismatch of database".to_string(),
            IntegrityVerdict::BothMismatch => "hash mismatch of content and database".to_string(),
        };
        println!("{}: {}", archive.display(), msg);
    }
    std::process::exit(exit_code(verdict));
}
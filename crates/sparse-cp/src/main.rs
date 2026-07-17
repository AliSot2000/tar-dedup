use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use sparse_cp::{
    sparse_copy_with_progress, sparse_page_count, sparse_page_count_with_progress, SparseCopyStats,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Verbosity {
    Quiet,
    Plain,
    Verbose,
    Pretty,
}

#[derive(Debug, Parser)]
#[command(
    name = "sparse-cp",
    about = "Sparse-aware file copy, or scan an input for all-zero blocks"
)]
struct Args {
    /// Input file path.
    input: PathBuf,

    /// Output file path (required unless `--list-only`).
    #[arg(required_unless_present = "list_only")]
    output: Option<PathBuf>,

    /// How much status to print.
    #[arg(long, value_enum, default_value_t = Verbosity::Plain)]
    verbosity: Verbosity,

    /// Block size in bytes used for zero detection / sparse seeks.
    #[arg(long, default_value_t = 4096)]
    block_size: u32,

    /// Only scan the input; print how many full zero blocks were found.
    #[arg(long)]
    list_only: bool,
}

fn main() {
    if let Err(e) = try_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let args = Args::parse();
    if args.block_size == 0 {
        bail!("--block-size must be > 0");
    }
    let block_size = args.block_size as usize;

    if args.list_only {
        run_list_only(&args, block_size)
    } else {
        let output = args
            .output
            .as_ref()
            .expect("clap requires output unless --list-only");
        run_copy(&args, output, block_size)
    }
}

fn run_list_only(args: &Args, block_size: usize) -> Result<()> {
    let input = &args.input;
    let size_in = std::fs::metadata(input)
        .with_context(|| format!("stat {}", input.display()))?
        .len();

    let count = match args.verbosity {
        Verbosity::Quiet => sparse_page_count(input, block_size)?,
        Verbosity::Plain => {
            // list-only plain: just the final line (capturable)
            sparse_page_count(input, block_size)?
        }
        Verbosity::Verbose => {
            let mut last_reported = 0u64;
            sparse_page_count_with_progress(input, block_size, &mut |read, total, elapsed| {
                if should_emit_verbose(read, total, last_reported) || read == total {
                    println!(
                        "read {} of {}, took {}, eta {}",
                        fmt_bytes(read),
                        fmt_bytes(total),
                        fmt_duration(elapsed),
                        fmt_eta(read, total, elapsed)
                    );
                    let _ = io::stdout().flush();
                    last_reported = read;
                }
            })?
        }
        Verbosity::Pretty => {
            let pb = progress_bar(size_in);
            let count = sparse_page_count_with_progress(input, block_size, &mut |read, _, _| {
                pb.set_position(read);
            })?;
            pb.finish_and_clear();
            count
        }
    };
    println!("Found {count} blocks of {block_size} of zeros.");
    Ok(())
}

fn run_copy(args: &Args, output: &PathBuf, block_size: usize) -> Result<()> {
    let input = &args.input;
    let size_in = std::fs::metadata(input)
        .with_context(|| format!("stat {}", input.display()))?
        .len();

    match args.verbosity {
        Verbosity::Quiet => {
            sparse_copy_with_progress(input, output, block_size, &mut |_, _, _| {})
                .with_context(|| {
                    format!("sparse copy {} → {}", input.display(), output.display())
                })?;
        }
        Verbosity::Plain => {
            println!(
                "Copy File {} to File {}",
                input.display(),
                output.display()
            );
            println!("Size in {}", fmt_bytes(size_in));
            println!("Copying...");
            let stats = sparse_copy_with_progress(input, output, block_size, &mut |_, _, _| {})
                .with_context(|| {
                    format!("sparse copy {} → {}", input.display(), output.display())
                })?;
            print_copy_footer(&stats);
        }
        Verbosity::Verbose => {
            println!(
                "Copy File {} to File {}",
                input.display(),
                output.display()
            );
            println!("Size in {}", fmt_bytes(size_in));
            println!("Copying...");
            let mut last_reported = 0u64;
            let stats = sparse_copy_with_progress(
                input,
                output,
                block_size,
                &mut |read, total, elapsed| {
                    if should_emit_verbose(read, total, last_reported) || read == total {
                        println!(
                            "read {} of {}, took {}, eta {}",
                            fmt_bytes(read),
                            fmt_bytes(total),
                            fmt_duration(elapsed),
                            fmt_eta(read, total, elapsed)
                        );
                        let _ = io::stdout().flush();
                        last_reported = read;
                    }
                },
            )
            .with_context(|| format!("sparse copy {} → {}", input.display(), output.display()))?;
            print_copy_footer(&stats);
        }
        Verbosity::Pretty => {
            println!(
                "Copy File {} to File {}",
                input.display(),
                output.display()
            );
            println!("Size in {}", fmt_bytes(size_in));
            println!("Copying...");
            let pb = progress_bar(size_in);
            let stats = sparse_copy_with_progress(input, output, block_size, &mut |read, _, _| {
                pb.set_position(read);
            })
            .with_context(|| format!("sparse copy {} → {}", input.display(), output.display()))?;
            pb.finish_and_clear();
            print_copy_footer(&stats);
        }
    }

    Ok(())
}

fn print_copy_footer(stats: &SparseCopyStats) {
    println!("Done!");
    println!(
        "Size out {} (blocks of 0 dropped from out)",
        fmt_bytes(stats.size_out)
    );
    println!("Saved {}", fmt_bytes(stats.bytes_saved));
}

fn progress_bar(len: u64) -> ProgressBar {
    let pb = ProgressBar::new(len.max(1));
    pb.set_style(
        ProgressStyle::with_template(
            "[{bar:40.cyan/blue}] {percent:>3}% ({bytes}/{total_bytes}) elapsed {elapsed} eta {eta}",
        )
        .expect("progress template")
        .progress_chars("=>-"),
    );
    if len == 0 {
        pb.finish_and_clear();
    }
    pb
}

/// Emit a verbose progress line at start, roughly every 16 MiB, and at completion.
fn should_emit_verbose(read: u64, total: u64, last_reported: u64) -> bool {
    const STEP: u64 = 16 * 1024 * 1024;
    if read == 0 {
        return true;
    }
    if total > 0 && read == total {
        return true;
    }
    read.saturating_sub(last_reported) >= STEP
}

fn fmt_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let n = n as f64;
    if n >= GIB {
        format!("{:.2}GiB", n / GIB)
    } else if n >= MIB {
        format!("{:.2}MiB", n / MIB)
    } else if n >= KIB {
        format!("{:.2}KiB", n / KIB)
    } else {
        format!("{n}B")
    }
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}min{s:02}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h}h{m:02}min{s:02}s")
    }
}

fn fmt_eta(read: u64, total: u64, elapsed: Duration) -> String {
    if read == 0 || total == 0 || read >= total {
        return "?".to_string();
    }
    let rate = read as f64 / elapsed.as_secs_f64().max(1e-9);
    if rate <= 0.0 {
        return "?".to_string();
    }
    let remain = (total - read) as f64 / rate;
    fmt_duration(Duration::from_secs_f64(remain))
}

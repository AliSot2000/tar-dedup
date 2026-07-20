//! Build dense synthetic test files for `sparse-cp`.
//!
//! ```text
//! cargo run -p sparse-cp --example mkfile -- \
//!   --file-size 1Gib --non-zero 2144 --block-size 4096 -f /tmp/test.bin
//!
//! # Obfuscated zeros: fill with random, then punch 0x00 into empty blocks
//! # one (or N) byte column at a time across all empty blocks:
//! cargo run -p sparse-cp --example mkfile -- \
//!   --file-size 16Kib --non-zero 1 -o -f /tmp/test.bin
//! cargo run -p sparse-cp --example mkfile -- \
//!   --file-size 16Kib --non-zero 1 -o 4 -f /tmp/test.bin
//! ```
//!
//! Bare `-o` uses `N = block_size / 2`. Omit `-o` for a plain dense write.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Parser};
use indicatif::{ProgressBar, ProgressStyle};

/// Presence of `-o` / `--obfuscate`: omitted, bare flag, or explicit `N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObfuscateArg {
    /// `-o` with no value → resolve to `block_size / 2` after parse.
    Auto,
    /// `-o N` / `--obfuscate N`.
    Explicit(u32),
}

fn parse_obfuscate_arg(raw: &str) -> Result<ObfuscateArg, String> {
    if raw.eq_ignore_ascii_case("auto") {
        return Ok(ObfuscateArg::Auto);
    }
    let n: u32 = raw
        .parse()
        .map_err(|_| format!("invalid --obfuscate value `{raw}` (expected u32 or `auto`)"))?;
    Ok(ObfuscateArg::Explicit(n))
}

#[derive(Debug, Parser)]
#[command(
    name = "mkfile",
    about = "Create a dense test file with a chosen number of non-zero blocks"
)]
struct Args {
    /// Destination path.
    #[arg(short = 'f', long = "output", value_name = "PATH")]
    output: PathBuf,

    /// Logical file size (bytes). Accepts SI (Kb/Mb/Gb/Tb) and IEC (Kib/Mib/Gib/Tib) suffixes.
    #[arg(long, value_name = "SIZE", value_parser = parse_file_size)]
    file_size: u64,

    /// Block size in bytes (placement grid for `--non-zero`).
    #[arg(long, default_value_t = 4096)]
    block_size: u32,

    /// Number of full blocks filled with non-zero data (all other bytes are written as 0x00).
    /// May be 0; must not exceed `file_size / block_size`.
    #[arg(long, default_value_t = 0)]
    non_zero: u64,

    /// Fill with random data, then zero empty blocks in strided passes.
    ///
    /// - omitted → no obfuscation (`None`)
    /// - `-o` / `--obfuscate` → `N = block_size / 2`
    /// - `-o N` / `--obfuscate N` → use explicit `N` (`0 < N ≤ block_size`)
    #[arg(
        short = 'o',
        long,
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "auto",
        value_parser = parse_obfuscate_arg,
        action = ArgAction::Set
    )]
    obfuscate: Option<ObfuscateArg>,
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
    let total_blocks = (args.file_size / block_size as u64) as usize;
    let remainder = (args.file_size % block_size as u64) as usize;
    if args.non_zero > total_blocks as u64 {
        bail!(
            "--non-zero ({}) exceeds total full blocks ({}) for file_size={} block_size={}",
            args.non_zero,
            total_blocks,
            args.file_size,
            block_size
        );
    }

    // Resolve optional obfuscate: None | Auto→block_size/2 | Explicit(n).
    let obfuscate: Option<usize> = match args.obfuscate {
        None => None,
        Some(ObfuscateArg::Auto) => {
            let n = block_size / 2;
            if n == 0 {
                bail!(
                    "bare --obfuscate needs block_size/2 > 0 (block_size={block_size})"
                );
            }
            Some(n)
        }
        Some(ObfuscateArg::Explicit(0)) => bail!("--obfuscate N must be > 0"),
        Some(ObfuscateArg::Explicit(n)) if n as usize > block_size => bail!(
            "--obfuscate N ({n}) must be <= --block-size ({block_size})"
        ),
        Some(ObfuscateArg::Explicit(n)) => Some(n as usize),
    };

    let mut seed = 0x5EED_u64 ^ args.file_size ^ block_size as u64 ^ args.non_zero;
    let dirty: HashSet<usize> = if args.non_zero > 0 {
        pick_unique_indices(args.non_zero as usize, total_blocks, &mut seed)
            .into_iter()
            .collect()
    } else {
        HashSet::new()
    };

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&args.output)
        .with_context(|| format!("create {}", args.output.display()))?;

    match obfuscate {
        None => {
            println!(
                "Writing dense file {} ({} B, {} full blocks, {} non-zero @ block_size {})",
                args.output.display(),
                args.file_size,
                total_blocks,
                args.non_zero,
                block_size
            );
            let pb = progress_bar(args.file_size);
            pb.set_message("dense write");
            write_dense_direct(
                &mut file,
                block_size,
                total_blocks,
                remainder,
                &dirty,
                &mut seed,
                &pb,
            )?;
            pb.finish_with_message("done");
            println!("Done.");
        }
        Some(stride) => {
            let phase2_passes = block_size.div_ceil(stride);
            let work_total = args.file_size.saturating_mul((phase2_passes as u64) + 1);
            let auto_note = matches!(args.obfuscate, Some(ObfuscateArg::Auto));
            println!(
                "Writing obfuscated dense file {} ({} B, {} full blocks, {} non-zero @ block_size {}, obfuscate={stride}{})",
                args.output.display(),
                args.file_size,
                total_blocks,
                args.non_zero,
                block_size,
                if auto_note {
                    " [= block_size/2]"
                } else {
                    ""
                }
            );
            println!("Phase 1/2: fill entire file with random data");
            println!(
                "Phase 2/2: zero empty blocks in {phase2_passes} pass(es) \
                 (ceil(block_size={block_size} / obfuscate={stride}))"
            );
            println!(
                "Progress total: {work_total} B-equivalent \
                 (file_size × (ceil(block_size/obfuscate) + 1))"
            );
            let pb = progress_bar(work_total);
            write_dense_obfuscated(
                &mut file,
                block_size,
                total_blocks,
                remainder,
                &dirty,
                stride,
                &mut seed,
                &pb,
            )?;
            pb.finish_with_message("done");
            println!("Done.");
        }
    }

    file.sync_all()?;
    Ok(())
}

/// Sequential dense write: zero pages or non-zero pages as needed.
fn write_dense_direct(
    file: &mut std::fs::File,
    block_size: usize,
    total_blocks: usize,
    remainder: usize,
    dirty: &HashSet<usize>,
    seed: &mut u64,
    pb: &ProgressBar,
) -> Result<()> {
    let zero_page = vec![0u8; block_size];
    let mut dirty_page = vec![0u8; block_size];
    let mut written = 0u64;
    for idx in 0..total_blocks {
        if dirty.contains(&idx) {
            fill_random(&mut dirty_page, seed);
            file.write_all(&dirty_page)?;
        } else {
            file.write_all(&zero_page)?;
        }
        written += block_size as u64;
        pb.set_position(written);
    }
    if remainder > 0 {
        file.write_all(&zero_page[..remainder])?;
        written += remainder as u64;
        pb.set_position(written);
    }
    Ok(())
}

/// 1) Fill entire file with random bytes.
/// 2) Zero empty blocks (and the tail) in strided passes of `stride` consecutive bytes.
///
/// Progress units are “byte-equivalents”: `file_size * (ceil(block_size/stride) + 1)`.
fn write_dense_obfuscated(
    file: &mut std::fs::File,
    block_size: usize,
    total_blocks: usize,
    remainder: usize,
    dirty: &HashSet<usize>,
    stride: usize,
    seed: &mut u64,
    pb: &ProgressBar,
) -> Result<()> {
    let file_size = total_blocks as u64 * block_size as u64 + remainder as u64;
    let phase2_passes = block_size.div_ceil(stride);

    // Phase 1: dense random fill → progress 0..file_size
    pb.set_message("phase 1/2: random fill");
    let mut chunk = vec![0u8; block_size.max(1)];
    let mut written = 0u64;
    while written < file_size {
        let n = ((file_size - written) as usize).min(chunk.len());
        fill_random(&mut chunk[..n], seed);
        file.write_all(&chunk[..n])?;
        written += n as u64;
        pb.set_position(written);
    }

    let empty: Vec<usize> = (0..total_blocks).filter(|i| !dirty.contains(i)).collect();
    let zeros = vec![0u8; stride];
    let empty_n = empty.len().max(1) as u64;

    // Phase 2: pass `p` maps onto progress file_size*(1+p) .. file_size*(2+p)
    for (pass, b) in (0..block_size).step_by(stride).enumerate() {
        let len = stride.min(block_size - b);
        let pass_base = file_size.saturating_mul(1 + pass as u64);
        pb.set_message(format!(
            "phase 2/2: pass {}/{phase2_passes}",
            pass + 1
        ));
        if empty.is_empty() {
            pb.set_position(pass_base + file_size);
            continue;
        }
        for (i, &n) in empty.iter().enumerate() {
            let offset = n as u64 * block_size as u64 + b as u64;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&zeros[..len])?;
            let within = file_size.saturating_mul((i as u64) + 1) / empty_n;
            pb.set_position(pass_base + within);
        }
    }

    // Tail: small; snap to the planned total after main passes.
    if remainder > 0 {
        pb.set_message("phase 2/2: zeroing tail");
        let base = total_blocks as u64 * block_size as u64;
        for b in (0..remainder).step_by(stride) {
            let len = stride.min(remainder - b);
            file.seek(SeekFrom::Start(base + b as u64))?;
            file.write_all(&zeros[..len])?;
        }
    }

    pb.set_position(file_size.saturating_mul((phase2_passes as u64) + 1));
    pb.set_message("phase 2/2: complete");
    Ok(())
}

fn progress_bar(total: u64) -> ProgressBar {
    let pb = ProgressBar::new(total.max(1));
    pb.set_style(
        ProgressStyle::with_template(
            "[{bar:40.cyan/blue}] {percent:>3}% ({bytes}/{total_bytes}) {msg} elapsed {elapsed} eta {eta}",
        )
        .expect("progress template")
        .progress_chars("=>-"),
    );
    pb
}

/// Parse sizes like `4096`, `1Kb`, `2.5Mib`, `1Gib` (SI base-1000 or IEC base-1024).
fn parse_file_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }

    let split = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num_str, unit_str) = s.split_at(split);
    let value: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in size `{raw}`"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid number in size `{raw}`"));
    }

    let mult = match unit_str.trim() {
        "" | "B" | "b" => 1.0,
        "Kb" | "kb" | "KB" => 1_000.0,
        "Mb" | "mb" | "MB" => 1_000_000.0,
        "Gb" | "gb" | "GB" => 1_000_000_000.0,
        "Tb" | "tb" | "TB" => 1_000_000_000_000.0,
        "Kib" | "kib" | "KiB" | "Ki" => 1024.0,
        "Mib" | "mib" | "MiB" | "Mi" => 1024.0 * 1024.0,
        "Gib" | "gib" | "GiB" | "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Tib" | "tib" | "TiB" | "Ti" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(format!(
                "unknown size unit `{other}` (use Kb/Mb/Gb/Tb or Kib/Mib/Gib/Tib)"
            ))
        }
    };

    let bytes = value * mult;
    if bytes > u64::MAX as f64 {
        return Err(format!("size `{raw}` is too large"));
    }
    Ok(bytes.round() as u64)
}

fn fill_random(buf: &mut [u8], seed: &mut u64) {
    for b in buf {
        *b = next_u64(seed) as u8;
    }
}

fn next_u64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn pick_unique_indices(count: usize, range: usize, seed: &mut u64) -> Vec<usize> {
    assert!(count <= range);
    let mut pool: Vec<usize> = (0..range).collect();
    for i in 0..count {
        let j = i + (next_u64(seed) as usize % (range - i));
        pool.swap(i, j);
    }
    pool.truncate(count);
    pool
}

#[cfg(test)]
mod parse_tests {
    use super::parse_file_size;

    #[test]
    fn parses_si_and_iec() {
        assert_eq!(parse_file_size("4096").unwrap(), 4096);
        assert_eq!(parse_file_size("1Kb").unwrap(), 1000);
        assert_eq!(parse_file_size("1Kib").unwrap(), 1024);
        assert_eq!(parse_file_size("1Mb").unwrap(), 1_000_000);
        assert_eq!(parse_file_size("1Mib").unwrap(), 1024 * 1024);
        assert_eq!(parse_file_size("1Gib").unwrap(), 1024u64.pow(3));
        assert_eq!(parse_file_size("2.5Kib").unwrap(), 2560);
    }
}

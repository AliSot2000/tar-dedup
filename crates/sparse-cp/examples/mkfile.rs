//! Build dense synthetic test files for `sparse-cp`.
//!
//! ```text
//! cargo run -p sparse-cp --example mkfile -- \
//!   --file-size 1Gib --non-zero 2144 --block-size 4096 /tmp/test.bin
//! ```
//!
//! This is a **separate executable** from `sparse-cp` (Cargo `examples/` target).
//! Prefer `src/bin/` + `[[bin]]` only if you want it installed next to `sparse-cp`.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "mkfile",
    about = "Create a dense test file with a chosen number of non-zero blocks"
)]
struct Args {
    /// Destination path.
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
    let block_size = args.block_size as u64;
    let total_blocks = args.file_size / block_size;
    let remainder = (args.file_size % block_size) as usize;
    if args.non_zero > total_blocks {
        bail!(
            "--non-zero ({}) exceeds total full blocks ({}) for file_size={} block_size={}",
            args.non_zero,
            total_blocks,
            args.file_size,
            block_size
        );
    }

    let mut seed = 0x5EED_u64 ^ args.file_size ^ block_size ^ args.non_zero;
    let dirty: HashSet<usize> = if args.non_zero > 0 {
        pick_unique_indices(args.non_zero as usize, total_blocks as usize, &mut seed)
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

    // Dense write: every byte is written (zeros or non-zeros). No set_len/seek holes.
    let zero_page = vec![0u8; block_size as usize];
    let mut dirty_page = vec![0u8; block_size as usize];
    for idx in 0..total_blocks as usize {
        if dirty.contains(&idx) {
            fill_nonzero(&mut dirty_page, &mut seed);
            file.write_all(&dirty_page)?;
        } else {
            file.write_all(&zero_page)?;
        }
    }
    if remainder > 0 {
        file.write_all(&zero_page[..remainder])?;
    }

    file.sync_all()?;
    println!(
        "wrote dense {} ({} B, {} full blocks, {} non-zero @ block_size {})",
        args.output.display(),
        args.file_size,
        total_blocks,
        args.non_zero,
        block_size
    );
    Ok(())
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

fn fill_nonzero(buf: &mut [u8], seed: &mut u64) {
    for b in buf {
        *b = (next_u64(seed) % 255) as u8 + 1;
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

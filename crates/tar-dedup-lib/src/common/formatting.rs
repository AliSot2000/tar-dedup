//! Shared number/byte/duration formatting helpers.

use std::time::Duration;

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;
const GIB: f64 = 1024.0 * MIB;

/// Render a byte count with IEC binary units, e.g. `3.06MiB`.
pub fn format_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= GIB {
        format!("{:.2}GiB", n / GIB)
    } else if n >= MIB {
        format!("{:.2}MiB", n / MIB)
    } else if n >= KIB {
        format!("{:.1}KiB", n / KIB)
    } else {
        format!("{n:.0}B")
    }
}

/// Render a bytes/second rate with IEC binary units, e.g. `3.06MiB/s`.
pub fn format_bytes_per_sec(bps: f64) -> String {
    format!("{}/s", format_bytes(bps as u64))
}

/// Render a byte count **rounded up to whole MiB** with thousands grouped by an
/// apostrophe, e.g. `1'048'576 MiB` (xz memory-limit style). Unlike
/// [`format_bytes`], this keeps a fixed `MiB` unit and rounds up.
pub fn format_mib(bytes: u64) -> String {
    let mib = (bytes + (1024 * 1024 - 1)) / (1024 * 1024);
    // Thin space grouping like xz (ASCII apostrophe fallback).
    let s = mib.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('\'');
        }
        out.push(ch);
    }
    format!("{} MiB", out.chars().rev().collect::<String>())
}

/// Render an elapsed duration compactly: `MM:SS`, or `H:MM:SS` past an hour.
pub fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.00MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00GiB");
    }

    #[test]
    fn formats_bytes_per_sec() {
        assert_eq!(format_bytes_per_sec(0.0), "0B/s");
        assert_eq!(format_bytes_per_sec(512.0), "512B/s");
        assert_eq!(format_bytes_per_sec(2048.0), "2.0KiB/s");
        assert_eq!(format_bytes_per_sec(3.0 * 1024.0 * 1024.0), "3.00MiB/s");
        assert_eq!(format_bytes_per_sec(2.0 * 1024.0 * 1024.0 * 1024.0), "2.00GiB/s");
    }

    #[test]
    fn formats_elapsed() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0:00");
        assert_eq!(format_elapsed(Duration::from_secs(25 * 60 + 12)), "25:12");
        assert_eq!(
            format_elapsed(Duration::from_secs(1 * 3600 + 5 * 60 + 7)),
            "1:05:07"
        );
    }

    #[test]
    fn format_mib_rounds_up_to_next_mib() {
        assert_eq!(format_mib(0), "0 MiB");
        assert_eq!(format_mib(1), "1 MiB");
        assert_eq!(format_mib(1024 * 1024), "1 MiB");
        assert_eq!(format_mib(1024 * 1024 + 1), "2 MiB");
        assert_eq!(format_mib(1024 * 1024 - 1024), "1 MiB");
    }

    #[test]
    fn format_mib_groups_thousands_with_apostrophe() {
        assert_eq!(format_mib(1_048_576u64 * 1024 * 1024), "1'048'576 MiB");
        assert_eq!(format_mib(123_456_789u64 * 1024 * 1024), "123'456'789 MiB");
        assert_eq!(format_mib(999 * 1024 * 1024), "999 MiB");
    }
}
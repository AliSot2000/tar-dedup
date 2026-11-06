//! Logging-axis resolution from the `--log-format` preset and the per-axis
//! `--log-short-time`/`--log-long-time` / `--log-no-target`/`--log-target`
//! overrides. Kept out of `cli.rs`: this is configuration logic, and it shares
//! the `--apply-all-metadata`/`--no-*` precedence with the capture bits.

use crate::cli::LogFormat;

use super::common::resolve_bool_flag;

/// Resolve the two logging axes from the preset + per-axis overrides, mirroring
/// `--apply-all-metadata`/`--no-*` (`--no-*` wins, then `--yes`, else base):
/// returns `(long_time, target_visible)`.
pub fn resolve_log_flags(
    format: LogFormat,
    short_time: bool,
    long_time: bool,
    no_target: bool,
    target: bool)
    -> (bool, bool) {
    let base = format == LogFormat::Verbose;
    (
        resolve_bool_flag(short_time, long_time, base),
        resolve_bool_flag(no_target, target, base),
    )
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::LogFormat::{Short, Verbose};

    #[test]
    fn resolve_log_flags_mirror_apply_all_metadata() {
        // Base comes from the preset; `--no-*` wins, then `--yes`.
        assert_eq!(resolve_log_flags(Short, false, false, false, false), (false, false));
        assert_eq!(resolve_log_flags(Verbose, false, false, false, false), (true, true));
        // `--log-short-time` overrides a verbose time axis.
        assert_eq!(resolve_log_flags(Verbose, true, false, false, false), (false, true));
        // `--log-target` overrides a short target axis.
        assert_eq!(resolve_log_flags(Short, false, false, false, true), (false, true));
    }
}
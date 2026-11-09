//! Cargo entry point for the shell integration suite (`integration/run.sh`).
//!
//! Each scenario is its own test named `scenario_<name>` so both
//! `cargo test` and `cargo llvm-cov` can select one scenario at a time:
//!
//! ```text
//! cargo test  -p tar-dedup --test integration_shell
//! cargo llvm-cov -p tar-dedup -p tar-dedup-lib --test integration_shell --summary-only
//! cargo llvm-cov -p tar-dedup -p tar-dedup-lib --test integration_shell scenario_xattr_user --summary-only
//! ```
//!
//! The scenario runner is invoked through `bash` with `TAR_DEDUP_BIN` pointing
//! at the freshly built binary, so (under `llvm-cov`) every CLI *child process*
//! writes profile data that merges back into the report — the only way to cover
//! the `cmd/*` subcommands and the real archive/extract pipelines end to end.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Repo root (two levels up from `crates/tar-dedup`).
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("crates dir")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn run_scenario(name: &str) {
    let script = repo_root().join("integration/run.sh");
    let bin = std::env::var("CARGO_BIN_EXE_tar-dedup").expect("cargo builds the bin for tests");
    let out: Output = Command::new("bash")
        .arg(&script)
        .arg(name)
        .env("TAR_DEDUP_BIN", bin)
        .current_dir(repo_root())
        .output()
        .expect("spawn bash integration/run.sh");

    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Cap the (usually CLI-shaped) log to the interesting tail.
        let clip = |s: &str| {
            let n = s.chars().count();
            if n > 2000 {
                let mut t: String = s.chars().skip(n - 2000).collect();
                t.insert_str(0, "… (truncated)\n");
                t
            } else {
                s.to_string()
            }
        };
        panic!(
            "integration scenario `{name}` failed (exit {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            clip(&stdout),
            clip(&stderr),
        );
    }
}

#[test]
fn scenario_dedup_roundtrip() { run_scenario("dedup_roundtrip") }
#[test]
fn scenario_bypass_archive_roundtrip() { run_scenario("bypass_archive_roundtrip") }
#[test]
fn scenario_path_clamp() { run_scenario("path_clamp") }
#[test]
fn scenario_resume_archive() { run_scenario("resume_archive") }
#[test]
fn scenario_xattr_apply_all() { run_scenario("xattr_apply_all") }
#[test]
fn scenario_xattr_binary() { run_scenario("xattr_binary") }
#[test]
fn scenario_xattr_none_default() { run_scenario("xattr_none_default") }
#[test]
fn scenario_xattr_on_symlink() { run_scenario("xattr_on_symlink") }
#[test]
fn scenario_xattr_user() { run_scenario("xattr_user") }
#[test]
fn scenario_cmd_inspect() { run_scenario("cmd_inspect") }
#[test]
fn scenario_cmd_stub_not_implemented() { run_scenario("cmd_stub_not_implemented") }
#[test]
fn scenario_paths_relative_input() { run_scenario("paths_relative_input") }
#[test]
fn scenario_paths_strip_transform_order() { run_scenario("paths_strip_transform_order") }
#[test]
fn scenario_paths_empty_name_skip() { run_scenario("paths_empty_name_skip") }
#[test]
fn scenario_paths_absolute_root() { run_scenario("paths_absolute_root") }
#[test]
fn scenario_path_collision_transform() { run_scenario("path_collision_transform") }
#[test]
fn scenario_no_strict_separation_fanout() { run_scenario("no_strict_separation_fanout") }
#[test]
fn scenario_link_tree() { run_scenario("link_tree") }
#[test]
fn scenario_hardlink_grouping_none() { run_scenario("hardlink_grouping_none") }
#[test]
fn scenario_hardlink_global_vs_source() { run_scenario("hardlink_global_vs_source") }
#[test]
fn scenario_hardlink_overlap_matrix() { run_scenario("hardlink_overlap_matrix") }
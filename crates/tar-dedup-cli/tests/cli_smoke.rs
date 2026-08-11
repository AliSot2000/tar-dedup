use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_exits_successfully() {
    Command::cargo_bin("tar-dedup")
        .expect("binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("tar-dedup"));
}

#[test]
fn extract_help_exits_successfully() {
    Command::cargo_bin("tar-dedup")
        .expect("binary")
        .args(["extract", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("-C"));
}

#[test]
fn archive_help_lists_new_path_and_sparse_flags() {
    let assert = Command::cargo_bin("tar-dedup")
        .expect("binary")
        .args(["archive", "--help"])
        .assert()
        .success();

    let out = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(out.contains("--input-dir"), "expected --input-dir in help");
    assert!(out.contains("--work-dir"), "expected --work-dir in help");
    assert!(out.contains("--sparsify"), "expected --sparsify in help");
    assert!(
        out.contains("working directory when resolving relative paths")
            || out.contains("--directory"),
        "expected -C/--directory path-resolution help"
    );
    assert!(!out.contains("--resume"), "archive must not expose --resume");
    assert!(
        !out.contains("--force-reset-to-phase"),
        "archive must not expose --force-reset-to-phase"
    );
}

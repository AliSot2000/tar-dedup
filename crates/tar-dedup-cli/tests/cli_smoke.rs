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
    for heading in [
        "Archive Paths",
        "Inputs",
        "Compression",
        "Indexing",
        "Filtering",
        "File Attributes",
        "Sparse Files",
        "Process Options",
    ] {
        assert!(
            out.contains(heading),
            "expected README help heading {heading:?} in archive --help"
        );
    }
    assert!(!out.contains("--resume"), "archive must not expose --resume");
    assert!(
        !out.contains("--force-reset-to-phase"),
        "archive must not expose --force-reset-to-phase"
    );
}

#[test]
fn resume_help_lists_work_dir_jobs_and_exit_after_stage() {
    let assert = Command::cargo_bin("tar-dedup")
        .expect("binary")
        .args(["resume", "--help"])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(out.contains("--work-dir"), "expected --work-dir in resume help");
    assert!(out.contains("--jobs"), "expected --jobs in resume help");
    assert!(
        out.contains("--exit-after-stage"),
        "expected --exit-after-stage in resume help"
    );
    assert!(!out.contains("--fresh"), "resume must not expose --fresh");
    assert!(!out.contains("--input-dir"), "resume must not expose archive inputs");
}

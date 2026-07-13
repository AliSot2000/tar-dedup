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

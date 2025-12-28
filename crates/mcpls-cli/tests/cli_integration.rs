//! Integration tests for the mcpls CLI binary.

#![allow(clippy::unwrap_used)]
#![allow(deprecated)]

use std::fs;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn test_help_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_help_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_invalid_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--invalid-flag")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn test_config_file_not_found() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_with_invalid_toml() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("invalid.toml");

    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--config")
        .arg(&config_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("-c")
        .arg("/nonexistent/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_with_empty_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("empty.toml");

    fs::write(&config_path, "").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--config").arg(&config_path).assert().failure();
}

#[test]
fn test_config_file_with_spaces_in_path() {
    let temp_dir = TempDir::new().unwrap();
    let subdir = temp_dir.path().join("path with spaces");
    fs::create_dir(&subdir).unwrap();
    let config_path = subdir.join("config.toml");

    fs::write(&config_path, "invalid content").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    cmd.arg("--config").arg(&config_path).assert().failure();
}

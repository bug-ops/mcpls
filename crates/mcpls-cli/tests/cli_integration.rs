//! Integration tests for the mcpls CLI binary.

#![allow(clippy::unwrap_used)]
#![allow(deprecated)]

use std::fs;
use std::process::Command;
use std::time::Duration;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// Vars that could leak in from the ambient environment (e.g. a developer's
/// shell, or a repo `.envrc`) and change these tests' outcome: `MCPLS_LOG`
/// could suppress a warning a test greps for, `MCPLS_CONFIG` could redirect
/// config loading away from the CWD/`--config` path under test,
/// `MCPLS_TRUST_PROJECT_CONFIG` could flip the trust decision a test is
/// specifically exercising, and `MCPLS_LOG_JSON` could flip the log output
/// format a test asserts on. `assert_cmd::Command`/`std::process::Command`
/// inherit the parent's full environment by default, so every test must
/// clear these before setting the ones it actually wants.
fn clear_ambient_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
}

#[test]
fn test_help_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_help_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_invalid_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--invalid-flag")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn test_config_file_not_found() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
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

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-c")
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

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure();
}

/// A CWD-discovered `./mcpls.toml` is untrusted by default: it must not be
/// parsed at all, regardless of `--config`/`MCPLS_CONFIG` (which are
/// unaffected by trust and aren't exercised here). We assert this by
/// planting an invalid TOML file and confirming the process does *not* fail
/// with a config-parse error — instead it logs the ignore-warning and
/// proceeds to serve (which blocks on stdio, so it's killed once the
/// timeout elapses; the kill itself is expected, not a test failure).
#[test]
fn test_trust_project_config_env_false_does_not_grant_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG");
    let output = cmd
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "false")
        // Generous bound: the untrusted path is expected to block on stdio
        // (proving it never bailed out on the broken TOML), so this timeout
        // always elapses by design. It only needs to be long enough that a
        // loaded CI runner doesn't get the process killed before mcpls even
        // finishes logging the ignore-warning, which would false-fail this
        // test rather than exercise the actual behavior under test.
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ignoring untrusted project-local config"),
        "expected the untrusted-ignore warning, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("failed to load configuration"),
        "MCPLS_TRUST_PROJECT_CONFIG=false must not grant trust; stderr: {stderr}"
    );
}

/// clap's derived value parser for this flag only accepts the literal
/// `true`/`false` (see `ArgAction::SetTrue`'s `default_value_parser` in
/// `clap_builder`, which uses `ValueParser::bool()`, not a boolish parser
/// accepting `0`/`1`/`yes`/`no`). A malformed value like `0` must be
/// *rejected* outright rather than silently coerced to either trust state —
/// this is the strongest form of "does not grant trust": the process never
/// gets far enough to load anything, trusted or not. We don't pin the exact
/// clap error wording (brittle across clap upgrades); it's enough that the
/// process fails before either trust branch's log line could appear, since
/// argument parsing runs before logging is even initialized.
#[test]
fn test_trust_project_config_env_invalid_value_rejected() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "0")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration").not())
        .stderr(predicate::str::contains("ignoring untrusted project-local config").not());
}

#[test]
fn test_trust_project_config_flag_grants_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .arg("--trust-project-config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration"));
}

#[test]
fn test_trust_project_config_env_true_grants_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "true")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration"));
}

/// `MCPLS_CONFIG` must stay trusted -- and be the file actually consulted --
/// even when a `./mcpls.toml` also exists in the CWD, regardless of the CWD
/// file's own trust state. Runs with `--trust-project-config` set (so the
/// CWD file is itself trusted) specifically to rule out a reordering bug
/// where a trusted CWD file gets checked/loaded before `MCPLS_CONFIG`: if
/// that ordering regressed, the invalid-TOML CWD file would be parsed first
/// and fail with a TOML-parse error, never reaching the `MCPLS_CONFIG`
/// check. Asserting the "configuration file not found" error for the
/// `MCPLS_CONFIG` path (not a TOML-parse error) proves `MCPLS_CONFIG` is
/// still checked first.
#[test]
fn test_mcpls_config_env_wins_over_cwd_file_even_when_trusted() {
    let temp_dir = TempDir::new().unwrap();
    let cwd_config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&cwd_config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .arg("--trust-project-config")
        .env("MCPLS_CONFIG", "/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("configuration file not found"))
        .stderr(predicate::str::contains("TOML parsing error").not());
}

#[test]
fn test_config_file_with_spaces_in_path() {
    let temp_dir = TempDir::new().unwrap();
    let subdir = temp_dir.path().join("path with spaces");
    fs::create_dir(&subdir).unwrap();
    let config_path = subdir.join("config.toml");

    fs::write(&config_path, "invalid content").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure();
}

/// #279: `--log-json` was parsed by clap but never passed to
/// `logging::init`, so the flag had no observable effect. A nonexistent
/// `--config` path is used to force a fast, deterministic failure right
/// after the "starting mcpls" line is logged (see `main.rs`), without
/// needing a timeout+kill for a process that would otherwise block on
/// stdio. `tracing_subscriber`'s JSON formatter always quotes the event's
/// `message` field as `"message":"..."`, which the default compact
/// formatter never produces (it renders unquoted `starting mcpls
/// version=...`), so this substring is a reliable discriminator between the
/// two formats without pulling in `serde_json` just for tests.
#[test]
fn test_log_json_flag_emits_json_formatted_logs() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--log-json")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""));
}

/// Same as `test_log_json_flag_emits_json_formatted_logs`, but via the
/// `MCPLS_LOG_JSON` env var (clap's `env` attribute on `Args::log_json`)
/// instead of the `--log-json` flag, since the two are parsed through
/// separate clap code paths that both need to reach `logging::init`.
#[test]
fn test_log_json_env_var_emits_json_formatted_logs() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_LOG_JSON", "true")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""));
}

/// Complements the two tests above: without `--log-json`/`MCPLS_LOG_JSON`,
/// output must stay in the default compact format. Guards against a
/// regression that flips the default (e.g. an inverted `if log_json`
/// condition), which the JSON-mode tests alone wouldn't catch.
#[test]
fn test_default_logging_is_not_json() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("starting mcpls"))
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
}

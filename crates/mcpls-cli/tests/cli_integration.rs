//! Integration tests for the mcpls CLI binary.

#![allow(deprecated, reason = "`assert_cmd::Command::cargo_bin` is deprecated")]

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

/// `parse_bool_flag` (the custom `value_parser` on this field, see
/// `args.rs`) accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and
/// `on`/`off`, case-insensitively — but a value outside that set must still
/// be *rejected* outright rather than silently coerced to either trust
/// state. This is the strongest form of "does not grant trust": the process
/// never gets far enough to load anything, trusted or not. We don't pin the
/// exact clap error wording (brittle across clap upgrades); it's enough
/// that the process fails before either trust branch's log line could
/// appear, since argument parsing runs before logging is even initialized.
#[test]
fn test_trust_project_config_env_invalid_value_rejected() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "banana")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration").not())
        .stderr(predicate::str::contains("ignoring untrusted project-local config").not());
}

/// Companion to `test_trust_project_config_env_false_does_not_grant_trust`:
/// `0` is one of the numeric spellings `parse_bool_flag` accepts as falsy
/// (issue #295), so it must behave identically to `false` — proceed to the
/// untrusted-config path, not get rejected as an invalid value like
/// `test_trust_project_config_env_invalid_value_rejected` above.
#[test]
fn test_trust_project_config_env_0_does_not_grant_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG");
    let output = cmd
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "0")
        // See test_trust_project_config_env_false_does_not_grant_trust above
        // for why this timeout is expected to always elapse.
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
        "MCPLS_TRUST_PROJECT_CONFIG=0 must not grant trust; stderr: {stderr}"
    );
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
/// two formats without pulling in `serde_json` just for tests. Also asserts
/// on the fatal-error line (`main`'s `tracing::error!` on `run()` failure)
/// to guard the crash path staying JSON too, not just the startup line.
#[test]
fn test_log_json_flag_emits_json_formatted_logs() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--log-json")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""))
        .stderr(predicate::str::contains(
            "\"message\":\"mcpls exited with an error\"",
        ));
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
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""))
        .stderr(predicate::str::contains(
            "\"message\":\"mcpls exited with an error\"",
        ));
}

/// Issue #295: `MCPLS_LOG_JSON` must accept boolean spellings beyond the
/// exact literal `true` already covered above. Exercises the real
/// `env = "MCPLS_LOG_JSON"` + `value_parser = parse_bool_flag` wiring on
/// `Args::log_json` end-to-end (through clap, not just `parse_bool_flag` in
/// isolation) for each truthy spelling, guarding against a regression that
/// drops either attribute from the field.
#[test]
fn test_log_json_env_var_accepts_truthy_conventions() {
    for value in ["1", "TRUE", "yes", "on", "Y"] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        clear_ambient_env(&mut cmd)
            .env("MCPLS_LOG_JSON", value)
            .arg("--config")
            .arg("/nonexistent/path/to/config.toml")
            .assert()
            .failure()
            .stderr(predicate::str::contains("\"message\":\"starting mcpls\""));
    }
}

/// Falsy counterpart of `test_log_json_env_var_accepts_truthy_conventions`:
/// each spelling must be accepted (the process reaches `logging::init`, not
/// a clap parse error) and select the compact non-JSON formatter, same as
/// the unset-env default.
#[test]
fn test_log_json_env_var_accepts_falsy_conventions() {
    for value in ["0", "no", "off", "N"] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        clear_ambient_env(&mut cmd)
            .env("MCPLS_LOG_JSON", value)
            .arg("--config")
            .arg("/nonexistent/path/to/config.toml")
            .assert()
            .failure()
            .stderr(predicate::str::contains("starting mcpls"))
            .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
    }
}

/// A value outside `parse_bool_flag`'s accepted set must still be rejected
/// at argument-parsing time, before `logging::init` (or anything else)
/// runs — mirrors
/// `test_trust_project_config_env_invalid_value_rejected` for the other
/// bool+env field.
#[test]
fn test_log_json_env_var_rejects_invalid_value() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_LOG_JSON", "banana")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
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

#[cfg(feature = "transport-http")]
#[test]
fn test_invalid_http_path_exits_with_usage_error_instead_of_panicking() {
    for bad in ["/", "mcp", ""] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        clear_ambient_env(&mut cmd)
            .env_remove("MCPLS_HTTP_PATH")
            .args(["--listen", "127.0.0.1:0", "--http-path", bad])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("panicked").not());
    }
}

#[cfg(feature = "transport-http")]
#[test]
fn test_invalid_http_path_env_exits_with_usage_error_in_stdio_mode() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_HTTP_PATH", "mcp")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("panicked").not());
}

#[cfg(feature = "transport-http")]
#[test]
fn test_default_port_allowed_host_env_exits_with_usage_error_before_binding() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_HTTP_ALLOWED_HOSTS", "a.example,b.example:443")
        .args(["--listen", "127.0.0.1:0"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("list the host without a port"))
        .stderr(predicate::str::contains("starting mcpls").not());
}

fn rust_config(root: &std::path::Path) -> String {
    format!(
        "[workspace]\nroots = ['{}']\n\n[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"rust-analyzer\"\nargs = []\nfile_patterns = [\"**/*.rs\"]\n",
        root.display()
    )
}

#[test]
fn test_allow_server_without_untrusted_is_a_usage_error() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .args(["--allow-server", "rust"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--workspace-trust untrusted"))
        .stderr(predicate::str::contains("starting mcpls").not());
}

#[test]
fn test_untrusted_with_trust_project_config_is_a_usage_error() {
    let mut flag = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut flag)
        .args(["--workspace-trust", "untrusted", "--trust-project-config"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--trust-project-config"));

    let mut env = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut env)
        .env("MCPLS_TRUST_PROJECT_CONFIG", "1")
        .args(["--workspace-trust", "untrusted"])
        .assert()
        .code(2);
}

#[test]
fn test_untrusted_rejects_a_config_file_inside_the_workspace() {
    let workspace = TempDir::new().unwrap();
    let config_path = workspace.path().join("mcpls.toml");
    fs::write(&config_path, rust_config(workspace.path())).unwrap();

    let mut by_flag = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut by_flag)
        .current_dir(workspace.path())
        .args(["--workspace-trust", "untrusted", "--config"])
        .arg(&config_path)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("lies inside the workspace"));

    let mut by_env = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut by_env)
        .current_dir(workspace.path())
        .env("MCPLS_CONFIG", "./mcpls.toml")
        .args(["--workspace-trust", "untrusted"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("lies inside the workspace"));
}

#[test]
fn test_untrusted_rejects_an_allowed_server_that_is_not_configured() {
    let workspace = TempDir::new().unwrap();
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("mcpls.toml");
    fs::write(&config_path, rust_config(workspace.path())).unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(workspace.path())
        .args([
            "--workspace-trust",
            "untrusted",
            "--allow-server",
            "rsut",
            "--config",
        ])
        .arg(&config_path)
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "'rsut' is not a configured server",
        ));
}

/// A user config reached through `$HOME` / `$XDG_CONFIG_HOME`, both pointed
/// into the workspace, as direnv-style tooling in a checkout can do.
#[cfg(unix)]
#[test]
fn test_untrusted_rejects_an_auto_discovered_config_inside_the_workspace() {
    let workspace = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let marker = elsewhere.path().join("evil-ran");
    let evil = workspace.path().join("evil.sh");
    fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    let config = format!(
        "[workspace]\nroots = ['{root}']\n\n[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"/bin/sh\"\nargs = ['{evil}']\nfile_patterns = [\"**/*.rs\"]\n",
        root = workspace.path().display(),
        evil = evil.display()
    );
    for dir in [
        workspace.path().join("Library/Application Support/mcpls"),
        workspace.path().join("xdg/mcpls"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mcpls.toml"), &config).unwrap();
    }

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
        .current_dir(elsewhere.path())
        .env("HOME", workspace.path())
        .env("XDG_CONFIG_HOME", workspace.path().join("xdg"))
        .args(["--workspace-trust", "untrusted", "--allow-server", "rust"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("lies inside the workspace"));

    assert!(!marker.exists(), "the workspace script ran");
}

#[cfg(unix)]
#[test]
fn test_untrusted_does_not_create_the_default_config() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("xdg"))
        .args(["--workspace-trust", "untrusted"])
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();

    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

/// GUI MCP clients often launch with cwd `/`; that is not a checkout.
#[cfg(unix)]
#[test]
fn test_untrusted_accepts_a_config_outside_the_roots_when_cwd_is_the_filesystem_root() {
    let workspace = TempDir::new().unwrap();
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("mcpls.toml");
    fs::write(&config_path, rust_config(workspace.path())).unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    let output = cmd
        .env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
        .current_dir("/")
        .args(["--workspace-trust", "untrusted", "--config"])
        .arg(&config_path)
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("starting mcpls"), "{stderr}");
    assert!(!stderr.contains("lies inside"), "{stderr}");
}

/// `$HOME` is set by whoever launches mcpls, so a checkout cannot use it to
/// exempt itself as "the home directory": the working directory, defaulted as
/// the only root, is still the workspace.
#[cfg(unix)]
#[test]
fn test_untrusted_does_not_take_a_home_pointing_at_the_workspace_for_the_home_directory() {
    let workspace = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let marker = elsewhere.path().join("evil-ran");
    let config = format!(
        "[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"touch '{}'\"]\nfile_patterns = [\"**/*.rs\"]\n",
        marker.display()
    );
    for dir in [
        workspace.path().join("Library/Application Support/mcpls"),
        workspace.path().join("xdg/mcpls"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mcpls.toml"), &config).unwrap();
    }

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .current_dir(workspace.path())
        .env("HOME", workspace.path())
        .env("XDG_CONFIG_HOME", workspace.path().join("xdg"))
        .args(["--workspace-trust", "untrusted", "--allow-server", "rust"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("lies inside the workspace"));

    assert!(!marker.exists(), "the workspace config ran a command");
}

/// A script outside the workspace whose `#!/usr/bin/env sh` interpreter must
/// not resolve through a `PATH` entry in the workspace, even when `$HOME`
/// names the workspace and the config has no roots.
#[cfg(unix)]
#[test]
fn test_untrusted_strips_workspace_path_entries_when_home_points_at_the_workspace() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Stdio;

    let workspace = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let (started, interpreter) = (
        outside.path().join("started"),
        outside.path().join("interpreter-ran"),
    );
    let script = outside.path().join("server.sh");
    fs::write(
        &script,
        format!(
            "#!/usr/bin/env sh\ntouch '{}'\nexec sleep 30\n",
            started.display()
        ),
    )
    .unwrap();
    fs::create_dir_all(workspace.path().join("bin")).unwrap();
    let fake_sh = workspace.path().join("bin/sh");
    fs::write(
        &fake_sh,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", interpreter.display()),
    )
    .unwrap();
    for file in [&script, &fake_sh] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config_path = outside.path().join("mcpls.toml");
    fs::write(
        &config_path,
        format!(
            "[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"{}\"\nargs = []\nfile_patterns = [\"**/*.rs\"]\n",
            script.display()
        ),
    )
    .unwrap();

    let mut child = Command::cargo_bin("mcpls")
        .unwrap()
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .current_dir(workspace.path())
        .env("HOME", workspace.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", workspace.path().join("bin").display()),
        )
        .args([
            "--workspace-trust",
            "untrusted",
            "--allow-server",
            "rust",
            "--config",
        ])
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !started.exists() && !interpreter.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(!interpreter.exists(), "the workspace interpreter was run");
    assert!(
        started.exists(),
        "the server outside the workspace must start"
    );
}

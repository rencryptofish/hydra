use predicates::prelude::*;

/// Test that `hydra ls` runs successfully and outputs something sensible.
/// Even without tmux sessions, it should print "No sessions" or list sessions.
#[test]
fn test_ls_runs() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.arg("ls");
    cmd.assert().success();
}

/// Test that `hydra --help` shows usage information.
#[test]
fn test_help_flag() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("AI Agent tmux session manager"));
}

/// Test that `hydra new` without arguments fails with an error about missing args.
#[test]
fn test_new_missing_args() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.arg("new");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

/// Test that `hydra kill` without arguments fails.
#[test]
fn test_kill_missing_args() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.arg("kill");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

/// Test that `hydra new` with an invalid agent type fails.
#[test]
fn test_new_invalid_agent() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.args(["new", "invalid-agent", "test-session"]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Unknown agent type"));
}

/// Test that an unknown subcommand produces an error.
#[test]
fn test_unknown_subcommand() {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("hydra");
    cmd.arg("foobar");
    cmd.assert().failure();
}

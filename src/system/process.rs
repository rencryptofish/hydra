use anyhow::{bail, Context, Result as AnyhowResult};
use tokio::process::Command;

/// Run a command with a timeout, returning its output.
pub async fn run_cmd_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> AnyhowResult<std::process::Output> {
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(result) => result.context("subprocess failed to execute"),
        Err(_) => bail!("subprocess timed out after {}s", timeout.as_secs()),
    }
}

/// Read tmux pane PID for a session.
pub async fn get_tmux_pane_pid(tmux_name: &str) -> Option<u32> {
    let output = run_cmd_timeout(
        Command::new("tmux").args(["list-panes", "-t", tmux_name, "-F", "#{pane_pid}"]),
        std::time::Duration::from_secs(5),
    )
    .await
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u32>().ok()
}

/// Collect descendant PIDs from the process tree rooted at `pid`.
pub async fn collect_descendant_pids(pid: u32, max_depth: usize, max_pids: usize) -> Vec<u32> {
    let mut all_pids = Vec::with_capacity(max_pids.min(16));
    all_pids.push(pid);

    let mut current_level = vec![pid];
    let mut depth = 0usize;

    while !current_level.is_empty() && depth < max_depth && all_pids.len() < max_pids {
        let mut next_level = Vec::new();

        for parent in current_level {
            if all_pids.len() >= max_pids {
                break;
            }

            let output = run_cmd_timeout(
                Command::new("pgrep").args(["-P", &parent.to_string()]),
                std::time::Duration::from_secs(5),
            )
            .await;

            let Ok(output) = output else {
                continue;
            };
            if !output.status.success() {
                continue;
            }

            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if all_pids.len() >= max_pids {
                    break;
                }

                if let Ok(child) = line.trim().parse::<u32>() {
                    if !all_pids.contains(&child) {
                        all_pids.push(child);
                        next_level.push(child);
                    }
                }
            }
        }

        current_level = next_level;
        depth += 1;
    }

    all_pids
}

use crate::models::DiffFile;

/// Parse `git diff --numstat` output into per-file stats.
/// Each line: `<insertions>\t<deletions>\t<path>`
/// Binary files show `-\t-\t<path>` — we skip those.
pub fn parse_diff_numstat(output: &str) -> Vec<DiffFile> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let ins_str = parts.next()?;
            let del_str = parts.next()?;
            let path = parts.next()?.to_string();
            if path.is_empty() {
                return None;
            }
            let insertions = ins_str.parse().ok()?; // skips binary "-"
            let deletions = del_str.parse().ok()?;
            Some(DiffFile {
                path,
                insertions,
                deletions,
                untracked: false,
            })
        })
        .collect()
}

/// Maximum number of diff files to process (bounds sort + render cost per tick).
const MAX_DIFF_FILES: usize = 200;

/// Get per-file git diff stats for the working tree, including untracked files.
pub(crate) async fn get_git_diff_numstat(cwd: &str) -> Vec<DiffFile> {
    // Determine the diff target: HEAD if it exists, otherwise the empty tree hash.
    let target = match tokio::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(o) if o.status.success() => "HEAD".to_string(),
        _ => "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string(),
    };

    let git_future = async {
        tokio::join!(
            tokio::process::Command::new("git")
                .args(["diff", &target, "--numstat"])
                .current_dir(cwd)
                .output(),
            tokio::process::Command::new("git")
                .args(["ls-files", "--others", "--exclude-standard"])
                .current_dir(cwd)
                .output(),
        )
    };

    let (diff_out, untracked_out) =
        match tokio::time::timeout(std::time::Duration::from_secs(3), git_future).await {
            Ok(results) => results,
            Err(_) => return Vec::new(), // timeout — return empty
        };

    let mut files = match diff_out {
        Ok(o) if o.status.success() => parse_diff_numstat(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    };

    if let Ok(o) = untracked_out {
        if o.status.success() {
            for path in String::from_utf8_lossy(&o.stdout).lines() {
                let path = path.trim();
                if !path.is_empty() {
                    files.push(DiffFile {
                        path: path.to_string(),
                        insertions: 0,
                        deletions: 0,
                        untracked: true,
                    });
                }
            }
        }
    }

    files.truncate(MAX_DIFF_FILES);
    files
}

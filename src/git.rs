use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{Config, WorktreeSpec};
use crate::model::{RepoStatus, Snapshot, WorktreeRow, WorktreeState};
use crate::util::{compact, run_output};

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorktreeEntry {
    path: PathBuf,
    branch: String,
}

pub fn build_snapshot(config: &Config) -> Snapshot {
    Snapshot {
        base_branch: config.base_branch.clone(),
        repo: repo_status(&config.repo_path),
        worktrees: worktree_rows(config),
    }
}

pub fn repo_status(path: &Path) -> RepoStatus {
    let status = worktree_status(path);
    RepoStatus {
        path: path.to_path_buf(),
        branch: worktree_branch(path),
        dirty: !status.is_empty(),
        summary: if status.is_empty() {
            "clean".to_string()
        } else {
            status
        },
    }
}

pub fn worktree_rows(config: &Config) -> Vec<WorktreeRow> {
    let entries = git_worktree_entries(&config.repo_path);
    if config.worktrees.is_empty() {
        return discovered_rows(config, &entries);
    }
    configured_rows(config, &entries)
}

fn configured_rows(config: &Config, entries: &[WorktreeEntry]) -> Vec<WorktreeRow> {
    let by_branch = entries
        .iter()
        .map(|entry| (entry.branch.clone(), entry.clone()))
        .collect::<HashMap<_, _>>();
    let by_path = entries
        .iter()
        .map(|entry| (canonical_string(&entry.path), entry.clone()))
        .collect::<HashMap<_, _>>();
    config
        .worktrees
        .iter()
        .map(|spec| {
            let entry = by_branch.get(&spec.branch).or_else(|| {
                spec.path
                    .as_ref()
                    .and_then(|path| by_path.get(&canonical_string(path)))
            });
            let path = entry
                .map(|entry| entry.path.clone())
                .or_else(|| spec.path.clone())
                .unwrap_or_else(|| {
                    config
                        .repo_path
                        .parent()
                        .unwrap_or(&config.repo_path)
                        .join(&spec.label)
                });
            row_for(config, spec, path)
        })
        .collect()
}

fn discovered_rows(config: &Config, entries: &[WorktreeEntry]) -> Vec<WorktreeRow> {
    let mut rows = entries
        .iter()
        .filter(|entry| entry.branch != config.base_branch)
        .filter(|entry| !entry.branch.is_empty())
        .map(|entry| {
            let label = entry
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&entry.branch)
                .to_string();
            row_for(
                config,
                &WorktreeSpec {
                    label,
                    branch: entry.branch.clone(),
                    path: Some(entry.path.clone()),
                },
                entry.path.clone(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| worktree_sort_key(&row.label));
    rows
}

fn row_for(config: &Config, spec: &WorktreeSpec, path: PathBuf) -> WorktreeRow {
    let present = path.join(".git").exists();
    let branch_exists = git_success(
        &["git", "rev-parse", "--verify", &spec.branch],
        &config.repo_path,
    );
    let current_branch = if present {
        worktree_branch(&path)
    } else {
        String::new()
    };
    let dirty_summary = if present {
        worktree_status(&path)
    } else {
        String::new()
    };
    let (ahead, behind) = if branch_exists {
        branch_counts(&config.repo_path, &config.base_branch, &spec.branch)
    } else {
        (0, 0)
    };
    let state = worktree_state(
        present,
        branch_exists,
        &spec.branch,
        &current_branch,
        &dirty_summary,
        ahead,
        behind,
    );
    WorktreeRow {
        label: spec.label.clone(),
        branch: spec.branch.clone(),
        path,
        current_branch,
        dirty: !dirty_summary.is_empty(),
        summary: if dirty_summary.is_empty() {
            "clean".to_string()
        } else {
            dirty_summary
        },
        ahead,
        behind,
        state,
        subject: if branch_exists {
            compact(
                git_stdout(
                    &["git", "log", "-1", "--pretty=%s", &spec.branch],
                    &config.repo_path,
                ),
                100,
            )
        } else {
            String::new()
        },
    }
}

pub fn worktree_state(
    present: bool,
    branch_exists: bool,
    expected_branch: &str,
    current_branch: &str,
    dirty_summary: &str,
    ahead: i64,
    behind: i64,
) -> WorktreeState {
    if !present || !branch_exists {
        WorktreeState::Missing
    } else if current_branch != expected_branch {
        WorktreeState::WrongBranch
    } else if !dirty_summary.is_empty() {
        WorktreeState::Dirty
    } else if ahead > 0 {
        WorktreeState::Ready
    } else if behind > 0 {
        WorktreeState::Behind
    } else {
        WorktreeState::Merged
    }
}

pub fn git_stdout(argv: &[&str], cwd: &Path) -> String {
    run_output(argv.iter().copied(), Some(cwd))
}

pub fn git_success(argv: &[&str], cwd: &Path) -> bool {
    std::process::Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn worktree_branch(path: &Path) -> String {
    git_stdout(&["git", "branch", "--show-current"], path)
        .trim()
        .to_string()
}

pub fn worktree_status(path: &Path) -> String {
    let output = git_stdout(&["git", "status", "--short"], path);
    let count = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    match count {
        0 => String::new(),
        1 => "1 changed file".to_string(),
        count => format!("{count} changed files"),
    }
}

pub fn git_ref(repo_path: &Path, reference: &str) -> String {
    git_stdout(&["git", "rev-parse", reference], repo_path)
        .trim()
        .to_string()
}

pub fn short_sha(value: &str) -> String {
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.chars().take(8).collect()
    }
}

fn git_worktree_entries(repo_path: &Path) -> Vec<WorktreeEntry> {
    parse_worktree_list(&git_stdout(
        &["git", "worktree", "list", "--porcelain"],
        repo_path,
    ))
}

fn parse_worktree_list(output: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for raw_line in output.lines() {
        if raw_line.trim().is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        if let Some(path) = raw_line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                branch: String::new(),
            });
        } else if let Some(branch) = raw_line.strip_prefix("branch ")
            && let Some(entry) = &mut current
        {
            entry.branch = branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_string();
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

fn branch_counts(repo_path: &Path, base_branch: &str, branch: &str) -> (i64, i64) {
    let output = git_stdout(
        &[
            "git",
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base_branch}...{branch}"),
        ],
        repo_path,
    );
    let parts = output.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 {
        return (0, 0);
    }
    let behind = parts[0].parse::<i64>().unwrap_or(0);
    let ahead = parts[1].parse::<i64>().unwrap_or(0);
    (ahead, behind)
}

fn canonical_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn worktree_sort_key(label: &str) -> (i32, String) {
    match label {
        "agent-A" => (0, label.to_string()),
        "agent-B" => (1, label.to_string()),
        "agent-C" => (2, label.to_string()),
        "agent-D" => (3, label.to_string()),
        label if label.contains("annotation") => (99, label.to_string()),
        _ => (10, label.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_worktree_output() {
        let rows = parse_worktree_list(
            "/repo\nworktree /repo\nHEAD 111\nbranch refs/heads/main\n\nworktree /repo-worktrees/agent-A\nHEAD 222\nbranch refs/heads/agent/A\n",
        );

        assert_eq!(2, rows.len());
        assert_eq!("agent/A", rows[1].branch);
    }

    #[test]
    fn state_marks_dirty_before_ready() {
        assert_eq!(
            WorktreeState::Dirty,
            worktree_state(true, true, "agent/A", "agent/A", "1 changed file", 1, 0)
        );
        assert_eq!(
            WorktreeState::Ready,
            worktree_state(true, true, "agent/A", "agent/A", "", 1, 0)
        );
    }
}

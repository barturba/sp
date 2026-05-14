use std::process::Command;

use crate::config::Config;
use crate::git::{
    build_snapshot, git_ref, git_success, short_sha, worktree_branch, worktree_status,
};
use crate::model::{WorktreeRow, WorktreeState};
use crate::util::{compact, shell_join};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationResult {
    pub ok: bool,
    pub message: String,
}

impl OperationResult {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    fn blocked(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

pub fn merge_worktree(
    config: &Config,
    label: &str,
    log: &mut dyn FnMut(String),
) -> OperationResult {
    let Some(target) = build_snapshot(config)
        .worktrees
        .into_iter()
        .find(|row| row.label == label)
    else {
        return OperationResult::blocked(format!("Merge blocked: unknown worktree {label}"));
    };
    if target.state != WorktreeState::Ready {
        return OperationResult::blocked(format!(
            "Merge blocked: {} is {}",
            target.label, target.state
        ));
    }
    if let Some(blocker) = base_checkout_blocker(config, "Merge") {
        return OperationResult::blocked(blocker);
    }
    if let Some(blocker) = target_blocker(&target) {
        return OperationResult::blocked(blocker);
    }

    let main_before = git_ref(&config.repo_path, "HEAD");
    let branch_before = git_ref(&config.repo_path, &target.branch);
    log(format!(
        "{}: base {} branch {}",
        target.label,
        short_sha(&main_before),
        short_sha(&branch_before)
    ));

    let result = run_command(&["git", "rebase", &config.base_branch], &target.path, log);
    if result.code != 0 {
        run_command(&["git", "rebase", "--abort"], &target.path, log);
        return OperationResult::blocked(format!(
            "Merge blocked: {} rebase failed: {}",
            target.label,
            compact(result.output, 160)
        ));
    }

    let result = run_command(
        &["git", "merge", "--ff-only", &target.branch],
        &config.repo_path,
        log,
    );
    if result.code != 0 {
        let merge = run_command(
            &["git", "merge", "--no-ff", "--no-commit", &target.branch],
            &config.repo_path,
            log,
        );
        if merge.code != 0 {
            run_command(&["git", "merge", "--abort"], &config.repo_path, log);
            return OperationResult::blocked(format!(
                "Merge blocked: {} conflicts: {}",
                target.label,
                compact(merge.output, 160)
            ));
        }
        let commit = run_command(
            &["git", "-c", "commit.gpgsign=false", "commit", "--no-edit"],
            &config.repo_path,
            log,
        );
        if commit.code != 0 {
            run_command(&["git", "merge", "--abort"], &config.repo_path, log);
            return OperationResult::blocked(format!(
                "Merge failed: {} commit failed: {}",
                target.label,
                compact(commit.output, 120)
            ));
        }
    }

    verified_merge(config, &target, &main_before, log)
}

pub fn merge_all(config: &Config, log: &mut dyn FnMut(String)) -> OperationResult {
    let mut merged = Vec::new();
    let snapshot = build_snapshot(config);
    for row in snapshot.worktrees {
        if row.state != WorktreeState::Ready {
            continue;
        }
        let result = merge_worktree(config, &row.label, log);
        if !result.ok {
            let suffix = if merged.is_empty() {
                String::new()
            } else {
                format!(" after merging {}", merged.join(", "))
            };
            return OperationResult::blocked(format!("{}{suffix}", result.message));
        }
        merged.push(row.label);
    }
    if merged.is_empty() {
        OperationResult::blocked("Merge skipped: no ready worktrees")
    } else {
        OperationResult::ok(format!("Merged {}", merged.join(", ")))
    }
}

pub fn rebase_all(config: &Config, log: &mut dyn FnMut(String)) -> OperationResult {
    if let Some(blocker) = base_checkout_blocker(config, "Rebase") {
        return OperationResult::blocked(blocker);
    }
    if let Some(result) = sync_base(config, log) {
        return result;
    }
    let mut rebased = Vec::new();
    let mut skipped = 0;
    for row in build_snapshot(config).worktrees {
        if row.state == WorktreeState::Missing {
            skipped += 1;
            continue;
        }
        let result = rebase_one(config, &row, log);
        if !result.ok {
            let suffix = if rebased.is_empty() {
                String::new()
            } else {
                format!(" after rebasing {}", rebased.join(", "))
            };
            return OperationResult::blocked(format!("{}{suffix}", result.message));
        }
        rebased.push(row.label);
    }
    if rebased.is_empty() {
        OperationResult::blocked("Rebase skipped: no present worktrees")
    } else {
        let suffix = if skipped == 0 {
            String::new()
        } else {
            format!("; skipped {skipped} missing")
        };
        OperationResult::ok(format!("Rebased {}{suffix}", rebased.join(", ")))
    }
}

pub fn deploy(config: &Config, log: &mut dyn FnMut(String)) -> OperationResult {
    if let Some(blocker) = base_checkout_blocker(config, "Deploy") {
        return OperationResult::blocked(blocker);
    }
    let Some(command) = &config.deploy_command else {
        return OperationResult::blocked("Deploy blocked: no deploy_command in sp.toml");
    };
    if command.is_empty() {
        return OperationResult::blocked("Deploy blocked: empty deploy_command");
    }
    let argv = command.iter().map(String::as_str).collect::<Vec<_>>();
    let result = run_command(&argv, &config.repo_path, log);
    if result.code == 0 {
        OperationResult::ok("Deploy completed")
    } else {
        OperationResult::blocked(format!("Deploy failed: {}", compact(result.output, 160)))
    }
}

fn sync_base(config: &Config, log: &mut dyn FnMut(String)) -> Option<OperationResult> {
    if !git_success(&["git", "remote", "get-url", "origin"], &config.repo_path) {
        log("No origin remote; rebasing onto local base branch".to_string());
        return None;
    }
    let fetch = run_command(&["git", "fetch", "origin"], &config.repo_path, log);
    if fetch.code != 0 {
        return Some(OperationResult::blocked(format!(
            "Rebase blocked: fetch origin failed: {}",
            compact(fetch.output, 120)
        )));
    }
    let remote = format!("origin/{}", config.base_branch);
    if !git_success(
        &["git", "rev-parse", "--verify", &remote],
        &config.repo_path,
    ) {
        log(format!("{remote} missing; rebasing onto local base branch"));
        return None;
    }
    let merge = run_command(
        &["git", "merge", "--ff-only", &remote],
        &config.repo_path,
        log,
    );
    if merge.code != 0 {
        return Some(OperationResult::blocked(format!(
            "Rebase blocked: base does not fast-forward to {remote}: {}",
            compact(merge.output, 120)
        )));
    }
    None
}

fn rebase_one(config: &Config, row: &WorktreeRow, log: &mut dyn FnMut(String)) -> OperationResult {
    if let Some(blocker) = target_blocker(row) {
        return OperationResult::blocked(blocker.replace("Merge", "Rebase"));
    }
    let stash_message = format!("sp auto-stash before rebase {}", row.label);
    let stashed = !worktree_status(&row.path).is_empty();
    if stashed {
        let stash = run_command(
            &["git", "stash", "push", "-u", "-m", &stash_message],
            &row.path,
            log,
        );
        if stash.code != 0 {
            return OperationResult::blocked(format!(
                "Rebase blocked: {} stash failed: {}",
                row.label,
                compact(stash.output, 120)
            ));
        }
    }
    let rebase = run_command(&["git", "rebase", &config.base_branch], &row.path, log);
    if rebase.code != 0 {
        run_command(&["git", "rebase", "--abort"], &row.path, log);
        if stashed {
            run_command(&["git", "stash", "pop", "--index"], &row.path, log);
        }
        return OperationResult::blocked(format!(
            "Rebase blocked: {} conflicts: {}",
            row.label,
            compact(rebase.output, 120)
        ));
    }
    if stashed {
        let restore = run_command(&["git", "stash", "pop", "--index"], &row.path, log);
        if restore.code != 0 {
            return OperationResult::blocked(format!(
                "Rebase blocked: {} stash restore failed: {}",
                row.label,
                compact(restore.output, 120)
            ));
        }
    }
    OperationResult::ok(format!("Rebased {}", row.label))
}

fn verified_merge(
    config: &Config,
    target: &WorktreeRow,
    main_before: &str,
    log: &mut dyn FnMut(String),
) -> OperationResult {
    let main_after = git_ref(&config.repo_path, "HEAD");
    if !worktree_status(&config.repo_path).is_empty() {
        return OperationResult::blocked(format!("Merge failed: {} left base dirty", target.label));
    }
    if target.ahead > 0 && main_after == main_before {
        return OperationResult::blocked(format!(
            "Merge failed: {} did not advance {} from {}",
            target.label,
            config.base_branch,
            short_sha(main_before)
        ));
    }
    if !git_success(
        &[
            "git",
            "merge-base",
            "--is-ancestor",
            &target.branch,
            &config.base_branch,
        ],
        &config.repo_path,
    ) {
        return OperationResult::blocked(format!(
            "Merge failed: {} does not contain {}",
            config.base_branch, target.label
        ));
    }
    let message = format!(
        "Merged {}: {} {} -> {}",
        target.label,
        config.base_branch,
        short_sha(main_before),
        short_sha(&main_after)
    );
    log(message.clone());
    OperationResult::ok(message)
}

fn base_checkout_blocker(config: &Config, action: &str) -> Option<String> {
    let branch = worktree_branch(&config.repo_path);
    if branch != config.base_branch {
        return Some(format!(
            "{action} blocked: base checkout on {}",
            if branch.is_empty() {
                "no branch"
            } else {
                &branch
            }
        ));
    }
    if !worktree_status(&config.repo_path).is_empty() {
        return Some(format!("{action} blocked: base checkout dirty"));
    }
    None
}

fn target_blocker(row: &WorktreeRow) -> Option<String> {
    if !row.path.join(".git").exists() {
        return Some(format!("Merge blocked: {} worktree missing", row.label));
    }
    let current_branch = worktree_branch(&row.path);
    if current_branch != row.branch {
        return Some(format!(
            "Merge blocked: {} on {}",
            row.label,
            if current_branch.is_empty() {
                "no branch"
            } else {
                &current_branch
            }
        ));
    }
    if !worktree_status(&row.path).is_empty() {
        return Some(format!("Merge blocked: {} dirty", row.label));
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandResult {
    code: i32,
    output: String,
}

fn run_command(argv: &[&str], cwd: &std::path::Path, log: &mut dyn FnMut(String)) -> CommandResult {
    let display = argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    log(format!("$ {}", shell_join(&display)));
    let output = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .output();
    let Ok(output) = output else {
        let message = format!("failed to start {}", argv[0]);
        log(message.clone());
        return CommandResult {
            code: 127,
            output: message,
        };
    };
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        log(line.to_string());
    }
    CommandResult {
        code: output.status.code().unwrap_or(1),
        output: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn commit_file(repo: &Path, path: &str, body: &str, message: &str) {
        let file = repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, body).unwrap();
        git(&["add", path], repo);
        git(
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                message,
            ],
            repo,
        );
    }

    #[test]
    fn merge_worktree_fast_forwards_base() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("agent-A");
        fs::create_dir(&repo).unwrap();
        git(&["init"], &repo);
        git(&["checkout", "-b", "main"], &repo);
        commit_file(&repo, "README.md", "repo\n", "init");
        git(
            &[
                "worktree",
                "add",
                "-b",
                "agent/A",
                worktree.to_str().unwrap(),
                "main",
            ],
            &repo,
        );
        commit_file(&worktree, "agent.txt", "agent\n", "agent change");
        let config = Config {
            repo_path: repo.clone(),
            base_branch: "main".to_string(),
            deploy_command: None,
            worktrees: vec![crate::config::WorktreeSpec {
                label: "agent-A".to_string(),
                branch: "agent/A".to_string(),
                path: Some(worktree),
            }],
        };
        let mut logs = Vec::new();

        let result = merge_worktree(&config, "agent-A", &mut |line| logs.push(line));

        assert!(result.ok, "{}", result.message);
        assert!(result.message.contains("Merged agent-A"));
        assert!(
            logs.iter()
                .any(|line| line == "$ git merge --ff-only agent/A")
        );
    }
}

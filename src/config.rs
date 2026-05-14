use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub repo_path: PathBuf,
    pub base_branch: String,
    pub deploy_command: Option<Vec<String>>,
    pub worktrees: Vec<WorktreeSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeSpec {
    pub label: String,
    pub branch: String,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    base_branch: Option<String>,
    deploy_command: Option<String>,
    worktree: Option<Vec<FileWorktree>>,
}

#[derive(Debug, Deserialize)]
struct FileWorktree {
    label: String,
    branch: String,
    path: Option<PathBuf>,
}

impl Config {
    pub fn load(repo: Option<PathBuf>, base: String, config_path: Option<PathBuf>) -> Result<Self> {
        let repo_path = repo
            .unwrap_or(std::env::current_dir().context("current directory")?)
            .canonicalize()
            .context("repository path")?;
        let config_path = config_path.or_else(|| {
            let candidate = repo_path.join("sp.toml");
            candidate.exists().then_some(candidate)
        });
        let file_config = match config_path {
            Some(path) => Some(load_file_config(&path)?),
            None => None,
        };
        let base_branch = file_config
            .as_ref()
            .and_then(|config| config.base_branch.clone())
            .unwrap_or(base);
        let deploy_command = file_config
            .as_ref()
            .and_then(|config| config.deploy_command.as_deref())
            .map(shell_words);
        let worktrees = file_config
            .and_then(|config| config.worktree)
            .unwrap_or_default()
            .into_iter()
            .map(|row| WorktreeSpec {
                label: row.label,
                branch: row.branch,
                path: row.path.map(|path| expand_relative_path(&repo_path, path)),
            })
            .collect();
        Ok(Self {
            repo_path,
            base_branch,
            deploy_command,
            worktrees,
        })
    }
}

fn load_file_config(path: &Path) -> Result<FileConfig> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn expand_relative_path(repo_path: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        repo_path.join(path)
    }
}

fn shell_words(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path();
        fs::write(
            repo.join("sp.toml"),
            r#"
base_branch = "trunk"
deploy_command = "bin/deploy"

[[worktree]]
label = "agent-A"
branch = "agent/A"
path = "../repo-worktrees/agent-A"
"#,
        )
        .unwrap();

        let config = Config::load(Some(repo.to_path_buf()), "main".to_string(), None).unwrap();

        assert_eq!("trunk", config.base_branch);
        assert_eq!(Some(vec!["bin/deploy".to_string()]), config.deploy_command);
        assert_eq!("agent-A", config.worktrees[0].label);
    }
}

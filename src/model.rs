use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoStatus {
    pub path: PathBuf,
    pub branch: String,
    pub dirty: bool,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRow {
    pub label: String,
    pub branch: String,
    pub path: PathBuf,
    pub current_branch: String,
    pub state: WorktreeState,
    pub dirty: bool,
    pub summary: String,
    pub ahead: i64,
    pub behind: i64,
    pub subject: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeState {
    Ready,
    Dirty,
    Behind,
    Merged,
    Missing,
    WrongBranch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub base_branch: String,
    pub repo: RepoStatus,
    pub worktrees: Vec<WorktreeRow>,
}

impl Snapshot {
    pub fn summary(&self) -> String {
        let ready = self.count(WorktreeState::Ready);
        let dirty = self.count(WorktreeState::Dirty);
        let blocked = self.count(WorktreeState::Missing) + self.count(WorktreeState::WrongBranch);
        let behind = self.count(WorktreeState::Behind);
        let mut parts = Vec::new();
        if ready > 0 {
            parts.push(plural(ready, "branch ready", "branches ready"));
        }
        if dirty > 0 {
            parts.push(plural(dirty, "branch dirty", "branches dirty"));
        }
        if blocked > 0 {
            parts.push(plural(blocked, "branch blocked", "branches blocked"));
        }
        if behind > 0 {
            parts.push(plural(behind, "branch behind", "branches behind"));
        }
        if self.repo.dirty {
            parts.push("base dirty".to_string());
        }
        if parts.is_empty() {
            "Idle - healthy, nothing to do".to_string()
        } else if ready > 0 && !self.repo.dirty {
            format!("Ready - {}", parts.join(", "))
        } else {
            format!("Blocked - {}", parts.join(", "))
        }
    }

    fn count(&self, state: WorktreeState) -> usize {
        self.worktrees
            .iter()
            .filter(|row| row.state == state)
            .count()
    }
}

impl WorktreeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Dirty => "dirty",
            Self::Behind => "behind",
            Self::Merged => "merged",
            Self::Missing => "missing",
            Self::WrongBranch => "wrong branch",
        }
    }
}

impl std::fmt::Display for WorktreeState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_prefers_ready_when_base_is_clean() {
        let snapshot = Snapshot {
            base_branch: "main".to_string(),
            repo: RepoStatus {
                path: ".".into(),
                branch: "main".to_string(),
                dirty: false,
                summary: "clean".to_string(),
            },
            worktrees: vec![WorktreeRow {
                label: "agent-A".to_string(),
                branch: "agent/A".to_string(),
                path: ".".into(),
                current_branch: "agent/A".to_string(),
                state: WorktreeState::Ready,
                dirty: false,
                summary: "clean".to_string(),
                ahead: 1,
                behind: 0,
                subject: "ship it".to_string(),
            }],
        };

        assert_eq!("Ready - 1 branch ready", snapshot.summary());
    }
}

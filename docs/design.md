# Design Notes

`sp` is a cockpit for one specific workflow: a base checkout plus multiple sibling worktrees doing independent work.

The program has three layers:

- `git`: reads repository and worktree state from normal git commands.
- `ops`: runs merge, merge-all, rebase-all, and deploy with conservative blockers.
- `ui`: renders a Ratatui dashboard and sends expensive work to background threads.

The UI does not own repository truth. It keeps a snapshot for rendering, asks the snapshot worker for fresh state, and asks the operation worker to mutate git. That split keeps input handling fast even when a repository has many worktrees.

Merge automation is intentionally narrow. A target branch must be clean and ahead of the base branch. The base checkout must be clean and on the configured base branch. `sp` rebases the target first, then fast-forwards the base checkout when possible. If a clean non-fast-forward merge is needed, it creates Git's normal merge commit. If conflicts appear, it aborts and reports the blocker.

Rebase-all has a different safety shape. Dirty worktrees are allowed because catching up should not require throwing away local scratch work. `sp` stashes dirty files, rebases the branch, and restores the stash. If the restore fails, the operation stops and tells the operator which worktree needs attention.

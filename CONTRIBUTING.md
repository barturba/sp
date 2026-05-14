# Contributing

`sp` is intentionally small. Contributions should keep the command fast, local, and explainable.

## Development Loop

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
```

## Design Rules

- Prefer explicit git commands over hidden state.
- Keep the TUI responsive by moving slow work off the input/render loop.
- Block risky operations with direct messages instead of guessing.
- Add tests for git behavior that could lose work or leave a checkout half-mutated.
- Keep configuration boring: one optional `sp.toml`, no daemon, no background database.

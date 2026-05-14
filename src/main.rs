use anyhow::Result;
use clap::Parser;

mod config;
mod git;
mod model;
mod ops;
mod util;

use config::Config;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "A fast terminal cockpit for git worktrees and agent branches."
)]
struct Args {
    /// Repository checkout to inspect.
    #[arg(long, value_name = "PATH")]
    repo: Option<std::path::PathBuf>,

    /// Base branch that agent branches merge into.
    #[arg(long, default_value = "main")]
    base: String,

    /// Optional TOML config file. Defaults to ./sp.toml when present.
    #[arg(long, value_name = "FILE")]
    config: Option<std::path::PathBuf>,

    /// Print one snapshot and exit instead of starting the TUI.
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(args.repo, args.base, args.config)?;
    if args.once {
        let snapshot = git::build_snapshot(&config);
        println!("{}", snapshot.summary());
        for row in snapshot.worktrees {
            println!(
                "{:<18} {:<24} {:<12} +{}/-{}  {}",
                row.label, row.branch, row.state, row.ahead, row.behind, row.subject
            );
        }
        return Ok(());
    }
    println!("{}", git::build_snapshot(&config).summary());
    Ok(())
}

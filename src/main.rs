//! `portool` CLI entry point.
//!
//! This currently defines only the `clap` command surface; all handlers
//! are placeholders replaced by the command implementations (Task 3).

use clap::{Parser, Subcommand};
use std::process::exit;

#[derive(Parser)]
#[command(
    name = "portool",
    version,
    about = "Passive global port ledger for git worktrees"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install the post-checkout hook, update .gitignore, and run sync.
    Init {
        /// Only install the post-checkout hook.
        #[arg(long, conflicts_with = "gitignore_only")]
        hook_only: bool,
        /// Only append `.env.portool` to .gitignore.
        #[arg(long)]
        gitignore_only: bool,
    },
    /// Allocate or refresh the port block for the current worktree.
    Sync {
        /// Suppress normal-case output (used by the git hook).
        #[arg(long)]
        quiet: bool,
    },
    /// List allocated port blocks.
    Ls {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Show all projects instead of just the current one.
        #[arg(long)]
        all: bool,
    },
    /// Reclaim stale worktree entries.
    Prune {
        /// Operate across all projects instead of just the current one.
        #[arg(long)]
        all: bool,
        /// Report what would be pruned without modifying the ledger.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Init {
            hook_only,
            gitignore_only,
        } => {
            let _ = (hook_only, gitignore_only);
            eprintln!("not implemented");
            exit(1);
        }
        Command::Sync { quiet } => {
            let _ = quiet;
            eprintln!("not implemented");
            exit(1);
        }
        Command::Ls { json, all } => {
            let _ = (json, all);
            eprintln!("not implemented");
            exit(1);
        }
        Command::Prune { all, dry_run } => {
            let _ = (all, dry_run);
            eprintln!("not implemented");
            exit(1);
        }
    }
}

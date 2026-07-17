//! `portool` CLI entry point: `clap` parsing, dispatch to [`portool::cmd`],
//! and exit-code mapping only.

use clap::{Parser, Subcommand};
use portool::cmd;
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
    /// Run a command with the worktree's allocated ports in its environment.
    Exec {
        /// Env file(s) to load, in order; later files override earlier ones.
        #[arg(short = 'e', long = "env-file", value_name = "PATH")]
        env_file: Vec<std::path::PathBuf>,
        /// Fail (exit 1) if the allocated block's ports are already in use.
        #[arg(long)]
        strict: bool,
        /// Move to a fresh block if the allocated block's ports are in use.
        #[arg(long)]
        reallocate_on_conflict: bool,
        /// The command to run (everything after `--`).
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<std::ffi::OsString>,
    },
    /// Force the current worktree onto a fresh port block.
    Reallocate {
        /// Suppress the normal-case summary line on stdout.
        #[arg(long)]
        quiet: bool,
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
    /// Validate the config and ledger; exit non-zero on any problem.
    Check,
    /// Diagnose and repair the current project (rebuild lost entries, report
    /// blocks whose ports are in use).
    Doctor {
        /// Move a corrupt (or unsupported-version) ledger aside and rebuild
        /// this project's entries from live worktrees' `.env.portool`.
        #[arg(long)]
        repair: bool,
    },
    /// Free the current worktree's block and remove its `.env.portool`.
    Release,
    /// Remove portool's hooks and `.gitignore` entry (reverses `init`).
    Deinit,
}

fn main() {
    // Keep clap's usage errors off portool's semantic exit codes (batch B
    // #15): a usage error exits 64 (EX_USAGE) rather than clap's default 2,
    // which used to collide with a real allocation error. `--help` /
    // `--version` still print to stdout and exit 0.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            let code = match err.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 64,
            };
            exit(code);
        }
    };

    let result = match cli.command {
        Command::Init {
            hook_only,
            gitignore_only,
        } => cmd::init::run(hook_only, gitignore_only),
        Command::Sync { quiet } => cmd::sync::run(quiet),
        Command::Ls { json, all } => cmd::ls::run(json, all),
        Command::Exec {
            env_file,
            strict,
            reallocate_on_conflict,
            command,
        } => cmd::exec::run(&env_file, &command, strict, reallocate_on_conflict),
        Command::Reallocate { quiet } => cmd::sync::reallocate_cmd(quiet),
        Command::Prune { all, dry_run } => cmd::prune::run(all, dry_run),
        Command::Check => cmd::check::run(),
        Command::Doctor { repair } => cmd::doctor::run(repair),
        Command::Release => cmd::release::run(),
        Command::Deinit => cmd::init::deinit(),
    };

    if let Err(err) = result {
        eprintln!("portool: error: {err}");
        exit(err.exit_code());
    }
}

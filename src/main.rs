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
    /// Install the post-checkout hook, update git's info/exclude, and run
    /// sync.
    Init {
        /// Only install the post-checkout hook.
        #[arg(long, conflicts_with = "gitignore_only")]
        hook_only: bool,
        /// Only add `.env.portool` to $GIT_COMMON_DIR/info/exclude.
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
        /// Warn if the allocated block's ports are already in use (advisory
        /// bind check; off by default -- your own dev servers legitimately
        /// occupy the block).
        #[arg(long)]
        check_ports: bool,
        /// Fail (exit 1) if the allocated block's ports are already in use
        /// (implies --check-ports).
        #[arg(long, conflicts_with = "reallocate_on_conflict")]
        strict: bool,
        /// Move to a fresh block if the allocated block's ports are in use
        /// (implies --check-ports). DANGER: processes already running keep
        /// the old ports, so the worktree can end up split across blocks.
        #[arg(long)]
        reallocate_on_conflict: bool,
        /// Let the given env files override inherited environment variables
        /// (default: the parent environment wins over env files).
        #[arg(long)]
        env_file_overrides: bool,
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
        /// Repair a corrupt ledger: restore the whole ledger from its
        /// backup, then rebuild this project's missing entries.
        #[arg(long)]
        repair: bool,
        /// With --repair and no usable backup: discard the bad ledger and
        /// rebuild only this project. Every other project's allocations are
        /// dropped until doctor runs there. DESTRUCTIVE.
        #[arg(long, requires = "repair")]
        abandon_other_projects: bool,
    },
    /// Free the current worktree's block and remove its `.env.portool`.
    Release,
    /// Remove portool's lines from this repo's git hooks (and nothing else).
    Unhook,
    /// Release this project's allocations and remove portool's env files,
    /// hooks, and ignore rule (full reverse of init).
    Deinit {
        /// Keep the ledger allocations and .env.portool files; only remove
        /// hooks and the ignore rule.
        #[arg(long)]
        keep_allocations: bool,
    },
    /// Reserve a port or port range so portool never allocates over it.
    Reserve {
        /// PORT or START-END (inclusive), e.g. 5432 or 6000-6009.
        ports: String,
        /// Optional label shown in ls (e.g. "postgres").
        #[arg(long)]
        label: Option<String>,
    },
    /// Remove a reservation (single port matches its containing block).
    Unreserve {
        /// PORT or START-END (inclusive).
        ports: String,
    },
    /// Protect the current worktree's allocation from GC.
    Pin {
        /// Optional label shown in ls.
        #[arg(long)]
        label: Option<String>,
    },
    /// Remove the current worktree's GC protection.
    Unpin,
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
            check_ports,
            strict,
            reallocate_on_conflict,
            env_file_overrides,
            command,
        } => cmd::exec::run(
            &env_file,
            &command,
            check_ports,
            strict,
            reallocate_on_conflict,
            env_file_overrides,
        ),
        Command::Reallocate { quiet } => cmd::sync::reallocate_cmd(quiet),
        Command::Prune { all, dry_run } => cmd::prune::run(all, dry_run),
        Command::Check => cmd::check::run(),
        Command::Doctor {
            repair,
            abandon_other_projects,
        } => cmd::doctor::run(repair, abandon_other_projects),
        Command::Release => cmd::release::run(),
        Command::Unhook => cmd::init::unhook(),
        Command::Deinit { keep_allocations } => cmd::init::deinit(keep_allocations),
        Command::Reserve { ports, label } => cmd::reserve::reserve(&ports, label),
        Command::Unreserve { ports } => cmd::reserve::unreserve(&ports),
        Command::Pin { label } => cmd::pin::pin(label),
        Command::Unpin => cmd::pin::unpin(),
    };

    if let Err(err) = result {
        eprintln!("portool: error: {err}");
        exit(err.exit_code());
    }
}

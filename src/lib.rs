//! portool: a passive global port ledger for git worktrees.
//!
//! The `portool` command line is the primary interface; this crate also
//! exposes a small library so another program can allocate, use, and
//! release a worktree's ports and local-resource identity **in-process**,
//! sharing the very same ledger, store, and locks as the standalone CLI.
//!
//! # Shared state
//!
//! The library reads and writes exactly what the CLI does: the registry at
//! `$XDG_STATE_HOME/portool/registry.json` (fallback
//! `$HOME/.local/state/portool`), guarded by an exclusive `flock` on
//! `registry.json.lock` next to it, and the global
//! `$XDG_CONFIG_HOME/portool/config.toml`. By default those locations come
//! from the process environment on every call. Library and CLI calls -- in
//! the same process, in different processes, or both -- therefore serialize
//! on the same lock and can never be handed the same port block.
//!
//! A caller that runs commands with an environment other than its own
//! (a different `HOME` or `XDG_STATE_HOME`) passes that environment as a
//! [`Context`] to [`Worktree::open_in`], [`release_removed_in`], or
//! [`load_config_in`], so it uses the same ledger and config as those
//! commands:
//!
//! ```no_run
//! # fn main() -> Result<(), portool::Error> {
//! let env = [("HOME", "/home/worker"), ("XDG_STATE_HOME", "/srv/state")];
//! let ctx = portool::Context::from_env(env);
//! let wt = portool::Worktree::open_in(&ctx, "/path/to/a/worktree")?;
//! wt.allocate()?; // uses /srv/state/portool/registry.json
//! portool::release_removed_in(&ctx, "/path/to/main/checkout", "/path/to/removed")?;
//! # Ok(()) }
//! ```
//!
//! # Example
//!
//! ```no_run
//! use portool::Worktree;
//!
//! # fn main() -> Result<(), portool::Error> {
//! let wt = Worktree::open("/path/to/a/worktree")?;
//!
//! // Allocate (or refresh) this worktree's port block; idempotent.
//! let alloc = wt.allocate()?;
//! println!("block {}-{}", alloc.block.0, alloc.block.1);
//! for (name, value) in &alloc.vars {
//!     println!("{name}={value}"); // PORT / <KEY>_PORT, PORTOOL_*_ID
//! }
//!
//! // The environment `portool exec` would give a command.
//! let env = wt.exec_env(&portool::ExecEnv::default())?;
//! # let _ = env.vars;
//!
//! // Warnings come back as data (nothing is printed to stderr).
//! for w in &alloc.warnings {
//!     eprintln!("{w}");
//! }
//!
//! // Free the allocation again.
//! wt.release()?;
//! # Ok(()) }
//! ```
//!
//! # Stability
//!
//! Only the items re-exported below are public API (semver applies);
//! everything else is a crate-private implementation detail.
//!
//! # Warnings
//!
//! The library never writes to standard error. Advisory warnings and hints
//! that the CLI prints (a missing `post-checkout` hook, a deprecated config
//! key, a failed backup refresh, ports in use, ...) are returned to the
//! caller as [`Warning`] values on the result of each call.

mod alloc;
mod cli;
mod cmd;
mod config;
mod display;
mod envfile;
mod envread;
mod error;
mod fault;
mod gitctx;
mod hooks;
mod identity;
mod leftovers;
mod lock;
mod manifest;
mod paths;
mod ports;
mod reclaim;
mod registry;
mod store;
mod warn;

pub use config::Config;
pub use error::{Error, Result};
pub use manifest::Manifest;
pub use paths::Context;

use gitctx::GitCtx;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Runs the `portool` command line in this process with `args` and returns
/// its exit code. This is exactly what the standalone `portool` binary does:
/// its `main` only calls `portool::run_cli(std::env::args_os())` and exits
/// with the result.
///
/// `args` starts with the program name, as from [`std::env::args_os`]; its
/// value is ignored (help and errors always say `portool`). A multi-call
/// binary invoked through a `portool` link can therefore forward its own
/// arguments unchanged.
///
/// Behaviour, output, and exit codes are identical to the binary of the same
/// version, including that it:
///
/// - writes to standard output and standard error;
/// - reads the current directory and the process environment (`HOME`,
///   `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, ...);
/// - for `portool exec`, **replaces the current process** with the command
///   (`execvp`) on success, so the call does not return.
///
/// Call it only where the process acts as the `portool` command. For
/// in-process allocation use [`Worktree`] and [`release_removed`] instead.
///
/// ```no_run
/// let code = portool::run_cli(std::env::args_os());
/// std::process::exit(code);
/// ```
pub fn run_cli<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    cli::run(args.into_iter().map(Into::into).collect())
}

/// An advisory, non-fatal message the CLI would print to standard error
/// (including any `portool: warning:` or `hint:` prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Warning {
    /// The full message line, without a trailing newline.
    pub message: String,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn warnings(lines: Vec<String>) -> Vec<Warning> {
    lines
        .into_iter()
        .map(|message| Warning { message })
        .collect()
}

/// The global configuration plus any warnings raised while reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigLoad {
    /// The configuration.
    pub config: Config,
    /// For example deprecation notices for ignored keys.
    pub warnings: Vec<Warning>,
}

/// Reads the global configuration (`config.toml`): the port pool and block
/// alignment. A missing file yields [`Config::default`]; a malformed one is
/// an error, exactly as for the CLI.
pub fn load_config() -> Result<ConfigLoad> {
    load_config_in(&Context::process())
}

/// [`load_config`] with the config location taken from `context`.
pub fn load_config_in(context: &Context) -> Result<ConfigLoad> {
    let (result, lines) = warn::collect(|| paths::scoped(context, Config::load));
    Ok(ConfigLoad {
        config: result?,
        warnings: warnings(lines),
    })
}

/// Reads and parses `<worktree>/.portool.toml`. Returns `Ok(None)` when the
/// worktree has no manifest (it then gets a single `PORT` variable).
pub fn read_manifest(worktree: impl AsRef<Path>) -> Result<Option<Manifest>> {
    cmd::sync::load_manifest(worktree.as_ref())
}

/// A port allocation for one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Allocation {
    /// The allocated block of host ports, inclusive on both ends.
    pub block: (u16, u16),
    /// One `(manifest key, port)` per port declared in `.portool.toml`, in
    /// ascending offset order. Empty when there is no manifest.
    pub ports: Vec<(String, u16)>,
    /// The portool-managed variables, in `.env.portool` order:
    /// `PORTOOL_PROJECT_ID`, `PORTOOL_WORKTREE_ID`, then one `<KEY>_PORT`
    /// per declared port (or a single `PORT` without a manifest). The two
    /// ids are stable, so they are suitable for deriving Compose project
    /// and database names.
    pub vars: Vec<(String, String)>,
    /// The worktree's `.env.portool` file, which holds the same variables.
    pub env_file: PathBuf,
    /// Advisory warnings raised while allocating.
    pub warnings: Vec<Warning>,
}

/// The result of [`Worktree::exec_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecOutput {
    /// The variables to set on top of the command's inherited environment.
    pub vars: BTreeMap<String, String>,
    /// Advisory warnings (for example "ports are in use").
    pub warnings: Vec<Warning>,
}

/// The result of releasing a worktree's allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReleaseOutcome {
    /// What happened.
    pub released: Released,
    /// Advisory warnings raised while releasing.
    pub warnings: Vec<Warning>,
}

/// Whether and how [`Worktree::exec_env`] checks that the block's ports are
/// free before use. Mirrors `portool exec`'s `--check-ports`, `--strict`,
/// and `--reallocate-on-conflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PortCheck {
    /// No bind check (the CLI default).
    #[default]
    Off,
    /// Warn on standard error if a port is in use, then continue.
    Warn,
    /// Fail with an error if a port is in use.
    Strict,
    /// Move to a fresh block if a port is in use (processes already
    /// running keep the old ports).
    Reallocate,
}

/// Options for [`Worktree::exec_env`], mirroring `portool exec`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExecEnv {
    /// Env files to load, in order; later files override earlier ones. Each
    /// is a file path (relative to the process's current directory, or
    /// absolute), not a profile name.
    pub env_files: Vec<PathBuf>,
    /// Let `env_files` override inherited variables (`--env-file-overrides`).
    /// By default the inherited environment wins over env files.
    pub env_file_overrides: bool,
    /// The pre-use bind check.
    pub port_check: PortCheck,
    /// The inherited environment used for precedence and `${NAME}`
    /// expansion. `None` means this process's own environment, as for the
    /// CLI.
    pub inherited: Option<Vec<(String, String)>>,
}

/// What a release did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Released {
    /// The worktree's ledger entry was freed.
    Freed,
    /// The worktree had no allocation; nothing to free (still success).
    NothingToRelease,
}

/// Check whether this version can read the selected registry without creating,
/// repairing, or updating the ledger. A missing registry is valid.
pub fn check_registry_in(context: &Context) -> Result<()> {
    paths::scoped(context, || {
        store::load_strict(&paths::registry_path()?).map(|_| ())
    })
}

/// A git worktree that portool can allocate ports for.
#[derive(Debug, Clone)]
pub struct Worktree {
    ctx: GitCtx,
    context: Context,
}

impl Worktree {
    /// Opens the worktree containing `path` (any path inside it). Its
    /// methods find the ledger and config from the process environment at
    /// the time of each call ([`Context::process`]).
    ///
    /// Fails if `path` is not inside a git repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Worktree> {
        Worktree::open_in(&Context::process(), path)
    }

    /// [`Worktree::open`] with the ledger and config locations taken from
    /// `context`; every method of the returned handle uses them.
    pub fn open_in(context: &Context, path: impl AsRef<Path>) -> Result<Worktree> {
        Ok(Worktree {
            ctx: GitCtx::discover(path.as_ref())?,
            context: context.clone(),
        })
    }

    /// The worktree's root directory (canonicalized).
    pub fn root(&self) -> &Path {
        &self.ctx.worktree_root
    }

    /// The stable id of the project (shared by all its worktrees).
    pub fn project_id(&self) -> String {
        identity::project_id(&self.ctx.common_dir)
    }

    /// The stable id of this worktree.
    pub fn worktree_id(&self) -> String {
        identity::worktree_id(&self.ctx.common_dir, &self.ctx.worktree_root)
    }

    /// Reads this worktree's `.portool.toml`, if any.
    pub fn manifest(&self) -> Result<Option<Manifest>> {
        read_manifest(&self.ctx.worktree_root)
    }

    /// Allocates (or refreshes) this worktree's port block and writes its
    /// `.env.portool`, like `portool sync --quiet`. Idempotent: an existing
    /// up-to-date allocation is returned unchanged.
    pub fn allocate(&self) -> Result<Allocation> {
        let (outcome, lines) =
            warn::collect(|| paths::scoped(&self.context, || cmd::sync::ensure(&self.ctx, true)));
        self.allocation(outcome?, lines)
    }

    /// Forces this worktree onto a fresh, different block, like
    /// `portool reallocate --quiet`. Errors if it has no allocation yet.
    pub fn reallocate(&self) -> Result<Allocation> {
        let (outcome, lines) = warn::collect(|| {
            paths::scoped(&self.context, || cmd::sync::reallocate(&self.ctx, true))
        });
        self.allocation(outcome?, lines)
    }

    /// Composes the environment `portool exec` would give a command:
    /// syncs the allocation, loads the env files, applies precedence, and
    /// expands `${NAME}` references. Returns the variables to **set on top
    /// of** the command's inherited environment (env-file and
    /// portool-managed winners only).
    pub fn exec_env(&self, options: &ExecEnv) -> Result<ExecOutput> {
        let (check, strict, reallocate) = match options.port_check {
            PortCheck::Off => (false, false, false),
            PortCheck::Warn => (true, false, false),
            PortCheck::Strict => (false, true, false),
            PortCheck::Reallocate => (false, false, true),
        };
        let (vars, lines) = warn::collect(|| {
            paths::scoped(&self.context, || {
                cmd::exec::build_env(
                    &self.ctx,
                    &options.env_files,
                    check,
                    strict,
                    reallocate,
                    options.env_file_overrides,
                    options.inherited.clone(),
                )
            })
        });
        Ok(ExecOutput {
            vars: vars?,
            warnings: warnings(lines),
        })
    }

    /// Frees this worktree's allocation and removes its `.env.portool`,
    /// like `portool release`. Having no allocation is success
    /// ([`Released::NothingToRelease`]).
    pub fn release(&self) -> Result<ReleaseOutcome> {
        release_ctx(&self.context, &self.ctx)
    }

    fn allocation(
        &self,
        outcome: cmd::sync::SyncOutcome,
        lines: Vec<String>,
    ) -> Result<Allocation> {
        let vars = envfile::variables(
            outcome.block,
            outcome.manifest.as_ref(),
            &self.project_id(),
            &self.worktree_id(),
        )?;
        let ports = outcome
            .manifest
            .iter()
            .flat_map(|m| m.ports.iter())
            .map(|(key, offset)| (key.clone(), outcome.block.0 + offset))
            .collect();
        Ok(Allocation {
            block: outcome.block,
            ports,
            vars,
            env_file: self.ctx.worktree_root.join(".env.portool"),
            warnings: warnings(lines),
        })
    }
}

/// Releases the allocation of a worktree that may already have been
/// deleted from disk. `repo` is any live worktree of the same project
/// (typically the main checkout); `worktree` is the removed worktree's
/// path. Use [`Worktree::release`] while the worktree still exists.
///
/// The ledger is found from the process environment
/// ([`Context::process`]); see [`release_removed_in`].
pub fn release_removed(
    repo: impl AsRef<Path>,
    worktree: impl AsRef<Path>,
) -> Result<ReleaseOutcome> {
    release_removed_in(&Context::process(), repo, worktree)
}

/// [`release_removed`] with the ledger location taken from `context`.
pub fn release_removed_in(
    context: &Context,
    repo: impl AsRef<Path>,
    worktree: impl AsRef<Path>,
) -> Result<ReleaseOutcome> {
    let ctx = GitCtx::for_worktree_path(repo.as_ref(), worktree.as_ref())?;
    release_ctx(context, &ctx)
}

fn release_ctx(context: &Context, ctx: &GitCtx) -> Result<ReleaseOutcome> {
    let (freed, lines) =
        warn::collect(|| paths::scoped(context, || cmd::release::release_worktree(ctx)));
    Ok(ReleaseOutcome {
        released: if freed? {
            Released::Freed
        } else {
            Released::NothingToRelease
        },
        warnings: warnings(lines),
    })
}

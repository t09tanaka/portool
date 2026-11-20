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
//! `$XDG_CONFIG_HOME/portool/config.toml`. Those locations come from the
//! process environment on every call. Library and CLI calls -- in the same
//! process, in different processes, or both -- therefore serialize on the
//! same lock and can never be handed the same port block.
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
//! # let _ = env;
//!
//! // Free the allocation again.
//! wt.release()?;
//! # Ok(()) }
//! ```
//!
//! # Stability
//!
//! Only the items re-exported below are public API (semver applies);
//! everything else is a crate-private implementation detail. Like the CLI,
//! some operations print advisory warnings and hints to standard error.

mod alloc;
mod cli;
mod cmd;
mod config;
mod display;
mod envfile;
mod envread;
mod error;
mod fault;
mod gc;
mod gitctx;
mod hooks;
mod identity;
mod lock;
mod manifest;
mod paths;
mod ports;
mod registry;
mod store;

pub use config::Config;
pub use error::{Error, Result};
pub use manifest::Manifest;

use gitctx::GitCtx;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Entry point of the `portool` binary. Not part of the library API: it is
/// exempt from semver and may change or disappear in any release.
#[doc(hidden)]
pub fn __run_cli() -> i32 {
    cli::run()
}

/// Reads the global configuration (`config.toml`): the port pool and block
/// alignment. A missing file yields [`Config::default`]; a malformed one is
/// an error, exactly as for the CLI.
pub fn load_config() -> Result<Config> {
    Config::load()
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

/// What [`Worktree::release`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Released {
    /// The worktree's ledger entry was freed.
    Freed,
    /// The worktree had no allocation; nothing to free (still success).
    NothingToRelease,
}

/// A git worktree that portool can allocate ports for.
#[derive(Debug, Clone)]
pub struct Worktree {
    ctx: GitCtx,
}

impl Worktree {
    /// Opens the worktree containing `path` (any path inside it).
    ///
    /// Fails if `path` is not inside a git repository.
    pub fn open(path: impl AsRef<Path>) -> Result<Worktree> {
        Ok(Worktree {
            ctx: GitCtx::discover(path.as_ref())?,
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
        let outcome = cmd::sync::ensure(&self.ctx, true)?;
        self.allocation(outcome)
    }

    /// Forces this worktree onto a fresh, different block, like
    /// `portool reallocate --quiet`. Errors if it has no allocation yet.
    pub fn reallocate(&self) -> Result<Allocation> {
        let outcome = cmd::sync::reallocate(&self.ctx, true)?;
        self.allocation(outcome)
    }

    /// Composes the environment `portool exec` would give a command:
    /// syncs the allocation, loads the env files, applies precedence, and
    /// expands `${NAME}` references. Returns the variables to **set on top
    /// of** the command's inherited environment (env-file and
    /// portool-managed winners only).
    pub fn exec_env(&self, options: &ExecEnv) -> Result<BTreeMap<String, String>> {
        let (check, strict, reallocate) = match options.port_check {
            PortCheck::Off => (false, false, false),
            PortCheck::Warn => (true, false, false),
            PortCheck::Strict => (false, true, false),
            PortCheck::Reallocate => (false, false, true),
        };
        cmd::exec::build_env(
            &self.ctx,
            &options.env_files,
            check,
            strict,
            reallocate,
            options.env_file_overrides,
            options.inherited.clone(),
        )
    }

    /// Frees this worktree's allocation and removes its `.env.portool`,
    /// like `portool release`. Having no allocation is success
    /// ([`Released::NothingToRelease`]).
    pub fn release(&self) -> Result<Released> {
        released(cmd::release::release_worktree(&self.ctx)?)
    }

    fn allocation(&self, outcome: cmd::sync::SyncOutcome) -> Result<Allocation> {
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
        })
    }
}

/// Releases the allocation of a worktree that may already have been
/// deleted from disk. `repo` is any live worktree of the same project
/// (typically the main checkout); `worktree` is the removed worktree's
/// path. Use [`Worktree::release`] while the worktree still exists.
pub fn release_removed(repo: impl AsRef<Path>, worktree: impl AsRef<Path>) -> Result<Released> {
    let ctx = GitCtx::for_worktree_path(repo.as_ref(), worktree.as_ref())?;
    released(cmd::release::release_worktree(&ctx)?)
}

fn released(freed: bool) -> Result<Released> {
    Ok(if freed {
        Released::Freed
    } else {
        Released::NothingToRelease
    })
}

//! portool: a passive global port ledger for git worktrees.
//!
//! **The only stable interface is the `portool` CLI.** Every module below
//! is an internal implementation detail, exposed solely so the binary and
//! this repository's integration tests can reach it; none of it follows
//! semver, and it is hidden from rustdoc accordingly. Depend on the CLI's
//! documented commands, exit codes, and file formats instead.

#[doc(hidden)]
pub mod alloc;
#[doc(hidden)]
pub mod cmd;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod envfile;
#[doc(hidden)]
pub mod envread;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod gc;
#[doc(hidden)]
pub mod gitctx;
#[doc(hidden)]
pub mod hooks;
#[doc(hidden)]
pub mod identity;
#[doc(hidden)]
pub mod lock;
#[doc(hidden)]
pub mod manifest;
#[doc(hidden)]
pub mod paths;
#[doc(hidden)]
pub mod ports;
#[doc(hidden)]
pub mod registry;
#[doc(hidden)]
pub mod store;

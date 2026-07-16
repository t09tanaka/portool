//! portool: a passive global port ledger for git worktrees.
//!
//! This crate is organized so that the allocation core is entirely
//! I/O-free and unit-testable in isolation; I/O and CLI plumbing are added
//! in later modules layered on top.

pub mod alloc;
pub mod cmd;
pub mod config;
pub mod envfile;
pub mod error;
pub mod gc;
pub mod gitctx;
pub mod identity;
pub mod lock;
pub mod manifest;
pub mod paths;
pub mod ports;
pub mod registry;
pub mod store;

//! CLI command implementations (spec §7-§9). `main.rs` is limited to `clap`
//! parsing, dispatch to these modules, and exit-code mapping.

pub mod check;
pub mod doctor;
pub mod exec;
pub mod init;
pub mod ls;
pub mod pin;
pub mod prune;
pub mod release;
pub mod reserve;
pub mod sync;

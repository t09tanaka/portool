//! CLI command implementations (spec §7-§9). `main.rs` is limited to `clap`
//! parsing, dispatch to these modules, and exit-code mapping.

pub mod init;
pub mod ls;
pub mod prune;
pub mod sync;

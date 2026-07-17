//! Global configuration (`config.toml`) parsing and defaults.

use crate::error::{Error, Result};
use serde::Deserialize;

/// Global portool configuration.
///
/// All fields are optional in the TOML source; any field left unspecified
/// falls back to [`Config::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The full port pool, inclusive on both ends. Worktree blocks are
    /// allocated directly from this pool (hardening batch C: the old
    /// per-project 500-wide subrange model is gone).
    pub range: (u16, u16),
    /// Block size rounding unit (and minimum block size).
    pub block_align: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            range: (3000, 9999),
            block_align: 5,
        }
    }
}

/// Mirrors the TOML schema of `config.toml`; every field is optional so
/// that partial configuration files are accepted. `deny_unknown_fields`
/// makes a typo (`ragne = …`) a hard error rather than a silently-ignored
/// field. `subrange_size` and `gc_days` are retained here only as
/// **deprecated, ignored** fields so that a legacy config that still sets
/// them keeps working (rather than tripping `deny_unknown_fields`); neither
/// affects allocation any more (GC is condition-based: gone directory + free
/// ports).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    range: Option<(u16, u16)>,
    subrange_size: Option<u16>,
    block_align: Option<u16>,
    gc_days: Option<u32>,
}

impl Config {
    /// Parses a `config.toml` source string, filling any unspecified field
    /// with the corresponding [`Config::default`] value.
    ///
    /// Returns an error if the TOML is malformed, if there is an unknown
    /// field, or if `range` is reversed (start > end) or `block_align` is
    /// zero. A still-present `subrange_size` is accepted but ignored with a
    /// one-line deprecation warning.
    pub fn from_toml_str(s: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(s)?;
        let default = Config::default();

        if raw.subrange_size.is_some() {
            eprintln!(
                "portool: warning: config `subrange_size` is deprecated and ignored \
                 (blocks are now allocated directly from `range`)"
            );
        }
        if raw.gc_days.is_some() {
            eprintln!(
                "portool: warning: config `gc_days` is deprecated and ignored \
                 (GC is condition-based: gone directory + free ports)"
            );
        }

        let range = raw.range.unwrap_or(default.range);
        let block_align = raw.block_align.unwrap_or(default.block_align);

        if range.0 > range.1 {
            return Err(Error::General(format!(
                "invalid config: range is reversed ([{}, {}])",
                range.0, range.1
            )));
        }
        if range.0 == 0 {
            // Port 0 is never a real allocation; bind(0) asks the OS for an
            // ephemeral port, which portool would misread as "0 is free"
            // (batch D #16).
            return Err(Error::General(
                "invalid config: range must not include port 0".into(),
            ));
        }
        if block_align == 0 {
            return Err(Error::General(
                "invalid config: block_align must not be zero".into(),
            ));
        }

        Ok(Config { range, block_align })
    }

    /// Loads the global config from [`crate::paths::config_path`].
    ///
    /// A missing file means [`Config::default`] (that is intentional, not a
    /// failure). Every other outcome is **fail-closed**: a read error, a
    /// parse error, an unknown field, or an invalid value is a hard error
    /// (exit 1). A malformed config must never silently fall back to
    /// defaults -- e.g. a broken `range = …` line reverting to `3000..=9999`
    /// under the user's feet is exactly the surprise this guards against.
    pub fn load() -> Result<Config> {
        let path = crate::paths::config_path()?;
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(err) => {
                return Err(Error::General(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };

        Config::from_toml_str(&contents)
            .map_err(|err| Error::General(format!("failed to parse {}: {err}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_global_constraints() {
        let cfg = Config::default();
        assert_eq!(cfg.range, (3000, 9999));
        assert_eq!(cfg.block_align, 5);
    }

    #[test]
    fn empty_source_yields_default() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_source_fills_defaults() {
        let cfg = Config::from_toml_str("block_align = 10\n").unwrap();
        assert_eq!(cfg.block_align, 10);
        assert_eq!(cfg.range, Config::default().range);
    }

    #[test]
    fn full_source_is_honored() {
        let cfg = Config::from_toml_str("range = [4000, 5000]\nblock_align = 4\n").unwrap();
        assert_eq!(
            cfg,
            Config {
                range: (4000, 5000),
                block_align: 4,
            }
        );
    }

    #[test]
    fn deprecated_gc_days_is_accepted_and_ignored() {
        // A legacy config that still sets gc_days must keep working
        // (accepted + ignored), not trip deny_unknown_fields.
        let cfg = Config::from_toml_str("gc_days = 7\nblock_align = 4\n").unwrap();
        assert_eq!(cfg.block_align, 4);
        assert_eq!(cfg.range, Config::default().range);
    }

    #[test]
    fn reversed_range_is_error() {
        let err = Config::from_toml_str("range = [9999, 3000]\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn equal_range_bounds_are_allowed() {
        let cfg = Config::from_toml_str("range = [3000, 3000]\n").unwrap();
        assert_eq!(cfg.range, (3000, 3000));
    }

    #[test]
    fn deprecated_subrange_size_is_accepted_and_ignored() {
        // A legacy config that still sets subrange_size must keep working
        // (accepted + ignored), not trip deny_unknown_fields.
        let cfg = Config::from_toml_str("subrange_size = 500\nblock_align = 4\n").unwrap();
        assert_eq!(cfg.block_align, 4);
        assert_eq!(cfg.range, Config::default().range);
    }

    #[test]
    fn zero_block_align_is_error() {
        let err = Config::from_toml_str("block_align = 0\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn malformed_toml_is_error() {
        let err = Config::from_toml_str("this is not valid toml =====").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // A typo like `ragne` must be a hard error, not silently ignored.
        let err = Config::from_toml_str("ragne = [4000, 5000]\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn range_including_port_zero_is_error() {
        let err = Config::from_toml_str("range = [0, 9999]\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }
}

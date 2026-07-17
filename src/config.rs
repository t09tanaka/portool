//! Global configuration (`config.toml`) parsing and defaults.

use crate::error::{Error, Result};
use serde::Deserialize;

/// Global portool configuration.
///
/// All fields are optional in the TOML source; any field left unspecified
/// falls back to [`Config::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The full port pool, inclusive on both ends.
    pub range: (u16, u16),
    /// Width of a subrange allocated to a project on first use.
    pub subrange_size: u16,
    /// Block size rounding unit (and minimum block size).
    pub block_align: u16,
    /// Age threshold (in days) used by cross-project GC (v0.2+).
    pub gc_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            range: (3000, 9999),
            subrange_size: 500,
            block_align: 5,
            gc_days: 30,
        }
    }
}

/// Mirrors the TOML schema of `config.toml`; every field is optional so
/// that partial configuration files are accepted. `deny_unknown_fields`
/// makes a typo (`ragne = …`) a hard error rather than a silently-ignored
/// field.
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
    /// Returns an error if the TOML is malformed, or if the resulting
    /// values are invalid: `range` reversed (start > end), `subrange_size`
    /// zero, or `block_align` zero.
    pub fn from_toml_str(s: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(s)?;
        let default = Config::default();

        let range = raw.range.unwrap_or(default.range);
        let subrange_size = raw.subrange_size.unwrap_or(default.subrange_size);
        let block_align = raw.block_align.unwrap_or(default.block_align);
        let gc_days = raw.gc_days.unwrap_or(default.gc_days);

        if range.0 > range.1 {
            return Err(Error::General(format!(
                "invalid config: range is reversed ([{}, {}])",
                range.0, range.1
            )));
        }
        if subrange_size == 0 {
            return Err(Error::General(
                "invalid config: subrange_size must not be zero".into(),
            ));
        }
        if block_align == 0 {
            return Err(Error::General(
                "invalid config: block_align must not be zero".into(),
            ));
        }

        Ok(Config {
            range,
            subrange_size,
            block_align,
            gc_days,
        })
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
        assert_eq!(cfg.subrange_size, 500);
        assert_eq!(cfg.block_align, 5);
        assert_eq!(cfg.gc_days, 30);
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
        assert_eq!(cfg.subrange_size, Config::default().subrange_size);
        assert_eq!(cfg.gc_days, Config::default().gc_days);
    }

    #[test]
    fn full_source_is_honored() {
        let cfg = Config::from_toml_str(
            "range = [4000, 5000]\nsubrange_size = 100\nblock_align = 4\ngc_days = 7\n",
        )
        .unwrap();
        assert_eq!(
            cfg,
            Config {
                range: (4000, 5000),
                subrange_size: 100,
                block_align: 4,
                gc_days: 7,
            }
        );
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
    fn zero_subrange_size_is_error() {
        let err = Config::from_toml_str("subrange_size = 0\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
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
}

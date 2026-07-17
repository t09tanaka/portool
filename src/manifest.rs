//! `.portool.toml` manifest parsing and the derived block-sizing rules.

use crate::error::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

/// A parsed `.portool.toml` manifest: the declared `(key, offset)` pairs,
/// normalized to ascending offset order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub ports: Vec<(String, u16)>,
}

/// Mirrors the on-disk shape of `.portool.toml`. Offset values are read as
/// raw TOML values so that non-integer / negative values can be rejected
/// with a clear error rather than a generic deserialization failure.
#[derive(Debug, Deserialize)]
struct RawManifest {
    ports: BTreeMap<String, toml::Value>,
}

impl Manifest {
    /// Parses a manifest source string.
    ///
    /// The `[ports]` table is required. Keys must match
    /// `^[a-z][a-z0-9_]*$`, offsets must be non-negative integers that fit
    /// in a `u16`, and offsets must be unique.
    pub fn parse(s: &str) -> Result<Manifest> {
        let raw: RawManifest =
            toml::from_str(s).map_err(|e| Error::General(format!("invalid manifest: {e}")))?;

        let mut ports = Vec::with_capacity(raw.ports.len());
        let mut seen_offsets = HashSet::with_capacity(raw.ports.len());

        for (key, value) in raw.ports {
            if !is_valid_key(&key) {
                return Err(Error::General(format!(
                    "invalid manifest: port key '{key}' must match ^[a-z][a-z0-9_]*$"
                )));
            }

            let offset = value.as_integer().ok_or_else(|| {
                Error::General(format!(
                    "invalid manifest: offset for '{key}' must be an integer"
                ))
            })?;
            if offset < 0 {
                return Err(Error::General(format!(
                    "invalid manifest: offset for '{key}' must not be negative"
                )));
            }
            let offset = u16::try_from(offset).map_err(|_| {
                Error::General(format!(
                    "invalid manifest: offset for '{key}' is out of range"
                ))
            })?;

            if !seen_offsets.insert(offset) {
                return Err(Error::General(format!(
                    "invalid manifest: duplicate offset {offset}"
                )));
            }

            ports.push((key, offset));
        }

        ports.sort_by_key(|(_, offset)| *offset);
        Ok(Manifest { ports })
    }

    /// The block size implied by this manifest: `max(max offset + 1,
    /// declared count)`, rounded up to a multiple of `block_align`.
    ///
    /// A manifest whose (aligned) size exceeds `u16::MAX` is rejected
    /// rather than clamped (external review P2 #6): clamping would silently
    /// hand two declared offsets the same port.
    pub fn block_size(&self, block_align: u16) -> Result<u16> {
        let max_offset_plus_one = self
            .ports
            .iter()
            .map(|(_, offset)| u32::from(*offset) + 1)
            .max()
            .unwrap_or(0);
        let count = self.ports.len() as u32;
        let raw = max_offset_plus_one.max(count).max(1);

        let align = u32::from(block_align.max(1));
        let aligned = raw.div_ceil(align) * align;
        u16::try_from(aligned).map_err(|_| {
            Error::General(format!(
                "invalid manifest: the declared ports need a {aligned}-port block, \
                 which cannot fit below port 65535"
            ))
        })
    }
}

/// The block size used when a project has no manifest at all: exactly one
/// `block_align`-wide block, exposed as a single `PORT` variable.
pub fn default_block_size(block_align: u16) -> u16 {
    block_align
}

/// Derives the environment variable name for a manifest key: upper-cased
/// plus a `_PORT` suffix (`web` -> `WEB_PORT`).
pub fn env_var_name(key: &str) -> String {
    format!("{}_PORT", key.to_uppercase())
}

/// The SHA-256 hex digest of `bytes`, truncated to its first 12 characters.
pub fn manifest_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let digest_bytes: &[u8] = digest.as_ref();
    let mut hex = String::with_capacity(digest_bytes.len() * 2);
    for byte in digest_bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(12);
    hex
}

fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_offset_order() {
        let m = Manifest::parse("[ports]\ndb = 3\nweb = 0\napi = 1\nhmr = 2\n").unwrap();
        assert_eq!(
            m.ports,
            vec![
                ("web".to_string(), 0),
                ("api".to_string(), 1),
                ("hmr".to_string(), 2),
                ("db".to_string(), 3),
            ]
        );
    }

    #[test]
    fn allows_sparse_offsets() {
        let m = Manifest::parse("[ports]\nweb = 0\napi = 3\n").unwrap();
        assert_eq!(
            m.ports,
            vec![("web".to_string(), 0), ("api".to_string(), 3)]
        );
    }

    #[test]
    fn missing_ports_table_is_error() {
        let err = Manifest::parse("").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn empty_ports_table_sizes_to_the_alignment_minimum() {
        let m = Manifest::parse("[ports]\n").unwrap();
        assert!(m.ports.is_empty());
        assert_eq!(m.block_size(5).unwrap(), 5);
    }

    #[test]
    fn rejects_invalid_key_uppercase() {
        let err = Manifest::parse("[ports]\nWeb = 0\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn rejects_invalid_key_leading_digit() {
        let err = Manifest::parse("[ports]\n1web = 0\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn rejects_duplicate_offsets() {
        let err = Manifest::parse("[ports]\nweb = 0\napi = 0\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn rejects_negative_offset() {
        let err = Manifest::parse("[ports]\nweb = -1\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn rejects_non_integer_offset() {
        let err = Manifest::parse("[ports]\nweb = 1.5\n").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn block_size_rounds_4_to_5() {
        // 4 declared ports, max offset 3 -> raw 4, rounds up to align 5.
        let m = Manifest::parse("[ports]\na = 0\nb = 1\nc = 2\nd = 3\n").unwrap();
        assert_eq!(m.block_size(5).unwrap(), 5);
    }

    #[test]
    fn block_size_rounds_5_to_5() {
        // 5 declared ports, max offset 4 -> raw 5, already a multiple of 5.
        let m = Manifest::parse("[ports]\na = 0\nb = 1\nc = 2\nd = 3\ne = 4\n").unwrap();
        assert_eq!(m.block_size(5).unwrap(), 5);
    }

    #[test]
    fn block_size_rounds_6_to_10() {
        // 6 declared ports, max offset 5 -> raw 6, rounds up to 10.
        let m = Manifest::parse("[ports]\na = 0\nb = 1\nc = 2\nd = 3\ne = 4\nf = 5\n").unwrap();
        assert_eq!(m.block_size(5).unwrap(), 10);
    }

    #[test]
    fn block_size_at_the_u16_ceiling_is_accepted() {
        // Max offset 65534 -> raw 65535, exactly representable with align 1.
        let m = Manifest::parse("[ports]\na = 65534\n").unwrap();
        assert_eq!(m.block_size(1).unwrap(), u16::MAX);
    }

    #[test]
    fn block_size_rejects_an_unrepresentable_manifest() {
        // Max offset 65535 -> raw 65536 > u16::MAX: rejecting beats the old
        // clamp, under which two offsets would saturate to the same port.
        let m = Manifest::parse("[ports]\na = 65534\nb = 65535\n").unwrap();
        let err = m.block_size(1).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("invalid manifest"));

        // Alignment overflow is rejected too: raw 65534 aligns up to 65540.
        let m = Manifest::parse("[ports]\na = 65533\n").unwrap();
        assert!(m.block_size(10).is_err());
    }

    #[test]
    fn default_block_size_equals_align() {
        assert_eq!(default_block_size(5), 5);
        assert_eq!(default_block_size(10), 10);
    }

    #[test]
    fn env_var_name_upper_cases_and_suffixes() {
        assert_eq!(env_var_name("web"), "WEB_PORT");
        assert_eq!(env_var_name("hmr"), "HMR_PORT");
    }

    #[test]
    fn manifest_hash_known_vector() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(manifest_hash(b""), "e3b0c44298fc");
    }

    #[test]
    fn manifest_hash_is_twelve_hex_chars() {
        let hash = manifest_hash(b"[ports]\nweb = 0\n");
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

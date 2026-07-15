//! Stable worktree identity derivation (spec-v0.2 §5): the
//! `PORTOOL_PROJECT_ID` / `PORTOOL_WORKTREE_ID` values written into
//! `.env.portool`. Both are pure functions of the canonicalized
//! git-common-dir (and worktree root), so they survive branch switches,
//! detached HEAD, and ledger loss.

use sha2::{Digest, Sha256};
use std::path::Path;

const PROJECT_ID_DOMAIN: &[u8] = b"portool-project-id-v1\0";
const WORKTREE_ID_DOMAIN: &[u8] = b"portool-worktree-id-v1\0";
const ID_LEN: usize = 16;

/// The stable project identifier: the first 16 lowercase-hex characters of
/// `SHA-256("portool-project-id-v1\0" + raw_bytes(common_dir))`. Identical
/// across every worktree of the same project.
pub fn project_id(common_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PROJECT_ID_DOMAIN);
    hasher.update(path_bytes(common_dir));
    truncated_hex(&hasher.finalize())
}

/// The stable worktree identifier: the first 16 lowercase-hex characters of
/// `SHA-256("portool-worktree-id-v1\0" + raw_bytes(common_dir) + "\0" +
/// raw_bytes(worktree_root))`. Unique per worktree, stable across branch
/// checkouts.
pub fn worktree_id(common_dir: &Path, worktree_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(WORKTREE_ID_DOMAIN);
    hasher.update(path_bytes(common_dir));
    hasher.update(b"\0");
    hasher.update(path_bytes(worktree_root));
    truncated_hex(&hasher.finalize())
}

/// The raw bytes of a path: on Unix the exact OS bytes (correct even for
/// non-UTF-8 paths), elsewhere a lossy UTF-8 rendering.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn truncated_hex(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(ID_LEN);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_lowercase_hex_16(s: &str) -> bool {
        s.len() == 16
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    }

    #[test]
    fn project_id_matches_known_vector() {
        assert_eq!(
            project_id(Path::new("/home/user/dev/myapp/.git")),
            "abe4c983b7295f37"
        );
    }

    #[test]
    fn worktree_id_matches_known_vectors() {
        assert_eq!(
            worktree_id(
                Path::new("/home/user/dev/myapp/.git"),
                Path::new("/home/user/dev/myapp")
            ),
            "740da24128c5e2f7"
        );
        assert_eq!(
            worktree_id(
                Path::new("/home/user/dev/myapp/.git"),
                Path::new("/home/user/dev/myapp-wt/feat-api")
            ),
            "39c020aea86d3272"
        );
    }

    #[test]
    fn different_worktree_paths_yield_different_worktree_ids() {
        let common = Path::new("/home/user/dev/myapp/.git");
        assert_ne!(
            worktree_id(common, Path::new("/home/user/dev/myapp")),
            worktree_id(common, Path::new("/home/user/dev/myapp-wt/feat-api"))
        );
    }

    #[test]
    fn different_common_dirs_yield_different_project_ids() {
        assert_ne!(
            project_id(Path::new("/home/user/dev/myapp/.git")),
            project_id(Path::new("/home/user/dev/other/.git"))
        );
    }

    #[test]
    fn ids_are_always_16_lowercase_hex_chars() {
        let common = Path::new("/home/user/dev/myapp/.git");
        let root = Path::new("/home/user/dev/myapp");
        assert!(is_lowercase_hex_16(&project_id(common)));
        assert!(is_lowercase_hex_16(&worktree_id(common, root)));
    }
}

//! Pure allocation algorithm: FNV-1a hashing and block allocation directly
//! from the pool (hardening batch C -- the old per-project subrange model is
//! gone). No I/O; bind checks are injected via closure.

use crate::registry::overlaps;

const FNV_OFFSET_BASIS: u32 = 2_166_136_261;
const FNV_PRIME: u32 = 16_777_619;

/// The 32-bit FNV-1a hash of `bytes`.
pub fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The preferred candidate slot index for a worktree, in `[0, num_slots)`.
///
/// Hashes `project_key` together with the branch (or, for detached HEAD,
/// the worktree path): stable per (project, branch), so re-creating a
/// worktree on the same branch tends to return to the same block, while
/// different projects' identically named branches spread across the pool
/// instead of piling onto one hotspot (external review P1-8). This is a
/// preference, not a reservation: the caller scans forward from here.
pub fn preferred_slot(
    project_key: &str,
    branch: Option<&str>,
    worktree_path: &str,
    num_slots: u32,
) -> u32 {
    if num_slots == 0 {
        return 0;
    }
    let discriminator = branch.unwrap_or(worktree_path);
    let seed = format!("{project_key}\n{discriminator}");
    fnv1a_32(seed.as_bytes()) % num_slots
}

/// Allocates a `block_size`-wide block for a worktree directly from `pool`
/// (hardening batch C).
///
/// Candidate block starts are the `block_align`-aligned positions
/// `pool.0 + k*block_align` that keep the whole block within `pool`. Starting
/// from [`preferred_slot`] and wrapping, the first candidate that overlaps
/// nothing in `occupied` and that `bind_ok` reports as bindable is returned.
/// `occupied` is every block already recorded anywhere in the ledger (plus
/// reservations); passing a finer `block_align` than `block_size` is fine --
/// the overlap check prevents blocks of differing sizes from colliding.
/// Returns `None` when every candidate is taken (the pool is exhausted).
#[allow(clippy::too_many_arguments)] // project_key (task 8) pushed this past 7; splitting the
                                     // pool/size/align/key/branch/path/occupied/bind_ok group
                                     // into a struct is not worth it for one call site.
pub fn allocate_block(
    pool: (u16, u16),
    block_size: u16,
    block_align: u16,
    project_key: &str,
    branch: Option<&str>,
    worktree_path: &str,
    occupied: &[(u16, u16)],
    bind_ok: &mut dyn FnMut((u16, u16)) -> bool,
) -> Option<(u16, u16)> {
    if block_size == 0 {
        return None;
    }
    let align = u32::from(block_align.max(1));
    let pool_start = u32::from(pool.0);
    let pool_end = u32::from(pool.1);
    let size = u32::from(block_size);

    // The block can't fit in the pool at all.
    if pool_start + size - 1 > pool_end {
        return None;
    }

    // Number of aligned positions where the whole block still fits.
    let num_slots = (pool_end - pool_start + 1 - size) / align + 1;
    let preferred = preferred_slot(project_key, branch, worktree_path, num_slots);

    for i in 0..num_slots {
        let slot = (preferred + i) % num_slots;
        let start = pool_start + slot * align;
        let end = start + size - 1;
        // `end <= pool_end` holds by the `num_slots` bound above.
        let candidate = (start as u16, end as u16);

        if occupied.iter().any(|&o| overlaps(o, candidate)) {
            continue;
        }
        if bind_ok(candidate) {
            return Some(candidate);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_free() -> impl FnMut((u16, u16)) -> bool {
        |_block: (u16, u16)| true
    }

    #[test]
    fn fnv1a_32_known_vectors() {
        assert_eq!(fnv1a_32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a_32(b"a"), 0xe40c_292c);
    }

    #[test]
    fn preferred_slot_hashes_project_and_branch() {
        let slots = 97;
        let expected = fnv1a_32(b"/p/.git\nfeature/api") % slots;
        assert_eq!(
            preferred_slot("/p/.git", Some("feature/api"), "/w", slots),
            expected
        );
        // Same branch, different project -> (almost surely) different slot,
        // and deterministically NOT computed from the branch alone.
        assert_ne!(
            preferred_slot("/p/.git", Some("feature/api"), "/w", slots),
            fnv1a_32(b"feature/api") % slots,
        );
    }

    #[test]
    fn preferred_slot_main_is_hashed_like_any_branch() {
        let slots = 97;
        let expected = fnv1a_32(b"/p/.git\nmain") % slots;
        assert_eq!(
            preferred_slot("/p/.git", Some("main"), "/w", slots),
            expected
        );
    }

    #[test]
    fn preferred_slot_detached_uses_project_and_path() {
        let slots = 97;
        let expected = fnv1a_32(b"/p/.git\n/home/dev/wt") % slots;
        assert_eq!(
            preferred_slot("/p/.git", None, "/home/dev/wt", slots),
            expected
        );
    }

    /// The block width for a 3000-9999 pool, 5-wide blocks, 5-wide align:
    /// `(9999 - 3000 + 1 - 5) / 5 + 1` aligned positions.
    fn default_num_slots(block_size: u16, align: u16) -> u32 {
        (u32::from(9999u16) - u32::from(3000u16) + 1 - u32::from(block_size)) / u32::from(align) + 1
    }

    #[test]
    fn allocate_block_is_deterministic_and_lands_in_pool() {
        let mut bind_ok = always_free();
        let block = allocate_block(
            (3000, 9999),
            5,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &[],
            &mut bind_ok,
        )
        .unwrap();
        assert!(
            block.0 >= 3000 && block.1 <= 9999,
            "block {block:?} must be within the pool"
        );
        assert_eq!(block.1 - block.0, 4, "block must be 5-wide");

        // Same inputs, freshly computed -> same block (P1-8: re-syncing the
        // same project+branch must return to the same block).
        let mut bind_ok2 = always_free();
        let block2 = allocate_block(
            (3000, 9999),
            5,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &[],
            &mut bind_ok2,
        )
        .unwrap();
        assert_eq!(
            block, block2,
            "same inputs must deterministically return the same block"
        );
    }

    #[test]
    fn allocate_block_uses_the_full_pool_not_a_500_wide_subrange() {
        // A 600-wide block (larger than the old default subrange) fits fine.
        let mut bind_ok = always_free();
        let block = allocate_block(
            (3000, 9999),
            600,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &[],
            &mut bind_ok,
        )
        .unwrap();
        assert_eq!(block.1 - block.0, 599);
        assert!(block.0 >= 3000 && block.1 <= 9999);
    }

    #[test]
    fn allocate_block_skips_occupied_preferred_slot() {
        let pool = (3000, 9999);
        let project_key = "/p/.git";
        let branch = Some("feature/api");
        let worktree_path = "/w";
        let num_slots = default_num_slots(5, 5);
        let preferred = preferred_slot(project_key, branch, worktree_path, num_slots);
        let preferred_start = pool.0 + (preferred * 5) as u16;
        let occupied = [(preferred_start, preferred_start + 4)];

        let mut bind_ok = always_free();
        let block = allocate_block(
            pool,
            5,
            5,
            project_key,
            branch,
            worktree_path,
            &occupied,
            &mut bind_ok,
        )
        .unwrap();
        assert_ne!(
            block, occupied[0],
            "must scan forward past the occupied preferred slot"
        );
        assert!(!overlaps(block, occupied[0]));
    }

    #[test]
    fn allocate_block_skips_a_slot_that_fails_bind() {
        let pool = (3000, 9999);
        let project_key = "/p/.git";
        let branch = Some("feature/api");
        let worktree_path = "/w";
        let num_slots = default_num_slots(5, 5);
        let preferred = preferred_slot(project_key, branch, worktree_path, num_slots);
        let preferred_start = pool.0 + (preferred * 5) as u16;
        let preferred_block = (preferred_start, preferred_start + 4);

        let mut bind_ok = |block: (u16, u16)| block != preferred_block;
        let block = allocate_block(
            pool,
            5,
            5,
            project_key,
            branch,
            worktree_path,
            &[],
            &mut bind_ok,
        )
        .unwrap();
        assert_ne!(
            block, preferred_block,
            "must scan past a slot that fails the bind check"
        );
    }

    #[test]
    fn allocate_block_returns_none_when_pool_is_full() {
        let occupied = [(3000, 3004), (3005, 3009)];
        let mut bind_ok = always_free();
        let block = allocate_block(
            (3000, 3009),
            5,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &occupied,
            &mut bind_ok,
        );
        assert_eq!(block, None);
    }

    #[test]
    fn allocate_block_returns_none_when_block_larger_than_pool() {
        let mut bind_ok = always_free();
        let block = allocate_block(
            (3000, 3003),
            5,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &[],
            &mut bind_ok,
        );
        assert_eq!(block, None);
    }

    #[test]
    fn allocate_block_packs_heterogeneous_sizes_without_overlap() {
        // A 10-wide block sits at the pool start; a 5-wide request must
        // land clear of it (finer align than the existing block size).
        let occupied = [(3000, 3009)];
        let mut bind_ok = always_free();
        let block = allocate_block(
            (3000, 3019),
            5,
            5,
            "/p/.git",
            Some("main"),
            "/w",
            &occupied,
            &mut bind_ok,
        )
        .unwrap();
        assert!(!overlaps(block, (3000, 3009)));
    }
}

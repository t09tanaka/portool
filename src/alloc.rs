//! Pure allocation algorithm: FNV-1a hashing, subrange scanning, and block
//! allocation (spec §6). No I/O; bind checks are injected via closure.

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

/// Scans `pool` from its start for the first `size`-wide inclusive interval
/// that does not overlap any interval in `occupied`. Returns `None` if no
/// such interval fits within `pool`.
pub fn find_free_subrange(
    pool: (u16, u16),
    occupied: &[(u16, u16)],
    size: u16,
) -> Option<(u16, u16)> {
    if size == 0 {
        return None;
    }

    let pool_start = u32::from(pool.0);
    let pool_end = u32::from(pool.1);
    let size = u32::from(size);

    let mut occ: Vec<(u32, u32)> = occupied
        .iter()
        .map(|&(s, e)| (u32::from(s), u32::from(e)))
        .collect();
    occ.sort_unstable_by_key(|&(s, _)| s);

    let mut candidate_start = pool_start;
    for (s, e) in occ {
        if candidate_start + size - 1 < s {
            return Some((candidate_start as u16, (candidate_start + size - 1) as u16));
        }
        if e >= candidate_start {
            candidate_start = e + 1;
        }
        if candidate_start > pool_end {
            return None;
        }
    }

    if candidate_start + size - 1 <= pool_end {
        Some((candidate_start as u16, (candidate_start + size - 1) as u16))
    } else {
        None
    }
}

/// The preferred slot index for a worktree: slot 0 for `main`/`master`,
/// otherwise `FNV-1a-32(branch ?? worktree_path) % slots`.
pub fn preferred_slot(branch: Option<&str>, worktree_path: &str, slots: u32) -> u32 {
    if slots == 0 {
        return 0;
    }
    match branch {
        Some(b) if b == "main" || b == "master" => 0,
        Some(b) => fnv1a_32(b.as_bytes()) % slots,
        None => fnv1a_32(worktree_path.as_bytes()) % slots,
    }
}

/// Allocates a block for a worktree from `subranges` (spec §6.3, frozen
/// decision 3).
///
/// `subranges` is scanned in array order. Within each subrange,
/// `slots = floor(width / block_size)` candidate slots are considered
/// (subranges too narrow for even one block are skipped), starting from
/// [`preferred_slot`] and wrapping around modulo `slots`. A candidate slot
/// is accepted if it does not overlap any interval in `occupied` and
/// `bind_ok` reports the block as bindable. Returns `None` if every
/// subrange is exhausted.
pub fn allocate_block(
    subranges: &[(u16, u16)],
    block_size: u16,
    branch: Option<&str>,
    worktree_path: &str,
    occupied: &[(u16, u16)],
    bind_ok: &mut dyn FnMut((u16, u16)) -> bool,
) -> Option<(u16, u16)> {
    if block_size == 0 {
        return None;
    }
    let block_size = u32::from(block_size);

    for &(sub_start, sub_end) in subranges {
        let width = u32::from(sub_end) - u32::from(sub_start) + 1;
        let slots = width / block_size;
        if slots == 0 {
            continue;
        }

        let preferred = preferred_slot(branch, worktree_path, slots);
        for i in 0..slots {
            let slot = (preferred + i) % slots;
            let start = u32::from(sub_start) + slot * block_size;
            let end = start + block_size - 1;
            let candidate = (start as u16, end as u16);

            if occupied.iter().any(|&o| overlaps(o, candidate)) {
                continue;
            }
            if bind_ok(candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_32_known_vectors() {
        assert_eq!(fnv1a_32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a_32(b"a"), 0xe40c_292c);
    }

    #[test]
    fn find_free_subrange_returns_pool_start_when_empty() {
        let found = find_free_subrange((3000, 9999), &[], 500);
        assert_eq!(found, Some((3000, 3499)));
    }

    #[test]
    fn find_free_subrange_fills_gap_between_occupied_ranges() {
        let occupied = [(3000, 3499), (4000, 4499)];
        let found = find_free_subrange((3000, 9999), &occupied, 500);
        assert_eq!(found, Some((3500, 3999)));
    }

    #[test]
    fn find_free_subrange_returns_none_when_pool_exhausted() {
        let occupied = [(3000, 3999)];
        let found = find_free_subrange((3000, 3999), &occupied, 500);
        assert_eq!(found, None);
    }

    #[test]
    fn find_free_subrange_ignores_unsorted_and_unrelated_occupied() {
        let occupied = [(9000, 9999), (3000, 3499)];
        let found = find_free_subrange((3000, 9999), &occupied, 500);
        assert_eq!(found, Some((3500, 3999)));
    }

    #[test]
    fn preferred_slot_main_and_master_are_zero() {
        assert_eq!(preferred_slot(Some("main"), "/whatever", 8), 0);
        assert_eq!(preferred_slot(Some("master"), "/whatever", 8), 0);
    }

    #[test]
    fn preferred_slot_feature_branch_matches_fnv_formula() {
        let slots = 7;
        let expected = fnv1a_32(b"feature/api") % slots;
        assert_eq!(
            preferred_slot(Some("feature/api"), "/anything", slots),
            expected
        );
    }

    #[test]
    fn preferred_slot_distributes_across_branches() {
        let slots = 1000;
        let a = preferred_slot(Some("feature/alpha"), "/x", slots);
        let b = preferred_slot(Some("feature/beta"), "/x", slots);
        assert_ne!(a, b);
    }

    #[test]
    fn preferred_slot_detached_head_uses_worktree_path() {
        let slots = 8;
        let expected = fnv1a_32(b"/home/takuto/dev/esimdb-wt/detached") % slots;
        assert_eq!(
            preferred_slot(None, "/home/takuto/dev/esimdb-wt/detached", slots),
            expected
        );
    }

    #[test]
    fn allocate_block_picks_preferred_slot_when_free() {
        let subranges = [(3000, 3499)];
        let mut bind_ok = |_block: (u16, u16)| true;
        let result = allocate_block(&subranges, 5, Some("main"), "/whatever", &[], &mut bind_ok);
        assert_eq!(result, Some((3000, 3004)));
    }

    #[test]
    fn allocate_block_skips_occupied_slot() {
        let subranges = [(3000, 3499)];
        let occupied = [(3000, 3004)];
        let mut bind_ok = |_block: (u16, u16)| true;
        let result = allocate_block(
            &subranges,
            5,
            Some("main"),
            "/whatever",
            &occupied,
            &mut bind_ok,
        );
        assert_eq!(result, Some((3005, 3009)));
    }

    #[test]
    fn allocate_block_skips_slot_that_fails_bind_check() {
        let subranges = [(3000, 3499)];
        let mut bind_ok = |block: (u16, u16)| block != (3000, 3004);
        let result = allocate_block(&subranges, 5, Some("main"), "/whatever", &[], &mut bind_ok);
        assert_eq!(result, Some((3005, 3009)));
    }

    #[test]
    fn allocate_block_returns_none_when_subrange_fully_occupied() {
        let subranges = [(3000, 3009)]; // exactly two slots of size 5
        let occupied = [(3000, 3004), (3005, 3009)];
        let mut bind_ok = |_block: (u16, u16)| true;
        let result = allocate_block(
            &subranges,
            5,
            Some("main"),
            "/whatever",
            &occupied,
            &mut bind_ok,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn allocate_block_returns_none_when_bind_always_fails() {
        let subranges = [(3000, 3009)];
        let mut bind_ok = |_block: (u16, u16)| false;
        let result = allocate_block(&subranges, 5, Some("main"), "/whatever", &[], &mut bind_ok);
        assert_eq!(result, None);
    }

    #[test]
    fn allocate_block_skips_subrange_too_narrow_for_one_block() {
        // First subrange is narrower than block_size (must be skipped
        // entirely), second subrange has room.
        let subranges = [(3000, 3003), (4000, 4499)];
        let mut bind_ok = |_block: (u16, u16)| true;
        let result = allocate_block(&subranges, 5, Some("main"), "/whatever", &[], &mut bind_ok);
        assert_eq!(result, Some((4000, 4004)));
    }
}

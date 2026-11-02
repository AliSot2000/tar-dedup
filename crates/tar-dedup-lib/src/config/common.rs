

/// Merge function for values implementing Clone.
/// Values equal -> keep the base, values differ, take candidate.
pub(crate) fn merge_pick_clone<T: PartialEq + Clone>(cand: &T, base: &T, default: &T) -> T {
    if cand == default { base.clone() } else { cand.clone() }
}

/// Merge function for values implementing Copy.
/// Values equal -> keep the base, values differ, take candidate.
pub(crate) fn merge_pick_copy<T: Copy + PartialEq>(cand: T, base: T, default: T) -> T {
    if cand == default { base } else { cand }
}

/// Resolve one capture bit: explicit `--no-x` wins, then `--x`, then the base
/// (INCLUDE = `true` when `capture_all`, EXCLUDE = `false`).
pub(crate) fn resolve_bool_flag(no: bool, yes: bool, base: bool) -> bool {
    debug_assert!(!(no && yes), "PRECONDITION FAILED: no and yes prohibited.");
    if no {
        false
    } else if yes {
        true
    } else {
        base
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_pick_copy_keeps_base_when_candidate_matches_default() {
        assert_eq!(merge_pick_copy(0u32, 5, 0), 5, "candidate == default -> base");
        assert_eq!(merge_pick_copy(3u32, 5, 0), 3, "candidate != default -> candidate");
        assert_eq!(merge_pick_copy(false, true, false), true);
        assert_eq!(merge_pick_copy(true, false, false), true);
    }

    #[test]
    fn merge_pick_clone_keeps_base_when_candidate_matches_default() {
        let base = vec![1u32, 2];
        let cand_eq = Vec::new();
        assert_eq!(merge_pick_clone(&cand_eq, &base, &Vec::new()), base);
        let cand = vec![9u32];
        assert_eq!(merge_pick_clone(&cand, &base, &Vec::new()), cand);
        // Shared default that differs from the base default instance.
        assert_eq!(merge_pick_clone(&vec![4u32], &base, &vec![4u32]), base);
    }

    #[test]
    fn resolve_bool_flag_no_wins_then_yes_then_base() {
        assert!(!resolve_bool_flag(true, false, true), "--no-x always wins");
        assert!(resolve_bool_flag(false, true, false), "--x next");
        assert!(resolve_bool_flag(false, false, true), "--no-x absent, --x absent -> base");
        assert!(!resolve_bool_flag(false, false, false));
    }
}

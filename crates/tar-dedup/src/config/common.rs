

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



pub(crate) fn merge_pick_clone<T: PartialEq + Clone>(cand: &T, base: &T, default: &T) -> T {
    if cand == default { base.clone() } else { cand.clone() }
}

pub(crate) fn merge_pick_copy<T: Copy + PartialEq>(cand: T, base: T, default: T) -> T {
    if cand == default { base } else { cand }
}
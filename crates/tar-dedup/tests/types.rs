use tar_dedup::db::types::FilePhase;

#[test]
fn file_phase_as_str_roundtrip() {
    let phases = [
        FilePhase::Inventoried,
        FilePhase::Hashed,
        FilePhase::Filtered,
        FilePhase::Deduped,
        FilePhase::Sparsified,
        FilePhase::Staged,
        FilePhase::Archived,
        FilePhase::Unarchived,
        FilePhase::Rehashed,
        FilePhase::AtDestination,
        FilePhase::PermissionsRestored,
    ];

    for phase in phases {
        assert_eq!(FilePhase::parse(phase.as_str()).expect("parse"), phase);
    }
}

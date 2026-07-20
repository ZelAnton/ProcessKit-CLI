//! Integration-test home for real-binary containment scenarios.

#[test]
fn roadmap_is_present() {
    assert!(std::path::Path::new("docs/ROADMAP.md").is_file());
}

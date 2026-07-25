//! The reference doc must render exactly from `ROLE_SUPPORT`; a mapping change that skips the
//! doc fails here rather than drifting silently.

const BEGIN: &str = "<!-- BEGIN GENERATED: role-support -->";
const END: &str = "<!-- END GENERATED: role-support -->";

#[test]
fn doc_matches_the_matrix() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/reference/a11y-roles.md"
    );
    let doc = std::fs::read_to_string(path).expect("read docs/reference/a11y-roles.md");
    let start = doc.find(BEGIN).expect("generated block start marker") + BEGIN.len();
    let end = doc.find(END).expect("generated block end marker");
    let block = doc[start..end].trim();
    assert_eq!(
        block,
        glass_core::role_support::render_markdown().trim(),
        "docs/reference/a11y-roles.md is stale — regenerate the block between the markers"
    );
}

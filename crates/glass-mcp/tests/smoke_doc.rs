//! The reference doc's target-app table must render exactly from the runner's own candidate
//! tables; a candidate that lands in the code and not in the doc fails here rather than leaving
//! the only written-down list wrong.

const BEGIN: &str = "<!-- BEGIN GENERATED: smoke-candidates -->";
const END: &str = "<!-- END GENERATED: smoke-candidates -->";

#[test]
fn the_target_apps_table_matches_the_candidate_tables() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reference/smoke.md");
    let doc = std::fs::read_to_string(path).expect("read docs/reference/smoke.md");
    // Git may check the file out with CRLF while the renderer always emits LF, which would fail
    // every line of the comparison on the Windows CI leg alone.
    let doc = doc.replace("\r\n", "\n");
    let start = doc.find(BEGIN).expect("generated block start marker") + BEGIN.len();
    let end = doc.find(END).expect("generated block end marker");
    assert_eq!(
        doc[start..end].trim(),
        glass_mcp::smoke::render_candidate_table().trim(),
        "docs/reference/smoke.md is stale — regenerate the block between the markers"
    );
}

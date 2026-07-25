//! The reference doc must render exactly from `ROLE_SUPPORT`; a mapping change that skips the
//! doc fails here rather than drifting silently.
//!
//! On Windows, Git's core.autocrlf may check out markdown files with CRLF line endings, while
//! render_markdown() always produces LF. This test normalizes CRLF → LF after reading the file
//! from disk to ensure the comparison is line-ending agnostic.

const BEGIN: &str = "<!-- BEGIN GENERATED: role-support -->";
const END: &str = "<!-- END GENERATED: role-support -->";

#[test]
fn doc_matches_the_matrix() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/reference/a11y-roles.md"
    );
    let doc = std::fs::read_to_string(path).expect("read docs/reference/a11y-roles.md");
    // Normalize CRLF → LF so the test passes regardless of how Git checked out the file.
    // On Windows, core.autocrlf may check out files with CRLF line endings, while render_markdown()
    // always emits LF. The comparison would fail on every line due to interior \r characters.
    let doc = doc.replace("\r\n", "\n");
    let start = doc.find(BEGIN).expect("generated block start marker") + BEGIN.len();
    let end = doc.find(END).expect("generated block end marker");
    let block = doc[start..end].trim();
    assert_eq!(
        block,
        glass_core::role_support::render_markdown().trim(),
        "docs/reference/a11y-roles.md is stale — regenerate the block between the markers"
    );
}

#[test]
fn crlf_normalization_works() {
    // Verify that CRLF normalization correctly converts Windows line endings to Unix line endings.
    // This is the mechanism that makes doc_matches_the_matrix() pass on a Windows checkout.
    let crlf_text = "line 1\r\nline 2\r\nline 3\r\n";
    let normalized = crlf_text.replace("\r\n", "\n");
    assert_eq!(normalized, "line 1\nline 2\nline 3\n");

    // Verify that the generated markdown can be found within normalized CRLF text.
    let generated = glass_core::role_support::render_markdown();
    let crlf_doc = format!("{}{}{}", BEGIN, generated, END).replace("\n", "\r\n");
    let normalized = crlf_doc.replace("\r\n", "\n");
    let start = normalized.find(BEGIN).unwrap() + BEGIN.len();
    let end = normalized.find(END).unwrap();
    let extracted = normalized[start..end].trim();
    assert_eq!(extracted, generated.trim());
}

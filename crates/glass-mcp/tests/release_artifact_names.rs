//! Guard: the release-artifact platform suffixes documented in `docs/reference/platforms.md` must
//! each be produced by `.github/workflows/release.yml`. The asset names are part of the 1.x
//! stability guarantee (see `docs/reference/stability.md`), so a rename/removal in the workflow that
//! forgets the doc — or a documented suffix the workflow never builds — is a drift this test catches.
//!
//! Direction: doc -> workflow. Each `glass-mcp-<tag>-<suffix>` in platforms.md must appear as a
//! literal in release.yml. Known limitation: only this direction is checked (the reverse — scanning
//! release.yml for asset patterns to confirm each is documented — is not implemented), so a brand-new
//! *undocumented* asset added to the workflow would not be caught; every rename/removal of a
//! documented suffix is.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // crates/glass-mcp -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root from CARGO_MANIFEST_DIR")
}

/// Platform suffixes documented as `glass-mcp-<tag>-<suffix>.<ext>` in platforms.md, excluding the
/// `<platform>` placeholder used in prose.
fn documented_suffixes(platforms_md: &str) -> BTreeSet<String> {
    platforms_md
        .split("glass-mcp-<tag>-")
        .skip(1)
        .map(|rest| rest.chars().take_while(|c| *c != '.').collect::<String>())
        .filter(|s| !s.is_empty() && !s.contains('<') && !s.contains('>'))
        .collect()
}

#[test]
fn documented_release_suffixes_are_produced_by_the_workflow() {
    let root = repo_root();
    let platforms = std::fs::read_to_string(root.join("docs/reference/platforms.md"))
        .expect("read docs/reference/platforms.md");
    let release_yml = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("read .github/workflows/release.yml");

    let suffixes = documented_suffixes(&platforms);
    assert!(
        suffixes.len() >= 4,
        "expected the four documented artifact suffixes, found {suffixes:?}"
    );

    for suffix in &suffixes {
        assert!(
            release_yml.contains(suffix.as_str()),
            "release.yml no longer references the `{suffix}` artifact suffix documented in \
             platforms.md — update one so the 1.x-stable asset names stay in sync"
        );
    }
}

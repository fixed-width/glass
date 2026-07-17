//! Guard: the per-platform READMEs bundled INTO the release archives
//! (`packaging/README-{gnu,musl,windows}.md`) ship standalone — `release.yml` puts only the
//! binary and that one file (renamed `README.md`) in each tarball/zip, no other repo files. So
//! every inline link in them must be an ABSOLUTE URL (or an in-page anchor): a repo-relative link
//! like `../docs/…` or a sibling `README-windows.md` resolves on GitHub but is broken the moment a
//! user extracts the archive. This test also forbids the exact stale claims a past drift shipped
//! (macOS "no prebuilt", Windows "not yet signed"), now that the product ships a notarized macOS
//! `.dmg` and a signed Windows `.exe`.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // crates/glass-mcp -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root from CARGO_MANIFEST_DIR")
}

const BUNDLED: &[&str] = &[
    "packaging/README-gnu.md",
    "packaging/README-musl.md",
    "packaging/README-windows.md",
];

/// Every inline link / image target `](target)` in the markdown, target read up to the first `)`.
fn link_targets(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(pos) = rest.find("](") {
        let after = &rest[pos + 2..];
        match after.find(')') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

#[test]
fn bundled_readmes_have_only_absolute_links() {
    let root = repo_root();
    for rel in BUNDLED {
        let md =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for target in link_targets(&md) {
            let absolute = target.starts_with("https://")
                || target.starts_with("http://")
                || target.starts_with("mailto:")
                || target.starts_with('#');
            assert!(
                absolute,
                "{rel} has the repo-relative link `]({target})`. This file ships STANDALONE inside \
                 the release archive (only the binary + this README are extracted), so a relative \
                 link breaks for the user. Use an absolute https:// URL \
                 (e.g. https://github.com/fixed-width/glass/blob/master/…)."
            );
        }
    }
}

#[test]
fn bundled_readmes_do_not_carry_stale_prebuilt_or_signing_claims() {
    let root = repo_root();
    // Exact stale claims a past drift shipped. The product now has a notarized macOS `.dmg` and a
    // signed Windows `.exe`, so none of these may reappear in a bundled README.
    let forbidden = [
        "no prebuilt binary yet",
        "not yet Authenticode-signed",
        "not yet signed",
    ];
    for rel in BUNDLED {
        let md =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for phrase in forbidden {
            assert!(
                !md.contains(phrase),
                "{rel} contains the stale claim {phrase:?} — the product ships a notarized macOS \
                 .dmg and a signed Windows exe; correct the README."
            );
        }
    }
}

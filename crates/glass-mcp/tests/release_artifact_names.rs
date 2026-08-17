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

/// Platform suffixes documented as `glass-mcp-<tag>-<suffix>` in platforms.md, with any `.<ext>`
/// trimmed off, and excluding the `<platform>` placeholder used in prose.
///
/// The suffix ends at the first `.`, whitespace, or closing backtick — whichever comes first. All
/// three bounds are load-bearing: the bare Linux assets are documented with no extension at all,
/// so a scan that only stopped at `.` would run straight past the closing backtick and swallow
/// whatever prose followed until the next full stop anywhere later in the file.
fn documented_suffixes(platforms_md: &str) -> BTreeSet<String> {
    platforms_md
        .split("glass-mcp-<tag>-")
        .skip(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| *c != '.' && *c != '`' && !c.is_whitespace())
                .collect::<String>()
        })
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

/// The one line of `release.yml` that satisfies `pred`, trimmed. Panics unless exactly one line
/// matches, so a needle that has silently become ambiguous is a failure rather than a coin toss.
///
/// The whole reason this test works line-by-line instead of with `release_yml.contains(..)` is
/// that the interesting strings are not unique in the file: `dist/glass-mcp-*` also appears in the
/// Linux attestation block and `dist/*.exe` appears three times, so a whole-file `contains` for
/// either stays green with the *upload* line deleted — and a release with no bare assets 404s
/// every `glass-mcp update`.
fn the_line<'a>(release_yml: &'a str, what: &str, pred: impl Fn(&str) -> bool) -> &'a str {
    let mut hits = release_yml.lines().map(str::trim).filter(|l| pred(l));
    let first = hits
        .next()
        .unwrap_or_else(|| panic!("release.yml no longer has {what}"));
    assert!(
        hits.next().is_none(),
        "more than one line of release.yml looks like {what} — this guard can no longer tell \
         which one it is checking"
    );
    first
}

/// Is `needle` one of `line`'s whole tokens?
///
/// Anchoring to the right line is only half the job: every glob this test looks for is a *prefix*
/// of a longer token that legitimately sits on the same line — `dist/*.exe` of
/// `dist/*.exe.sha256`, `dist/glass-mcp-*` of `dist/glass-mcp-*.tar.gz`, `glass-mcp-*` of
/// `glass-mcp-*.tar.gz`. A `contains` check is therefore satisfied by the *narrowed* form;
/// comparing whole tokens is what makes the narrowing fail.
///
/// Tokens are split on whitespace, `,`, and parentheses — the last because PowerShell writes
/// `(Get-ChildItem …).FullName`, so the final path would otherwise carry `).FullName` with it.
/// Quotes, `;` and braces are trimmed off each end for the same reason: `sh` writes
/// `for f in glass-mcp-*;`.
fn has_token(line: &str, needle: &str) -> bool {
    line.split([' ', '\t', ',', '(', ')'])
        .map(|t| t.trim_matches(['"', '\'', ';', '`', '{', '}']))
        .any(|t| t == needle)
}

/// The bare, uncompressed binary assets `glass-mcp update` downloads. The archives stay — these
/// are additional — so this is a second, independent direction of the same doc↔workflow guard:
/// documenting an updater asset the workflow never builds would leave `update` fetching a 404.
#[test]
fn the_bare_binary_assets_are_documented_and_produced() {
    let root = repo_root();
    let platforms = std::fs::read_to_string(root.join("docs/reference/platforms.md"))
        .expect("read docs/reference/platforms.md");
    let release_yml = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("read .github/workflows/release.yml");

    // The per-platform suffixes never appear inside a full asset name in the workflow — `pkg`
    // takes the suffix as an argument and builds the name at run time — so what is checkable here
    // is the set of lines that produce, hash and upload the bare files.

    // Linux: produced by copying the staged binary flat beside the archive...
    let linux_copy = the_line(&release_yml, "the bare linux binary copy", |l| {
        l.starts_with(r#"cp "$dir/glass-mcp""#)
    });
    assert!(
        has_token(linux_copy, "dist/$name"),
        "the bare linux binary no longer lands at dist/<asset name>: {linux_copy}"
    );
    // ...hashed by a loop whose glob covers it. Narrowed back to `*.tar.gz` and the bare assets
    // ship with no `.sha256`, which is a 404 at `update`'s sidecar fetch.
    let linux_sidecars = the_line(&release_yml, "the linux sha256sum loop", |l| {
        l.contains("sha256sum")
    });
    assert!(
        has_token(linux_sidecars, "glass-mcp-*"),
        "the linux checksum loop no longer covers the bare binaries: {linux_sidecars}"
    );
    // ...and uploaded by a glob wide enough to carry it.
    let linux_upload = the_line(&release_yml, "the linux release upload", |l| {
        l.contains("gh release upload") && l.contains("$TAG") && !l.contains("dmg")
    });
    assert!(
        has_token(linux_upload, "dist/glass-mcp-*"),
        "the linux upload no longer publishes the bare binaries: {linux_upload}"
    );

    // Windows: the same three steps, in PowerShell.
    //
    // Two lines copy the built .exe — one into the archive's staging directory
    // (`dist/$name/glass-mcp.exe`), one flat beside it (`dist/$name.exe`) — so this cannot anchor
    // on a single line the way the others do. It asserts instead that among all of those copies,
    // one lands flat.
    let exe_copies: Vec<&str> = release_yml
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("Copy-Item target/release/glass-mcp.exe"))
        .collect();
    assert!(
        exe_copies.iter().any(|l| has_token(l, "dist/$name.exe")),
        "nothing copies the bare .exe flat into dist/ — the copies are {exe_copies:?}"
    );
    let windows_sidecars = the_line(&release_yml, "the windows Get-FileHash loop", |l| {
        l.starts_with("foreach ($f in Get-ChildItem")
    });
    assert!(
        has_token(windows_sidecars, "dist/*.exe"),
        "the windows checksum loop no longer covers the bare .exe: {windows_sidecars}"
    );
    // The windows upload passes `$files`, so the assignment is what decides the asset set.
    let windows_upload = the_line(&release_yml, "the windows upload file list", |l| {
        l.starts_with("$files = ")
    });
    for needle in ["dist/*.exe", "dist/*.exe.sha256"] {
        assert!(
            has_token(windows_upload, needle),
            "the windows upload no longer publishes {needle}: {windows_upload}"
        );
    }
    let windows_gh = the_line(&release_yml, "the windows release upload", |l| {
        l.contains("gh release upload") && l.contains("GITHUB_REF_NAME")
    });
    assert!(
        has_token(windows_gh, "$files"),
        "the windows upload no longer uses the file list above: {windows_gh}"
    );

    assert!(
        platforms.contains("uncompressed"),
        "platforms.md must document the bare binary assets as what `update` fetches"
    );
}

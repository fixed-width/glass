//! Where a release lives, and what to call its asset.
//!
//! "Latest" is resolved by following `/releases/latest`'s redirect rather than through the REST
//! API: no token, no 60/hr unauthenticated rate limit, no JSON, and GitHub already excludes
//! prereleases from that endpoint. The cost is that the `Location` header is attacker-influenced
//! input which then gets interpolated into a download URL, so it is validated twice here — once
//! for the redirect target's prefix, once for the tag's own shape.

/// Where releases live. A constructor argument on `ReleaseSource` rather than a hardcoded
/// literal at the call site, so tests can drive the real code path against a local server.
pub(crate) const GITHUB_BASE: &str = "https://github.com";

/// The repo path under the base. Part of the redirect prefix check.
pub(crate) const REPO_PATH: &str = "/fixed-width/glass";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocationError {
    /// No `Location` header on a response that should have redirected.
    Missing,
    /// The redirect did not land on this repo's `/releases/tag/` path — a repo with no published
    /// release (GitHub redirects to `/releases`), an error page, or a foreign origin.
    NotATagRedirect,
    /// The right shape of URL, but the final segment is not a release tag.
    MalformedTag(String),
}

/// The platform asset suffix this binary was built for, from compile-time facts rather than
/// runtime probing: the running binary knows how it was built, so gnu-vs-musl needs no heuristic
/// and an update is always like-for-like. `None` means this target has no published asset.
pub(crate) fn asset_suffix() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        Some("x86_64-linux-musl")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", not(target_env = "musl")))]
    {
        Some("x86_64-linux-gnu")
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        Some("x86_64-windows.exe")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(windows, target_arch = "x86_64")
    )))]
    {
        None
    }
}

/// The bare-binary asset published for `tag` on this target. Keeps the tag's leading `v`:
/// `release.yml` names assets from `GITHUB_REF_NAME`, which is the tag itself.
pub(crate) fn asset_name(tag: &str) -> Option<String> {
    Some(format!("glass-mcp-{tag}-{}", asset_suffix()?))
}

/// Is this a glass release tag? `v` plus a numeric triple plus an optional letter-initial
/// prerelease. Anything else — a path separator, a query, whitespace, a `describe` suffix — is
/// rejected before it reaches a URL.
pub(crate) fn is_valid_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let mut parts = core.split('.');
    let triple_ok = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none();
    let pre_ok = match pre {
        None => true,
        Some(p) => {
            p.starts_with(|c: char| c.is_ascii_alphabetic())
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        }
    };
    triple_ok && pre_ok
}

/// Pull the release tag out of `/releases/latest`'s redirect target.
///
/// Both halves matter. The prefix check pins origin *and* repo path, so a redirect to another
/// host — or to `/releases`, which is what a repo with no published release returns — cannot be
/// mistaken for a release. The tag check then pins the remaining segment's shape, so nothing that
/// could escape the download path survives.
pub(crate) fn tag_from_location(base: &str, location: &str) -> Result<String, LocationError> {
    let prefix = format!("{base}{REPO_PATH}/releases/tag/");
    let tag = location
        .strip_prefix(&prefix)
        .ok_or(LocationError::NotATagRedirect)?;
    if !is_valid_tag(tag) {
        return Err(LocationError::MalformedTag(tag.to_string()));
    }
    Ok(tag.to_string())
}

use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// How long to wait for a connection before giving up. Short: the endpoint is either reachable
/// or it is not, and a stalled connect should report rather than hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request budget for the two small metadata requests (the redirect, the sidecar).
const META_TIMEOUT: Duration = Duration::from_secs(60);
/// Whole-request budget for the asset body. Generous, but bounded — a stalled download must not
/// hang forever.
const ASSET_TIMEOUT: Duration = Duration::from_secs(600);
/// How many redirects an asset fetch may follow. GitHub's real flow uses exactly one (to
/// `objects.githubusercontent.com`); the bound is what stops a redirect loop. Compared with `>`,
/// matching reqwest's own `Policy::limited` semantics, so "at most MAX_REDIRECTS followed" is
/// literally what the code does.
const MAX_REDIRECTS: usize = 5;

/// Where to fetch releases from. The base is a constructor argument rather than a constant at
/// the call site so the tests drive this exact code against a local server; there is deliberately
/// no environment variable, which would ship a knob that repoints the updater at any host.
pub(crate) struct ReleaseSource {
    base: String,
}

impl ReleaseSource {
    pub(crate) fn github() -> Self {
        Self::with_base(GITHUB_BASE)
    }

    pub(crate) fn with_base(base: impl Into<String>) -> Self {
        ReleaseSource { base: base.into() }
    }

    /// The newest published release's tag. `/releases/latest` redirects to the tag page and
    /// already excludes prereleases and drafts, so the redirect target is the whole answer.
    pub(crate) async fn latest_tag(&self) -> anyhow::Result<String> {
        let url = format!("{}{REPO_PATH}/releases/latest", self.base);
        // Redirects OFF: the Location header IS the result here, not something to follow.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(META_TIMEOUT)
            .user_agent(concat!("glass-mcp/", env!("GLASS_VERSION")))
            .build()?;
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{url} did not redirect to a release (HTTP {})",
                    resp.status()
                )
            })?;
        tag_from_location(&self.base, location).map_err(|e| match e {
            LocationError::Missing | LocationError::NotATagRedirect => anyhow::anyhow!(
                "{url} redirected to {location}, which is not a release tag page — \
                 the repo may have no published release, or GitHub may be returning an error page"
            ),
            LocationError::MalformedTag(t) => {
                anyhow::anyhow!("{url} redirected to an unexpected release tag {t:?}")
            }
        })
    }

    pub(crate) fn asset_url(&self, tag: &str, asset: &str) -> String {
        format!("{}{REPO_PATH}/releases/download/{tag}/{asset}", self.base)
    }

    /// Build the client used for asset and sidecar fetches. Redirects ARE followed here (GitHub
    /// sends release downloads to objects.githubusercontent.com), bounded to
    /// [`MAX_REDIRECTS`] hops, and a hop that downgrades the scheme is refused — over https that
    /// is the "https all the way" rule, and over the tests' loopback http it is the same rule, so
    /// production's guard is the one under test.
    fn following_client(&self, timeout: Duration) -> anyhow::Result<reqwest::Client> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            let downgraded = attempt
                .previous()
                .first()
                .is_some_and(|first| first.scheme() != attempt.url().scheme());
            if downgraded {
                return attempt.error("refusing a redirect that changes the URL scheme");
            }
            attempt.follow()
        });
        Ok(reqwest::Client::builder()
            .redirect(policy)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            .user_agent(concat!("glass-mcp/", env!("GLASS_VERSION")))
            .build()?)
    }

    /// Fetch a small text resource — the checksum sidecar.
    pub(crate) async fn fetch_text(&self, url: &str) -> anyhow::Result<String> {
        // `with_context` rather than folding `e` into the message with `{e}`: reqwest's `Display`
        // for a redirect-policy refusal is just "error following redirect for url (...)" — the
        // policy's own message ("too many redirects", "refusing a redirect that changes the URL
        // scheme") only exists on `e`'s `source()`. `with_context` keeps `e` as the source instead
        // of discarding it, so that text is still reachable from the returned error.
        let resp = self
            .following_client(META_TIMEOUT)?
            .get(url)
            .send()
            .await
            .with_context(|| format!("could not fetch {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("{url} returned HTTP {status}");
        }
        Ok(resp.text().await?)
    }

    /// Stream the asset to `dest`, hashing as it goes, and return the lowercase hex SHA-256 of
    /// what was written. One pass: the binary is never fully buffered in memory, and the digest
    /// describes the bytes that actually landed rather than a re-read of the file.
    ///
    /// On any failure the partial file is removed, so a failed download leaves nothing behind for
    /// a later run to trip over.
    pub(crate) async fn download_to(&self, url: &str, dest: &Path) -> anyhow::Result<String> {
        match self.download_inner(url, dest).await {
            Ok(digest) => Ok(digest),
            Err(e) => {
                let _ = std::fs::remove_file(dest);
                Err(e)
            }
        }
    }

    async fn download_inner(&self, url: &str, dest: &Path) -> anyhow::Result<String> {
        use std::io::Write;

        // See the matching comment on `fetch_text`: `with_context` keeps the redirect policy's
        // refusal message reachable via the error's source chain instead of discarding it.
        let mut resp = self
            .following_client(ASSET_TIMEOUT)?
            .get(url)
            .send()
            .await
            .with_context(|| format!("could not fetch {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("{url} returned HTTP {status}");
        }
        let mut file = std::fs::File::create(dest)
            .map_err(|e| anyhow::anyhow!("could not write {}: {e}", dest.display()))?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = resp.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        Ok(hex(&hasher.finalize()))
    }
}

/// Lowercase hex, to match `sha256sum`'s output — which is what the sidecar contains.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://github.com";

    #[test]
    fn a_release_tag_is_valid() {
        assert!(is_valid_tag("v1.3.0"));
        assert!(is_valid_tag("v1.2.0-rc1"));
        assert!(is_valid_tag("v10.20.30"));
    }

    /// The tag is interpolated into a download URL, so anything that could escape the release
    /// path has to be rejected here rather than sanitized later.
    #[test]
    fn a_tag_that_could_escape_the_url_is_rejected() {
        for t in [
            "v1.3.0/../../evil",
            "../v1.3.0",
            "v1.3.0?x=1",
            "v1.3.0#f",
            "v1.3.0 ",
            "1.3.0",
            "vv1.3.0",
            "v1.3.0-5-g563feea",
            "",
        ] {
            assert!(!is_valid_tag(t), "{t:?} must be rejected");
        }
    }

    #[test]
    fn the_tag_comes_out_of_a_well_formed_location() {
        let loc = "https://github.com/fixed-width/glass/releases/tag/v1.4.0";
        assert_eq!(tag_from_location(BASE, loc).unwrap(), "v1.4.0");
    }

    /// A repo with no published release redirects to `/releases`, and a GitHub error page during
    /// an incident lands here too. Neither may be reported as "you are up to date".
    #[test]
    fn a_redirect_without_a_tag_segment_is_an_error() {
        assert!(matches!(
            tag_from_location(BASE, "https://github.com/fixed-width/glass/releases"),
            Err(LocationError::NotATagRedirect)
        ));
    }

    #[test]
    fn a_redirect_off_the_expected_prefix_is_an_error() {
        for loc in [
            "https://evil.example/fixed-width/glass/releases/tag/v1.4.0",
            "http://github.com/fixed-width/glass/releases/tag/v1.4.0",
            "https://github.com/someone/else/releases/tag/v1.4.0",
        ] {
            assert!(
                matches!(
                    tag_from_location(BASE, loc),
                    Err(LocationError::NotATagRedirect)
                ),
                "{loc:?} must be rejected"
            );
        }
    }

    /// De-anchoring regression. `strip_prefix` is anchored at byte 0, which is the only reason a
    /// URL that merely *contains* the expected prefix is rejected. Nothing else in the suite pins
    /// that, and cargo-mutants cannot generate a de-anchoring mutant — so without this, an
    /// accidental switch to a substring match would pass every committed test.
    #[test]
    fn a_prefix_appearing_mid_url_is_not_a_tag_redirect() {
        let loc =
            "https://attacker.example/x/https://github.com/fixed-width/glass/releases/tag/v1.4.0";
        assert!(matches!(
            tag_from_location(BASE, loc),
            Err(LocationError::NotATagRedirect)
        ));
    }

    #[test]
    fn a_malformed_tag_in_an_otherwise_valid_location_is_an_error() {
        let loc = "https://github.com/fixed-width/glass/releases/tag/v1.4.0/../../evil";
        assert!(matches!(
            tag_from_location(BASE, loc),
            Err(LocationError::MalformedTag(_))
        ));
    }

    /// The base is a constructor argument so tests can point at a local server; the prefix check
    /// has to follow it, or the production origin check would be untested.
    #[test]
    fn the_prefix_check_follows_the_configured_base() {
        let base = "http://127.0.0.1:8080";
        let loc = "http://127.0.0.1:8080/fixed-width/glass/releases/tag/v1.4.0";
        assert_eq!(tag_from_location(base, loc).unwrap(), "v1.4.0");
        assert!(
            tag_from_location(
                base,
                "https://github.com/fixed-width/glass/releases/tag/v1.4.0"
            )
            .is_err()
        );
    }

    /// Asset names embed the tag WITH its `v` — `release.yml` builds them from GITHUB_REF_NAME.
    ///
    /// Skips where no asset is published (macOS, non-x86_64): CI's macOS job runs
    /// `cargo test --workspace --lib`, so this module executes there, and `asset_name` correctly
    /// returns `None`. `the_asset_suffix_matches_this_build` is what asserts that case.
    #[test]
    fn the_asset_name_keeps_the_tags_v() {
        let Some(name) = asset_name("v1.4.0") else {
            return;
        };
        assert!(name.starts_with("glass-mcp-v1.4.0-"), "got {name}");
        assert!(
            !name.contains("glass-mcp-1.4.0"),
            "the v must not be stripped: {name}"
        );
    }

    #[test]
    fn the_asset_suffix_matches_this_build() {
        let suffix = asset_suffix();
        if cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "musl"
        )) {
            assert_eq!(suffix, Some("x86_64-linux-musl"));
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(suffix, Some("x86_64-linux-gnu"));
        } else if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(suffix, Some("x86_64-windows.exe"));
        } else {
            assert_eq!(suffix, None, "unsupported targets must refuse, not guess");
        }
    }

    use crate::update::testserver::FakeRelease;

    const BODY: &[u8] = b"not really a binary, but it hashes just the same";
    /// sha256 of BODY. Recompute with `printf '%s' '<BODY>' | sha256sum` if BODY changes.
    const BODY_SHA: &str = "24c71201dd8f823148c6c782b5d0b288376212d1b8669a0ebdefd3c5b3b623fe";

    #[tokio::test]
    async fn latest_tag_follows_the_redirect() {
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        assert_eq!(src.latest_tag().await.unwrap(), "v1.4.0");
    }

    #[tokio::test]
    async fn a_missing_release_is_an_error_not_an_up_to_date_report() {
        // An empty tag makes the redirect land on `/releases/tag/` with nothing after it — the
        // same shape as a repo with nothing published, or a GitHub error page. It must be an
        // error, never a quiet "you are up to date".
        let server = FakeRelease::start("", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        assert!(src.latest_tag().await.is_err());
    }

    #[tokio::test]
    async fn download_writes_the_bytes_and_returns_their_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("downloaded");
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        let url = src.asset_url("v1.4.0", "asset");
        let digest = src.download_to(&url, &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert_eq!(digest, BODY_SHA);
    }

    #[tokio::test]
    async fn a_404_asset_is_an_error_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("downloaded");
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        let url = src.asset_url("v1.4.0", "no-such-asset");
        assert!(src.download_to(&url, &dest).await.is_err());
        assert!(
            !dest.exists(),
            "a failed download must leave nothing behind"
        );
    }

    #[tokio::test]
    async fn fetch_text_reads_the_sidecar() {
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "abc123  asset\n");
        let src = ReleaseSource::with_base(server.base());
        let url = format!("{}.sha256", src.asset_url("v1.4.0", "asset"));
        assert_eq!(src.fetch_text(&url).await.unwrap(), "abc123  asset\n");
    }

    /// The scheme guard, which is the reason `following_client` has a custom policy at all. The
    /// error must be ours, not a TLS/connect failure — asserting on the word "scheme" is what
    /// distinguishes "the policy refused it" from "it tried https on a plaintext port and
    /// failed". Two things have to hold for that assertion to mean anything:
    ///
    /// - It has to inspect the *root cause*, not `.to_string()` — `anyhow::Error`'s `Display`
    ///   only shows the top-level context ("could not fetch {url}"), never the policy's own
    ///   message, which lives on the source chain (see the `with_context` calls in `fetch_text`
    ///   and `download_inner`).
    /// - It has to inspect the *last* link in that chain, not "does any link contain the word" —
    ///   both the context string and reqwest's own `Display` for the failure echo the request
    ///   URL, and this test's URL is `/redirect/scheme`, which trivially contains "scheme" on its
    ///   own. Only the deepest cause — the literal string passed to `attempt.error(...)`, which
    ///   never carries a URL — is guaranteed free of that coincidence.
    #[tokio::test]
    async fn a_redirect_that_changes_the_scheme_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("downloaded");
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        let url = format!("{}/redirect/scheme", server.base());
        let err = src.download_to(&url, &dest).await.unwrap_err();
        let root_cause = err.chain().last().expect("at least one link").to_string();
        assert!(
            root_cause.contains("scheme"),
            "expected the scheme guard as the root cause, got: {root_cause}"
        );
        assert!(
            !dest.exists(),
            "a refused redirect must leave nothing behind"
        );
    }

    /// The hop cap. Pins that a bound exists, not its exact value — the value is arbitrary, but a
    /// redirect loop with no bound is a hang. Root cause, not `.to_string()`, for the same reason
    /// as the scheme test above: the top-level context and reqwest's own `Display` both only
    /// report a URL, never the policy's "too many redirects" message.
    #[tokio::test]
    async fn an_endless_redirect_chain_is_cut_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("downloaded");
        let server = FakeRelease::start("v1.4.0", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        let url = format!("{}/redirect/chain/0", server.base());
        let err = src.download_to(&url, &dest).await.unwrap_err();
        let root_cause = err.chain().last().expect("at least one link").to_string();
        assert!(
            root_cause.contains("too many redirects"),
            "expected the hop cap as the root cause, got: {root_cause}"
        );
        assert!(
            !dest.exists(),
            "a refused redirect must leave nothing behind"
        );
    }
}

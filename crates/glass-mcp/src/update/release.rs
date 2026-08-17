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
}

//! Classifying and comparing glass versions.
//!
//! `crate::VERSION` comes from `build.rs::glass_version()`, which emits two quite different
//! shapes: a CI tag build gives the tag with its leading `v` stripped (`1.3.0`), while a local
//! build gives `git describe --tags --always --dirty` (`1.3.0-5-g563feea`, `1.3.0-dirty`, or a
//! bare short SHA). Only the first shape may be updated over.

use std::cmp::Ordering;
use std::fmt;

/// A released glass version: what `build.rs` embeds for a CI tag build, or a release tag with
/// its `v` stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// `Some("rc1")` for `1.2.0-rc1`, `None` for a final release.
    pre: Option<String>,
}

impl Version {
    /// Parse a *released* version. Returns `None` for anything a local build could produce, which
    /// is what stops the apply path from overwriting a from-source binary.
    pub(crate) fn parse_released(s: &str) -> Option<Version> {
        // The no-VCS fallback. It is a well-formed triple, so it has to be rejected by value.
        if s == "0.0.0" {
            return None;
        }
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parse_number(parts.next()?)?;
        let minor = parse_number(parts.next()?)?;
        let patch = parse_number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => None,
            Some(p) if is_release_prerelease(p) => Some(p.to_string()),
            Some(_) => return None,
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

/// A component of the `MAJOR.MINOR.PATCH` core. Rejects the empty string and any sign, which
/// `u64::from_str` would otherwise accept in part (`+1`) or reject unhelpfully.
fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Is this suffix a real prerelease tag rather than a `git describe` artifact?
///
/// In practice a `describe` suffix (`5-g563feea`) is rejected by its internal dash, which the
/// byte-class check below catches on its own. The first-character check exists for the case the
/// byte-class check can't reach: a dash-free, digit-initial suffix (`5`, `563feea`) that would
/// otherwise pass as a prerelease tag. `dirty` starts with a letter too, so it is excluded by
/// name — it is the one `describe` suffix the other two checks would let through.
fn is_release_prerelease(p: &str) -> bool {
    p != "dirty"
        && p.starts_with(|c: char| c.is_ascii_alphabetic())
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

impl Ord for Version {
    /// Numeric on the triple, then "a prerelease is older than its own final release".
    ///
    /// Deliberately **not** general semver. The only comparison production ever performs is a
    /// local version against `/releases/latest`, which never returns a prerelease — so the
    /// both-are-prereleases arm below cannot fire in production and exists for totality. Its
    /// lexicographic order would sort `rc10` before `rc2`; that is acceptable precisely because
    /// nothing reaches it, and it is documented here so no one later mistakes this for a complete
    /// semver implementation.
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match &self.pre {
            Some(p) => write!(f, "-{p}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ci_tag_build_parses() {
        let v = Version::parse_released("1.3.0").expect("a released version");
        assert_eq!(v.to_string(), "1.3.0");
    }

    #[test]
    fn a_prerelease_tag_parses_and_keeps_its_suffix() {
        let v = Version::parse_released("1.2.0-rc1").expect("rc tags are released versions");
        assert_eq!(v.to_string(), "1.2.0-rc1");
    }

    /// The discriminating pair for the *shape* rule. `1.2.0-rc1` and `1.3.0-5-g563feea` BOTH carry
    /// a `-` suffix, so a classifier that merely looks for a dash gets one of them wrong whichever
    /// way it guesses.
    ///
    /// What rejects the describe suffix here is the byte-class guard — `5-g563feea` holds an
    /// internal dash — not the first-character rule. The first-character rule is covered by
    /// `a_digit_initial_suffix_is_not_a_release_prerelease`.
    #[test]
    fn a_describe_suffix_is_rejected_while_an_rc_tag_is_accepted() {
        assert!(Version::parse_released("1.3.0-5-g563feea").is_none());
        assert!(Version::parse_released("1.2.0-rc1").is_some());
    }

    #[test]
    fn a_dirty_tree_is_rejected() {
        assert!(Version::parse_released("1.3.0-dirty").is_none());
        assert!(Version::parse_released("1.3.0-5-g563feea-dirty").is_none());
    }

    /// The `starts_with(alphabetic)` clause's own case, and the only one that exercises it.
    ///
    /// Every `git describe` suffix in real use carries an internal dash (`5-g563feea`), which the
    /// byte-class guard rejects by itself — so without a dash-free, digit-initial input that clause
    /// is redundant against the whole suite, and the mutation gate would rightly flag it as a
    /// survivor. These two inputs are what make the stated rule ("the first character decides")
    /// actually load-bearing.
    #[test]
    fn a_digit_initial_suffix_is_not_a_release_prerelease() {
        assert!(Version::parse_released("1.3.0-5").is_none());
        assert!(Version::parse_released("1.3.0-563feea").is_none());
    }

    /// `git describe --always` with no tags in reach emits a bare short SHA.
    #[test]
    fn a_bare_sha_is_rejected() {
        assert!(Version::parse_released("563feea").is_none());
    }

    /// `build.rs` falls back to CARGO_PKG_VERSION when there is no VCS at all, and that is
    /// pinned at 0.0.0. It parses as a valid triple, so it needs its own rejection.
    #[test]
    fn the_no_vcs_fallback_is_rejected() {
        assert!(Version::parse_released("0.0.0").is_none());
    }

    #[test]
    fn malformed_versions_are_rejected() {
        for s in ["", "1.3", "1.3.0.1", "1.3.x", "v1.3.0", "1.-3.0", "1.3.0-"] {
            assert!(Version::parse_released(s).is_none(), "{s:?} must not parse");
        }
    }

    #[test]
    fn ordering_compares_the_triple_numerically() {
        let older = Version::parse_released("1.3.0").unwrap();
        let newer = Version::parse_released("1.4.0").unwrap();
        assert!(newer > older);
        assert!(
            Version::parse_released("1.10.0").unwrap() > Version::parse_released("1.9.0").unwrap()
        );
        assert!(
            Version::parse_released("2.0.0").unwrap() > Version::parse_released("1.99.99").unwrap()
        );
        assert_eq!(older, Version::parse_released("1.3.0").unwrap());
    }

    /// The rule that makes this worth hand-rolling: `/releases/latest` never returns a
    /// prerelease, so someone on `1.4.0-rc1` must be told they are AHEAD of `1.3.0` rather than
    /// offered a downgrade. Both directions, because an implementation that swapped the arms
    /// would still pass a one-directional test.
    #[test]
    fn a_prerelease_is_older_than_its_own_final_release() {
        let rc = Version::parse_released("1.4.0-rc1").unwrap();
        let fin = Version::parse_released("1.4.0").unwrap();
        assert!(rc < fin);
        assert!(fin > rc);
        assert!(rc > Version::parse_released("1.3.0").unwrap());
    }
}

//! Compiled fixture recognition; no entry authorizes input or qualifies configuration.

use glass_core::{AppSpec, Result, SandboxLevel};

use super::refusal;

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
pub(super) struct FixtureProfile {
    pub name: &'static str,
    sha256: [u8; 32],
    image_uuid: [u8; 16],
    os_build: &'static str,
    architecture: &'static str,
}

// Frozen window_target.swift build: Swift 6.4, -swift-version 5, arm64 macOS 27.0.
const FIXTURES: &[FixtureProfile] = &[FixtureProfile {
    name: "appkit-ownership-v1",
    sha256: [
        20, 84, 63, 146, 64, 31, 84, 54, 209, 161, 141, 42, 116, 27, 240, 157, 175, 46, 234, 221,
        111, 198, 191, 230, 114, 55, 22, 213, 119, 141, 46, 11,
    ],
    image_uuid: [
        23, 83, 230, 23, 103, 113, 56, 32, 169, 164, 215, 48, 8, 64, 201, 5,
    ],
    os_build: "26A428",
    architecture: "aarch64",
}];

impl FixtureProfile {
    fn matches_artifact(&self, sha256: [u8; 32], image_uuid: [u8; 16]) -> bool {
        self.sha256 == sha256 && self.image_uuid == image_uuid
    }

    pub(super) fn require_host(&self, os_build: &str, architecture: &str) -> Result<()> {
        if self.os_build != os_build || self.architecture != architecture {
            return Err(refusal("matched fixture host changed or is unavailable"));
        }
        Ok(())
    }
}

pub(super) struct FixtureBinding {
    profile: Option<&'static FixtureProfile>,
    invalidated: bool,
}

impl FixtureBinding {
    pub(super) fn bind(
        sha256: [u8; 32],
        image_uuid: [u8; 16],
        spec: &AppSpec,
        mut read_os_build: impl FnMut() -> Result<String>,
        architecture: &str,
    ) -> Result<Self> {
        let candidate = unique_fixture_candidate(FIXTURES, sha256, image_uuid, spec, architecture);
        let profile = if let Some(candidate) = candidate {
            candidate
                .require_host(&read_os_build()?, architecture)
                .is_ok()
                .then_some(candidate)
        } else {
            None
        };
        Ok(Self {
            profile,
            invalidated: false,
        })
    }

    pub(super) fn name(&self) -> Option<&'static str> {
        self.profile
            .filter(|_| !self.invalidated)
            .map(|profile| profile.name)
    }

    pub(super) fn revalidate(
        &mut self,
        mut read_os_build: impl FnMut() -> Result<String>,
        architecture: &str,
    ) -> Result<()> {
        if self.invalidated {
            return Err(refusal("matched fixture binding was invalidated"));
        }
        let Some(profile) = self.profile else {
            return Ok(());
        };
        let result = read_os_build().and_then(|build| profile.require_host(&build, architecture));
        if result.is_err() {
            self.invalidated = true;
        }
        result
    }

    pub(super) fn observe<T>(
        &mut self,
        mut read_os_build: impl FnMut() -> Result<String>,
        architecture: &str,
        observation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.revalidate(&mut read_os_build, architecture)?;
        let observed = observation()?;
        self.revalidate(read_os_build, architecture)
            .map_err(glass_core::GlassError::after_dispatch)?;
        Ok(observed)
    }
}

fn unique_fixture_candidate<'a>(
    profiles: &'a [FixtureProfile],
    sha256: [u8; 32],
    image_uuid: [u8; 16],
    spec: &AppSpec,
    architecture: &str,
) -> Option<&'a FixtureProfile> {
    if spec.build.is_some()
        || spec.window_hint.is_some()
        || spec.cwd.is_some()
        || !spec.env.is_empty()
        || spec.sandbox != SandboxLevel::Default
        || !spec.a11y
        || spec.run.len() != 2
        || !matches!(
            spec.run[1].as_str(),
            "stable" | "replace" | "close" | "move" | "compete" | "exit"
        )
    {
        return None;
    }
    let mut matches = profiles.iter().filter(|profile| {
        profile.matches_artifact(sha256, image_uuid) && profile.architecture == architecture
    });
    let profile = matches.next()?;
    matches.next().is_none().then_some(profile)
}

#[cfg(test)]
fn matching_fixture(
    sha256: [u8; 32],
    image_uuid: [u8; 16],
    spec: &AppSpec,
    os_build: &str,
    architecture: &str,
) -> Option<&'static FixtureProfile> {
    unique_matching_fixture(FIXTURES, sha256, image_uuid, spec, os_build, architecture)
}

#[cfg(test)]
fn unique_matching_fixture<'a>(
    profiles: &'a [FixtureProfile],
    sha256: [u8; 32],
    image_uuid: [u8; 16],
    spec: &AppSpec,
    os_build: &str,
    architecture: &str,
) -> Option<&'a FixtureProfile> {
    unique_fixture_candidate(profiles, sha256, image_uuid, spec, architecture)
        .filter(|profile| profile.require_host(os_build, architecture).is_ok())
}

pub(super) fn parse_os_build(bytes: &[u8], length: usize) -> Result<&str> {
    let value = bytes
        .get(..length)
        .and_then(|value| value.strip_suffix(&[0]))
        .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_alphanumeric))
        .ok_or_else(|| refusal("kernel OS build response is incomplete"))?;
    std::str::from_utf8(value).map_err(|_| refusal("kernel OS build is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundDispatch, WindowHint};
    use sha2::{Digest, Sha256};

    fn spec() -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["/tmp/any-name".into(), "stable".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 3_000,
            sandbox: SandboxLevel::Default,
            a11y: true,
        }
    }

    fn matched(spec: &AppSpec) -> Option<&'static FixtureProfile> {
        let profile = &FIXTURES[0];
        matching_fixture(
            profile.sha256,
            profile.image_uuid,
            spec,
            "26A428",
            "aarch64",
        )
    }

    #[test]
    fn frozen_fixture_source_matches_its_build_record() {
        assert_eq!(
            Sha256::digest(include_bytes!("../../fixture/window_target.swift"))
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "910c1cec0d8b44d8e70211a44440bbef4ffebda800ff5864590cef15910cca43"
        );
    }

    #[test]
    fn exact_artifact_and_declared_shape_match_independently_of_basename() {
        let mut launch = spec();
        for scenario in ["stable", "replace", "close", "move", "compete", "exit"] {
            launch.run[1] = scenario.into();
            assert_eq!(matched(&launch).unwrap().name, "appkit-ownership-v1");
        }
    }

    #[test]
    fn equal_uuid_does_not_enroll_an_unrecognized_digest() {
        let profile = &FIXTURES[0];
        let mut digest = profile.sha256;
        digest[0] ^= 1;
        assert!(
            matching_fixture(digest, profile.image_uuid, &spec(), "26A428", "aarch64").is_none()
        );
    }

    #[test]
    fn equal_digest_does_not_hide_a_wrong_image_uuid() {
        let profile = &FIXTURES[0];
        let mut uuid = profile.image_uuid;
        uuid[0] ^= 1;
        assert!(matching_fixture(profile.sha256, uuid, &spec(), "26A428", "aarch64").is_none());
    }

    #[test]
    fn unknown_or_extra_arguments_do_not_match_the_declared_shape() {
        for args in [vec![], vec!["unknown"], vec!["stable", "extra"]] {
            let mut launch = spec();
            launch.run.truncate(1);
            launch.run.extend(args.into_iter().map(String::from));
            assert!(matched(&launch).is_none());
        }
    }

    #[test]
    fn changed_declared_configuration_is_unrecognized() {
        let mut cases: Vec<_> = (0..7).map(|_| spec()).collect();
        cases[0].build = Some("true".into());
        cases[1].window_hint = Some(WindowHint {
            title: Some("target-window".into()),
            class: None,
        });
        cases[2].cwd = Some("/tmp".into());
        cases[3].env = vec![("UNRECOGNIZED_FIXTURE_SETTING".into(), "1".into())];
        cases[4].sandbox = SandboxLevel::Strict;
        cases[5].sandbox = SandboxLevel::Off;
        cases[6].a11y = false;
        for launch in cases {
            assert!(matched(&launch).is_none());
        }
    }

    #[test]
    fn missing_or_ambiguous_registry_entries_do_not_match() {
        let profile = &FIXTURES[0];
        assert!(
            unique_matching_fixture(
                &[],
                profile.sha256,
                profile.image_uuid,
                &spec(),
                "26A428",
                "aarch64"
            )
            .is_none()
        );
        let duplicates = [FIXTURES, FIXTURES].concat();
        assert!(
            unique_matching_fixture(
                &duplicates,
                profile.sha256,
                profile.image_uuid,
                &spec(),
                "26A428",
                "aarch64"
            )
            .is_none()
        );
    }

    #[test]
    fn unsupported_host_never_matches_and_a_changed_matched_host_refuses() {
        let profile = &FIXTURES[0];
        for (build, architecture) in [("", "aarch64"), ("26A429", "aarch64"), ("26A428", "x86_64")]
        {
            assert!(
                matching_fixture(
                    profile.sha256,
                    profile.image_uuid,
                    &spec(),
                    build,
                    architecture
                )
                .is_none()
            );
            assert_eq!(
                profile
                    .require_host(build, architecture)
                    .unwrap_err()
                    .bound_dispatch(),
                Some(BoundDispatch::NotDispatched)
            );
        }
    }

    fn binding() -> FixtureBinding {
        let profile = &FIXTURES[0];
        FixtureBinding::bind(
            profile.sha256,
            profile.image_uuid,
            &spec(),
            || Ok("26A428".into()),
            "aarch64",
        )
        .unwrap()
    }

    #[test]
    fn unmatched_candidates_never_read_host_facts_or_gain_a_profile() {
        let profile = &FIXTURES[0];
        let mut launch = spec();
        launch.run[1] = "unknown".into();
        let mut binding = FixtureBinding::bind(
            profile.sha256,
            profile.image_uuid,
            &launch,
            || panic!("unmatched shape read host"),
            "aarch64",
        )
        .unwrap();
        assert_eq!(binding.name(), None);
        binding
            .observe(
                || panic!("unmatched observation read host"),
                "aarch64",
                || Ok(()),
            )
            .unwrap();
        assert_eq!(binding.name(), None);
        let unknown = FixtureBinding::bind(
            [0; 32],
            profile.image_uuid,
            &spec(),
            || panic!("unknown artifact read host"),
            "aarch64",
        )
        .unwrap();
        assert_eq!(unknown.name(), None);
    }

    #[test]
    fn lost_host_before_observation_invalidates_without_running_it() {
        let mut binding = binding();
        let error = binding
            .observe(
                || Err(refusal("host unavailable")),
                "aarch64",
                || -> Result<()> { panic!("observation ran after host loss") },
            )
            .unwrap_err();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(
            binding
                .observe(
                    || panic!("invalidated binding read host"),
                    "aarch64",
                    || Ok(())
                )
                .is_err()
        );
        assert_eq!(binding.name(), None);
    }

    #[test]
    fn host_is_read_on_both_sides_and_post_read_loss_never_rebinds() {
        use std::cell::Cell;
        let reads = Cell::new(0);
        let observed = Cell::new(false);
        let mut binding = binding();
        binding
            .revalidate(
                || {
                    reads.set(reads.get() + 1);
                    Ok("26A428".into())
                },
                "aarch64",
            )
            .unwrap();
        let error = binding
            .observe(
                || {
                    reads.set(reads.get() + 1);
                    Ok(if reads.get() < 3 { "26A428" } else { "26A429" }.into())
                },
                "aarch64",
                || {
                    observed.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(observed.get());
        assert_eq!(reads.get(), 3);
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(
            binding
                .revalidate(|| panic!("restored host was adopted"), "aarch64")
                .is_err()
        );
        assert_eq!(binding.name(), None);
    }

    #[test]
    fn incomplete_or_noncanonical_kernel_build_responses_refuse() {
        assert_eq!(parse_os_build(b"26A428\0", 7).unwrap(), "26A428");
        for (bytes, length) in [
            (b"26A428\0".as_slice(), 6),
            (b"26A428\0", 8),
            (b"\0", 1),
            (b"26\0A\0", 5),
            (b"26\xff\0", 4),
            (b"26 A\0", 5),
        ] {
            assert_eq!(
                parse_os_build(bytes, length).unwrap_err().bound_dispatch(),
                Some(BoundDispatch::NotDispatched)
            );
        }
    }
}

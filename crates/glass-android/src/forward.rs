//! Track companion forwards even when adb loses the allocated port's reply.

use glass_core::{Deadline, GlassError, Result};

use crate::adb::Adb;

#[derive(PartialEq, Eq)]
struct Entry {
    serial: String,
    port: u16,
}

pub(crate) struct Forward {
    remote: String,
    before: Vec<Entry>,
    port: Option<u16>,
    attempted: bool,
}

impl Forward {
    pub(crate) fn prepare(adb: &Adb, socket: &str) -> Result<Self> {
        let remote = format!("localabstract:{socket}");
        let before = entries(&adb.run(["forward", "--list"])?, adb.serial(), &remote)?;
        Ok(Self {
            remote,
            before,
            port: None,
            attempted: false,
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn for_test(socket: &str, port: u16) -> Self {
        Self {
            remote: format!("localabstract:{socket}"),
            before: Vec::new(),
            port: Some(port),
            attempted: true,
        }
    }

    pub(crate) fn open(&mut self, adb: &Adb) -> Result<u16> {
        self.attempted = true;
        let out = adb.run(["forward", "tcp:0", &self.remote])?;
        let port = parse_forward_port(&out)
            .ok_or_else(|| GlassError::Backend(format!("adb forward gave no port: {out:?}")))?;
        self.port = Some(port);
        Ok(port)
    }

    pub(crate) fn port(&self) -> Option<u16> {
        self.port
    }

    pub(crate) fn remove(&mut self, adb: &Adb, deadline: Deadline) -> Result<()> {
        if !self.attempted {
            return Ok(());
        }
        let port = match self.port {
            Some(port) => port,
            None => {
                let listing = adb.run_until(["forward", "--list"], deadline)?;
                let added: Vec<_> = entries(&listing, adb.serial(), &self.remote)?
                    .into_iter()
                    .filter(|entry| !self.before.contains(entry))
                    .collect();
                match added.as_slice() {
                    [] => {
                        self.attempted = false;
                        return Ok(());
                    }
                    [entry] => entry.port,
                    _ => {
                        return Err(GlassError::Backend(format!(
                            "cannot identify the new adb forward for {}; leaving ambiguous rules untouched",
                            self.remote
                        )));
                    }
                }
            }
        };
        // Retain the discovered port if removal fails, so shutdown can retry it.
        self.port = Some(port);
        if let Err(error) = adb.run_until(["forward", "--remove", &format!("tcp:{port}")], deadline)
        {
            let absent = adb
                .run_until(["forward", "--list"], deadline)
                .and_then(|listing| entries(&listing, adb.serial(), &self.remote))
                .is_ok_and(|entries| entries.iter().all(|entry| entry.port != port));
            if !absent {
                return Err(error);
            }
        }
        self.port = None;
        self.attempted = false;
        Ok(())
    }
}

fn entries(listing: &str, serial: Option<&str>, remote: &str) -> Result<Vec<Entry>> {
    let mut found = Vec::new();
    for line in listing.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('*') {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let [device, local, target] = fields.as_slice() else {
            return Err(GlassError::Backend(format!(
                "invalid adb forward entry: {line:?}"
            )));
        };
        if *target != remote || serial.is_some_and(|serial| serial != *device) {
            continue;
        }
        let Some(local) = local.strip_prefix("tcp:") else {
            continue;
        };
        let port = parse_forward_port(local)
            .ok_or_else(|| GlassError::Backend(format!("invalid adb forward entry: {line:?}")))?;
        found.push(Entry {
            serial: device.to_string(),
            port,
        });
    }
    Ok(found)
}

fn parse_forward_port(out: &str) -> Option<u16> {
    out.lines()
        .map(str::trim)
        .find_map(|line| line.parse::<u16>().ok().filter(|port| *port != 0))
}

pub(crate) fn note_cleanup<T>(step: &str, outcome: &Result<T>) {
    if let Err(error) = outcome
        && !glass_core::note_if_skipped(step, outcome)
    {
        eprintln!("glass: cleanup failed {step}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forward_port() {
        assert_eq!(parse_forward_port("41234\n"), Some(41234));
        assert_eq!(parse_forward_port(""), None);
        assert_eq!(parse_forward_port("0\n"), None);
        assert_eq!(
            parse_forward_port(
                "\n* daemon not running; starting now at tcp:5037\n* daemon started successfully\n41234\n"
            ),
            Some(41234)
        );
    }

    #[test]
    fn forward_list_ignores_blank_lines_and_adb_daemon_notices() {
        let listing = "\n* daemon not running; starting now at tcp:5037\n\
                       * daemon started successfully\n\n\
                       device tcp:4321 localabstract:glass-agent\n";
        let found = entries(listing, Some("device"), "localabstract:glass-agent").unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 4321);
    }

    #[test]
    fn cleanup_diagnostics_report_failures_and_skipped_steps_once() {
        const PROBE: &str = "TEST_ANDROID_CLEANUP_DIAGNOSTIC";
        if let Ok(case) = std::env::var(PROBE) {
            let outcome: Result<()> = match case.as_str() {
                "failure" => Err(GlassError::Backend("device offline".into())),
                "skipped" => Err(GlassError::deadline_not_started("test cleanup")),
                "success" => Ok(()),
                _ => panic!("unknown diagnostic probe {case}"),
            };
            note_cleanup("restoring test state", &outcome);
            return;
        }

        let diagnostic = |case: &str| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "forward::tests::cleanup_diagnostics_report_failures_and_skipped_steps_once",
                    "--nocapture",
                ])
                .env(PROBE, case)
                .output()
                .expect("run the diagnostic probe with stderr capture disabled");
            assert!(output.status.success(), "{case}: {output:?}");
            String::from_utf8(output.stderr).unwrap()
        };

        let failure = diagnostic("failure");
        assert!(
            failure.contains("glass: cleanup failed restoring test state:"),
            "{failure}"
        );
        assert!(failure.contains("device offline"), "{failure}");
        assert_eq!(failure.lines().count(), 1, "{failure}");

        let skipped = diagnostic("skipped");
        assert!(
            skipped.contains("glass: teardown skipped restoring test state"),
            "{skipped}"
        );
        assert_eq!(skipped.lines().count(), 1, "{skipped}");
        assert!(diagnostic("success").is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn malformed_port_removes_only_the_new_rule_on_the_selected_device() {
        use crate::adb::{Answer, FakeAdb};

        let before = "emulator-5554 tcp:4001 localabstract:glass-a11y\n\
                      emulator-5556 tcp:4002 localabstract:glass-a11y\n";
        let after = format!(
            "{before}emulator-5554 tcp:4003 localabstract:glass-a11y\n\
                             emulator-5556 tcp:4004 localabstract:glass-a11y\n\
                             emulator-5554 tcp:4005 localabstract:unrelated\n"
        );
        let fake = FakeAdb::scripted(vec![
            (
                "*forward --list",
                vec![Answer::says(before), Answer::says(after)],
            ),
            ("*forward tcp:0 *", vec![Answer::says("not a port")]),
            ("*", vec![Answer::Silent]),
        ]);
        let adb = fake.adb().with_serial("emulator-5554");
        let mut forward = Forward::prepare(&adb, "glass-a11y").unwrap();
        assert!(forward.open(&adb).is_err());
        forward.remove(&adb, Deadline::UNBOUNDED).unwrap();

        let removals: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|call| call.contains("--remove"))
            .collect();
        assert_eq!(removals, vec!["-s emulator-5554 forward --remove tcp:4003"]);
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_forward_can_still_have_allocated_a_rule() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::scripted(vec![
            (
                "*forward --list",
                vec![
                    Answer::Silent,
                    Answer::says("device tcp:4003 localabstract:glass-agent\n"),
                ],
            ),
            ("*forward tcp:0 *", vec![Answer::fails("reply lost")]),
            ("*", vec![Answer::Silent]),
        ]);
        let mut forward = Forward::prepare(fake.adb(), "glass-agent").unwrap();
        assert!(forward.open(fake.adb()).is_err());
        forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap();
        assert!(
            fake.called("forward --remove tcp:4003"),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn ambiguous_new_forwards_are_retained_without_removing_any_rule() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::scripted(vec![
            (
                "*forward --list",
                vec![
                    Answer::Silent,
                    Answer::says(
                        "device tcp:4003 localabstract:glass-a11y\ndevice tcp:4004 localabstract:glass-a11y\n",
                    ),
                ],
            ),
            ("*forward tcp:0 *", vec![Answer::says("not a port")]),
            ("*", vec![Answer::Silent]),
        ]);
        let mut forward = Forward::prepare(fake.adb(), "glass-a11y").unwrap();
        assert!(forward.open(fake.adb()).is_err());
        let error = forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
        assert!(forward.attempted);
        assert!(!fake.called("--remove"), "{:?}", fake.calls());
    }

    #[test]
    #[cfg(unix)]
    fn listing_failure_keeps_an_unreported_forward_for_a_later_attempt() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::scripted(vec![
            (
                "*forward --list",
                vec![
                    Answer::Silent,
                    Answer::fails("offline"),
                    Answer::says("device tcp:4003 localabstract:glass-a11y\n"),
                ],
            ),
            ("*forward tcp:0 *", vec![Answer::says("not a port")]),
            ("*", vec![Answer::Silent]),
        ]);
        let mut forward = Forward::prepare(fake.adb(), "glass-a11y").unwrap();
        assert!(forward.open(fake.adb()).is_err());
        assert!(forward.remove(fake.adb(), Deadline::UNBOUNDED).is_err());
        forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap();
        assert!(
            fake.called("forward --remove tcp:4003"),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn malformed_listing_does_not_claim_an_unreported_forward_was_removed() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::scripted(vec![
            (
                "*forward --list",
                vec![Answer::Silent, Answer::says("truncated listing")],
            ),
            ("*forward tcp:0 *", vec![Answer::says("not a port")]),
            ("*", vec![Answer::Silent]),
        ]);
        let mut forward = Forward::prepare(fake.adb(), "glass-a11y").unwrap();
        assert!(forward.open(fake.adb()).is_err());
        assert!(forward.remove(fake.adb(), Deadline::UNBOUNDED).is_err());
        assert!(forward.attempted);
        assert!(!fake.called("--remove"), "{:?}", fake.calls());
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_retry_confirms_a_known_forward_is_already_absent() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[
            (
                "*forward --remove *",
                Answer::fails("listener 'tcp:4321' not found"),
            ),
            (
                "*forward --list",
                Answer::says("device tcp:4322 localabstract:glass-agent\n"),
            ),
            ("*", Answer::Silent),
        ]);
        let mut forward = Forward::for_test("glass-agent", 4321);
        forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap();
        let after = fake.calls();
        forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap();

        assert_eq!(fake.calls(), after);
        assert_eq!(forward.port(), None);
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_retry_keeps_a_known_forward_when_absence_cannot_be_confirmed() {
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[
            ("*forward --remove *", Answer::fails("removal failed")),
            ("*forward --list", Answer::fails("listing failed")),
            ("*", Answer::Silent),
        ]);
        let mut forward = Forward::for_test("glass-agent", 4321);
        let error = forward.remove(fake.adb(), Deadline::UNBOUNDED).unwrap_err();

        assert!(error.to_string().contains("removal failed"), "{error}");
        assert!(fake.called("forward --list"));
        assert_eq!(forward.port(), Some(4321));
    }
}

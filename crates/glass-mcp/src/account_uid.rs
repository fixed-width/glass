//! Find an unused account UID without materializing a launchd user domain.

use anyhow::{Context, bail, ensure};
use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_LISTING: usize = 8 * 1024 * 1024;
const UID_END: u32 = 60000;

fn system_listing(command: &mut Command, timeout: Duration) -> anyhow::Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot inspect existing launchd domains")?;
    let stdout = child.stdout.take().context("missing launchd output")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((MAX_LISTING + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok::<_, std::io::Error>(bytes)
    });
    let result = (|| {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(
                    status.success(),
                    "cannot inspect existing launchd domains: {status}"
                );
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "inspection of existing launchd domains timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("launchd output reader stopped"))??;
    result?;
    ensure!(
        bytes.len() <= MAX_LISTING,
        "existing launchd domain listing exceeded its bound"
    );
    String::from_utf8(bytes).context("existing launchd domain listing is not UTF-8")
}

fn parse_domains(listing: &str) -> anyhow::Result<BTreeSet<u32>> {
    ensure!(
        listing.len() <= MAX_LISTING,
        "existing launchd domain listing exceeded its bound"
    );
    ensure!(
        listing.lines().next() == Some("system = {") && listing.lines().last() == Some("}"),
        "incomplete launchd system-domain listing"
    );
    let mut sections = listing.match_indices("\n\tsubdomains = {\n");
    let (start, header) = sections
        .next()
        .context("launchd subdomain listing is missing")?;
    ensure!(
        sections.next().is_none(),
        "ambiguous launchd subdomain listing"
    );
    let mut users = BTreeSet::new();
    let mut domains = BTreeSet::new();
    for line in listing[start + header.len()..].lines() {
        if line == "\t}" {
            return Ok(users);
        }
        let entry = line
            .strip_prefix("\t\t")
            .context("invalid launchd subdomain entry")?;
        let (kind, id) = entry
            .split_once('/')
            .context("invalid launchd subdomain entry")?;
        ensure!(
            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid launchd subdomain identity"
        );
        let id: u32 = id.parse().context("invalid launchd subdomain identity")?;
        ensure!(
            domains.insert((kind, id)) && domains.len() <= 65536,
            "duplicate or excessive launchd subdomains"
        );
        match kind {
            "user" | "gui" => {
                users.insert(id);
            }
            "pid" | "login" | "session" => {}
            _ => bail!("unrecognized launchd subdomain kind"),
        }
    }
    bail!("incomplete launchd subdomain listing")
}

#[cfg(target_os = "macos")]
fn existing_domains() -> anyhow::Result<BTreeSet<u32>> {
    // Inspecting user/<candidate> can create that domain, even for an unallocated UID.
    let listing = system_listing(
        Command::new("/bin/launchctl")
            .args(["print", "system"])
            .env("LC_ALL", "C"),
        Duration::from_secs(10),
    )?;
    parse_domains(&listing)
}

fn select_uid(
    domains: &BTreeSet<u32>,
    mut directory_uid: impl FnMut(u32) -> anyhow::Result<u32>,
    mut has_processes: impl FnMut(u32) -> anyhow::Result<bool>,
) -> anyhow::Result<u32> {
    let mut minimum = 501;
    for _ in 0..64 {
        while domains.contains(&minimum) && minimum < UID_END {
            minimum += 1;
        }
        ensure!(
            minimum < UID_END,
            "no unused development-account UID is available"
        );
        let uid = directory_uid(minimum)?;
        ensure!(
            (minimum..UID_END).contains(&uid),
            "invalid available account UID"
        );
        if !domains.contains(&uid) && !has_processes(uid)? {
            return Ok(uid);
        }
        minimum = uid + 1;
    }
    bail!("account UID availability did not stabilize; no account was created")
}

#[cfg(target_os = "macos")]
pub(crate) fn available() -> anyhow::Result<u32> {
    select_uid(
        &existing_domains()?,
        |minimum| glass_macos::account_setup::available_uid(minimum).map_err(anyhow::Error::msg),
        |uid| Ok(!glass_macos::private_session::account_processes(uid)?.is_empty()),
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn in_use(uid: u32) -> anyhow::Result<bool> {
    Ok(existing_domains()?.contains(&uid)
        || !glass_macos::private_session::account_processes(uid)?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_existing_user_and_gui_domains_reserve_uids() {
        let listing = "system = {\n\tservices = {\n\t\tuser/9000\n\t}\n\tsubdomains = {\n\t\tuser/501\n\t\tgui/600\n\t\tpid/502\n\t\tlogin/100\n\t\tsession/101\n\t}\n}\n";
        assert_eq!(parse_domains(listing).unwrap(), BTreeSet::from([501, 600]));
        assert!(
            parse_domains("system = {\n\tsubdomains = {\n\t}\n}\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unreadable_or_changed_domain_listings_refuse_allocation() {
        for entries in [
            "\t\tuser/-1\n",
            "\t\tuser/4294967296\n",
            "\t\tunknown/502\n",
            "\t\tuser/502\n\t\tuser/502\n",
            "\tuser/502\n",
            "\t\tuser/\n",
        ] {
            assert!(
                parse_domains(&format!(
                    "system = {{\n\tsubdomains = {{\n{entries}\t}}\n}}\n"
                ))
                .is_err()
            );
        }
        for listing in [
            "",
            "system = {\n}\n",
            "system = {\n\tsubdomains = {\n}",
            "user/501 = {\n\tsubdomains = {\n\t}\n}\n",
            "system = {\n\tsubdomains = {\n\t}\n\tsubdomains = {\n\t}\n}\n",
        ] {
            assert!(parse_domains(listing).is_err());
        }
    }

    #[test]
    fn thousands_of_reserved_domains_are_skipped_without_individual_probes() {
        let domains = (501..10000).collect();
        let mut queries = Vec::new();
        let uid = select_uid(
            &domains,
            |minimum| {
                queries.push(minimum);
                Ok(minimum)
            },
            |_| Ok(false),
        )
        .unwrap();
        assert_eq!(uid, 10000);
        assert_eq!(queries, [10000]);
    }

    #[test]
    fn directory_records_and_live_processes_are_also_excluded() {
        let uid = select_uid(
            &BTreeSet::from([504]),
            |minimum| Ok(minimum.max(503)),
            |uid| Ok(uid == 503),
        )
        .unwrap();
        assert_eq!(uid, 505);
    }

    #[test]
    fn failed_or_unstable_queries_do_not_loop_or_assume_a_uid_is_unused() {
        assert!(
            select_uid(
                &BTreeSet::new(),
                |_| bail!("directory unavailable"),
                |_| Ok(false)
            )
            .is_err()
        );
        assert!(
            select_uid(&BTreeSet::new(), Ok, |_| bail!(
                "process inventory unavailable"
            ))
            .is_err()
        );
        assert!(select_uid(&BTreeSet::new(), |_| Ok(500), |_| Ok(false)).is_err());
        assert!(select_uid(&(501..UID_END).collect(), Ok, |_| Ok(false)).is_err());
        let mut calls = 0;
        assert!(
            select_uid(
                &BTreeSet::new(),
                |minimum| {
                    calls += 1;
                    Ok(minimum)
                },
                |_| Ok(true)
            )
            .is_err()
        );
        assert_eq!(calls, 64);
    }

    #[test]
    fn domain_inspection_refuses_failed_oversized_and_stalled_commands() {
        assert!(
            system_listing(
                Command::new("/bin/sh").args(["-c", "exit 1"]),
                Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(
            system_listing(
                Command::new("/bin/sh").args([
                    "-c",
                    "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done"
                ]),
                Duration::from_secs(5)
            )
            .is_err()
        );
        let start = Instant::now();
        assert!(
            system_listing(
                Command::new("/bin/sh").args(["-c", "exec sleep 5"]),
                Duration::from_millis(20)
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires stable launchd user-domain inventory; read-only account allocation"]
    fn native_allocation_does_not_create_user_domains() {
        let before = existing_domains().unwrap();
        for _ in 0..3 {
            let uid = available().unwrap();
            assert!(!before.contains(&uid));
            assert!(!in_use(uid).unwrap());
            assert_eq!(glass_macos::account_setup::available_uid(uid).unwrap(), uid);
        }
        assert_eq!(before, existing_domains().unwrap());
    }
}

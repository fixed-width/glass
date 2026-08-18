//! glass#455: a desktop a11y bus that takes `org.a11y.Bus.GetAddress` and never answers is the one
//! host state this check exists to surface, and the one that was reported in green as "no desktop
//! a11y bus running" — the state where nothing is wrong. Nothing had ever driven it: the unit
//! tests classify facts, and no test had produced the fact.
//!
//! Its own test binary, holding this one test: it points `DBUS_SESSION_BUS_ADDRESS` at a private
//! bus of its own, which every other test's session reads. The operator's real a11y bus is only
//! ever read from, never touched — this one is spawned, wedged and killed here.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use glass_core::CheckStatus;

/// Restores each variable's prior value, not "unset" — this suite runs inside a desktop session
/// that has its own.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

#[allow(unsafe_code)]
impl EnvGuard {
    /// `DBUS_SESSION_BUS_ADDRESS` points the probes at the wedged bus; `AT_SPI_BUS_ADDRESS` is
    /// cleared so the connection probe has to ask it rather than a bus named in the environment.
    fn set(address: &str) -> EnvGuard {
        let prior = ["DBUS_SESSION_BUS_ADDRESS", "AT_SPI_BUS_ADDRESS"]
            .map(|k| (k, std::env::var_os(k)))
            .to_vec();
        // SAFETY: this binary holds exactly one test, so nothing else in the process is reading
        // the environment while it is mutated.
        unsafe {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address);
            std::env::remove_var("AT_SPI_BUS_ADDRESS");
        }
        EnvGuard(prior)
    }
}

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            // SAFETY: as above — one test in this binary.
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

/// A private `dbus-daemon --session`, killed when the test ends — a panic included.
struct PrivateBus(Child);

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl PrivateBus {
    fn start() -> (PrivateBus, String) {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn dbus-daemon");
        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let bus = PrivateBus(child);
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .expect("read the bus address");
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        (bus, address)
    }
}

/// Owns `org.a11y.Bus` and answers `GetAddress` never — a launcher wedged mid-call, which is what
/// an operator whose AT-SPI reads hang actually has.
struct NeverAnswers;

#[zbus::interface(name = "org.a11y.Bus")]
impl NeverAnswers {
    async fn get_address(&self) -> String {
        std::future::pending::<()>().await;
        unreachable!("the call is taken and never answered")
    }
}

/// Wedge `bus`, returning once the name is owned — so a probe that gets "no such name" is a
/// failure of the code under test rather than of the setup.
fn wedge(address: &str) {
    let address = address.to_string();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(address.as_str())
                .expect("address")
                .serve_at("/org/a11y/bus", NeverAnswers)
                .expect("serve")
                .name("org.a11y.Bus")
                .expect("request the name")
                .build()
                .await
                .expect("own org.a11y.Bus");
            tx.send(()).expect("signal the test");
            // Holds the connection — and the name — for as long as the test process lives.
            std::future::pending::<()>().await;
            drop(conn);
        });
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("org.a11y.Bus was never owned");
}

#[test]
#[ignore = "spawns a private session bus and mutates the process environment; run via scripts/test-a11y.sh"]
fn a_host_bus_that_never_answers_is_not_reported_as_a_host_without_one() {
    let (_bus, address) = PrivateBus::start();
    wedge(&address);
    let _env = EnvGuard::set(&address);

    let started = Instant::now();
    let checks = glass_a11y_linux::doctor::checks();
    let waited = started.elapsed();

    let host = checks
        .iter()
        .find(|c| c.name == "host desktop a11y")
        .expect("a host desktop a11y check");
    assert_eq!(host.status, CheckStatus::Warn, "{host:#?}");
    assert!(host.detail.contains("did not answer"), "{}", host.detail);
    assert!(
        host.remedy.is_some(),
        "a warning about a wedged bus has to say what to do about it"
    );
    // Each probe gives up after its own budget; doctor waiting with the bus is the other way this
    // check fails an operator.
    assert!(
        waited < Duration::from_secs(30),
        "doctor waited {waited:?} on a bus that never answers"
    );
}

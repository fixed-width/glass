//! glass#455: a desktop a11y bus that takes `org.a11y.Bus.GetAddress` and never answers was
//! reported in green as "no desktop a11y bus running". Nothing had driven that state: the unit
//! tests classify facts, and no test had produced the fact.
//!
//! Its own test binary because it repoints `DBUS_SESSION_BUS_ADDRESS` for the whole process — the
//! variable every other test's glass session reads. The operator's real a11y bus is never touched:
//! the one wedged here is spawned and killed by this file.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use glass_a11y_linux::doctor::PROBE_BUDGET;
use glass_core::CheckStatus;

/// How long the private bus gets to print its address, and its wedge to own the name — a bound on
/// setup failure, not the one under test.
const SETUP_BUDGET: Duration = Duration::from_secs(10);

/// Restores the variable's prior value, not "unset" — this suite runs inside a desktop session
/// that has its own.
struct EnvGuard(Option<OsString>);

#[allow(unsafe_code)]
impl EnvGuard {
    fn set(address: &str) -> EnvGuard {
        let prior = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
        // SAFETY: this binary holds one test, and this runs before it starts a thread — the
        // process is single-threaded here, so nothing can be reading the environment.
        unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address) };
        EnvGuard(prior)
    }
}

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: doctor's probe threads are detached and still parked on the wedged bus, but
        // they read this variable once, when connecting, which they are long past — zbus resolves
        // the address before the call that hangs them. Nothing else in the process reads it.
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", v) },
            None => unsafe { std::env::remove_var("DBUS_SESSION_BUS_ADDRESS") },
        }
    }
}

/// A private `dbus-daemon --session`, reaped when the test ends — a panic included.
struct PrivateBus(Child);

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // SIGTERM first, so the daemon unlinks its own socket rather than leaving one behind.
        glass_proc_linux::reap_graceful(&mut self.0, glass_proc_linux::REAP_GRACE);
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
        // Bounded: a daemon that starts and prints nothing would otherwise hang the suite.
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(read.map(|_| line));
        });
        let address = rx
            .recv_timeout(SETUP_BUDGET)
            .expect("dbus-daemon printed no address")
            .expect("read the bus address")
            .trim()
            .to_string();
        // Joined, so the process is single-threaded again before the test mutates its environment.
        reader.join().expect("the address reader");
        assert!(!address.is_empty(), "dbus-daemon printed an empty address");
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

/// Wedge `address`, returning once the name is owned — so a probe that gets "no such name" is a
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
    rx.recv_timeout(SETUP_BUDGET)
        .expect("org.a11y.Bus was never owned");
}

#[test]
#[ignore = "spawns a private session bus and mutates the process environment; run via scripts/test-a11y.sh"]
fn a_host_bus_that_never_answers_is_not_reported_as_a_host_without_one() {
    let (_bus, address) = PrivateBus::start();
    let _env = EnvGuard::set(&address);
    wedge(&address);

    let started = Instant::now();
    let checks = glass_a11y_linux::doctor::checks();
    let waited = started.elapsed();

    let host = checks
        .iter()
        .find(|c| c.name == "host desktop a11y")
        .expect("a host desktop a11y check");
    assert_eq!(host.status, CheckStatus::Warn, "{host:#?}");
    // The phrase belongs to this state alone, so a `Warn` from any other one fails here.
    assert!(
        host.detail
            .contains("nothing answered org.a11y.Bus.GetAddress"),
        "{}",
        host.detail
    );
    assert!(
        host.remedy.is_some(),
        "a warning about a bus that answers nothing has to say what to do about it"
    );
    // One budget, not two: the connection is only attempted once an address has been answered.
    assert!(
        waited < PROBE_BUDGET + PROBE_BUDGET / 2,
        "doctor waited {waited:?} on a bus that never answers"
    );
}

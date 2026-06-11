//! `PrivateBus`: a per-session private D-Bus session bus + AT-SPI registry so a
//! launched app publishes an accessibility tree isolated from the host session.
//! Spawns `dbus-daemon --session --print-address` and `at-spi-bus-launcher`, and
//! resolves the a11y-bus address; reaps both on `Drop` (mirrors `glass-x11`'s `Xvfb`).

/// Placeholder; implemented in Task 2.
pub struct PrivateBus;

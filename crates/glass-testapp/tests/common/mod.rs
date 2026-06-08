//! Test-only helper: a private Xvfb for integration tests. Delegates to the
//! production `glass_x11::Xvfb` so there is one implementation; this wrapper just
//! supplies the test screen size and panics on failure (what tests expect).

#![allow(dead_code)]

use std::ops::Deref;

pub struct Xvfb(glass_x11::Xvfb);

impl Xvfb {
    /// Start a private Xvfb for tests (panics on failure, failing the test).
    pub fn start() -> Xvfb {
        Xvfb(glass_x11::Xvfb::start("1024x768x24").expect("spawn Xvfb (is it installed?)"))
    }
}

impl Deref for Xvfb {
    type Target = glass_x11::Xvfb;
    fn deref(&self) -> &glass_x11::Xvfb {
        &self.0
    }
}

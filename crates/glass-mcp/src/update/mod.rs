//! `glass-mcp update`: fetch the latest release, verify it, and replace this binary.

mod release;
mod swap;
#[cfg(test)]
mod testserver;
mod verify;
mod version;

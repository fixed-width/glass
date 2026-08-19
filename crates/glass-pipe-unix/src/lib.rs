//! Reading a child's pipe on a background thread, with a way to stop.
//!
//! A reader that ends only at EOF cannot be ended at all when the child's write end outlives the
//! child: everything the child spawned inherits it, so a process left running holds the pipe open
//! and the reader parks in `read` forever, holding a thread and an fd — accumulating per launch
//! under a long-lived `glass-mcp serve --http`.
//!
//! What a tap adds is an exit that does not depend on the far end: the read is non-blocking, a
//! self-pipe polled alongside the data is the wakeup, and `Drop` joins. Unix rather than Linux
//! because macOS needs the same thing and has no eventfd; Windows cannot share any of it
//! (`glass-windows` carries its own over `PeekNamedPipe`).

#![cfg(unix)]
#![forbid(unsafe_code)]

mod line;
mod tap;
#[cfg(test)]
mod testsup;

pub use line::LineTap;
pub use tap::{ChunkSink, PipeTap};

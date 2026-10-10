//! Read-only qualification diagnostic. It never loads an event-posting API.

#[cfg(target_os = "macos")]
fn main() {
    match glass_macos::window_directed::probe_readiness() {
        Ok(report) => println!("{report:#?}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("window-directed native readiness requires macOS");
    std::process::exit(1);
}

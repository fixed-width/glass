fn main() {
    println!("cargo:rerun-if-changed=src/private_session.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    if !std::env::var("HOST").is_ok_and(|host| host.ends_with("-apple-darwin")) {
        println!(
            "cargo:warning=macOS native bridge requires a Mac build; cross-host metadata checks cover Rust only"
        );
        return;
    }
    cc::Build::new()
        .env(
            "MACOSX_DEPLOYMENT_TARGET",
            std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "14.0".into()),
        )
        .file("src/private_session.m")
        .flag("-fobjc-arc")
        .flag("-std=c11")
        .warnings_into_errors(true)
        .compile("glass_private_session");
    for framework in [
        "Foundation",
        "OpenDirectory",
        "ApplicationServices",
        "Security",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=bsm");
}

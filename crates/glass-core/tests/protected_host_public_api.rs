use glass_core::{
    HostPathAccess, HostPathProtectionMode, ProtectedHostPath, ProtectedHostPathKind,
};

#[test]
fn protected_host_types_are_publicly_reexported() {
    let path = ProtectedHostPath::file("artifact");
    let _: ProtectedHostPathKind = path.kind;
    let _: HostPathProtectionMode = HostPathProtectionMode::SandboxRules;
    let _: HostPathAccess = HostPathAccess::NoActiveTarget;
}

#![windows_subsystem = "windows"]

use std::io::Write;

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(
        window: *mut std::ffi::c_void,
        text: *const u16,
        title: *const u16,
        flags: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
}

fn main() {
    let module: Vec<u16> = "SbieDll.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: the module name remains live and terminated; the borrowed module is not freed.
    println!(
        "sandboxie_loaded={}",
        !unsafe { GetModuleHandleW(module.as_ptr()) }.is_null()
    );
    println!(
        "cwd={}",
        std::env::current_dir()
            .expect("working directory")
            .display()
    );
    if let Some(path) = std::env::args_os().nth(1) {
        println!("protected_read_denied={}", std::fs::read(path).is_err());
    }
    std::io::stdout().flush().expect("flush probe observations");
    let text: Vec<u16> = "Working directory recorded"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let title: Vec<u16> = "Glass Sandboxie cwd probe"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: the strings are terminated and remain live throughout this modal owned window.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0) };
}

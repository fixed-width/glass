//! Native launchd socket ownership for the private worker; no GUI or event dispatch.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};

unsafe extern "C" {
    fn launch_activate_socket(
        name: *const std::ffi::c_char,
        fds: *mut *mut i32,
        count: *mut usize,
    ) -> i32;
    fn free(value: *mut std::ffi::c_void);
    fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
}

pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: the socket is live and both outputs point to writable uid/gid storage.
    let code = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if code == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn activate_listener() -> io::Result<UnixListener> {
    let mut fds = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: launchd fills these outputs with an allocated descriptor array on success.
    let code = unsafe { launch_activate_socket(c"Worker".as_ptr(), &mut fds, &mut count) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if fds.is_null() {
        return Err(io::Error::other("launchd returned no descriptor array"));
    }
    // SAFETY: the successful API result supplies count live descriptors owned by the caller.
    let descriptors = unsafe { std::slice::from_raw_parts(fds, count) };
    let mut owned = Vec::with_capacity(count);
    for descriptor in descriptors {
        // SAFETY: launch_activate_socket transfers each descriptor and no Rust owner exists yet.
        owned.push(unsafe { OwnedFd::from_raw_fd(*descriptor) });
    }
    // SAFETY: launch_activate_socket documents this array as malloc-owned by the caller.
    unsafe { free(fds.cast()) };
    if owned.len() != 1 {
        return Err(io::Error::other(
            "private worker requires exactly one listener",
        ));
    }
    let descriptor = owned
        .pop()
        .ok_or_else(|| io::Error::other("private listener absent"))?;
    Ok(UnixListener::from(descriptor))
}

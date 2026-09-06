use super::*;
use windows::Win32::System::Memory::GlobalFlags;
use windows::Win32::System::WindowsProgramming::{GMEM_INVALID_HANDLE, GMEM_LOCKCOUNT};

fn lock_count(memory: &OwnedHGlobal) -> u32 {
    // SAFETY: the allocation remains owned while its lock count is queried.
    let flags = unsafe { GlobalFlags(memory.handle()) };
    assert_ne!(flags, GMEM_INVALID_HANDLE);
    flags & GMEM_LOCKCOUNT
}

#[test]
fn allocation_initializes_payload_and_padding() {
    for size in [1, 3, 13, 31, 4097] {
        let bytes = vec![0xa5; size];
        let memory = OwnedHGlobal::from_bytes(&bytes).unwrap();
        let lock = memory.lock().unwrap();
        assert_eq!(&lock.as_bytes()[..size], bytes);
        assert!(lock.as_bytes()[size..].iter().all(|&byte| byte == 0));
    }
}

#[test]
fn read_locks_release_independently() {
    let memory = OwnedHGlobal::from_bytes(b"clipboard").unwrap();
    assert_eq!(lock_count(&memory), 0);
    let first = memory.lock().unwrap();
    let second = memory.lock().unwrap();
    assert_eq!(lock_count(&memory), 2);
    drop(first);
    assert_eq!(lock_count(&memory), 1);
    assert_eq!(&second.as_bytes()[..9], b"clipboard");
    drop(second);
    assert_eq!(lock_count(&memory), 0);
}

#[test]
fn unwinding_unlocks_without_freeing_the_owner() {
    let memory = OwnedHGlobal::from_bytes(b"clipboard").unwrap();
    let result = std::panic::catch_unwind(|| {
        let _lock = memory.lock().unwrap();
        assert_eq!(lock_count(&memory), 1);
        panic!("unwind with locked memory");
    });
    assert!(result.is_err());
    assert_eq!(lock_count(&memory), 0);
    assert_eq!(&memory.lock().unwrap().as_bytes()[..9], b"clipboard");
}

#[test]
fn ownership_transfer_keeps_the_allocation_alive() {
    let memory = OwnedHGlobal::from_bytes(b"transferred").unwrap();
    let raw = memory.into_raw();
    // Reclaim the sole ownership transferred above, so the fixture frees the allocation.
    let reclaimed = OwnedHGlobal { h: raw };
    assert_eq!(lock_count(&reclaimed), 0);
    assert_eq!(&reclaimed.lock().unwrap().as_bytes()[..11], b"transferred");
}

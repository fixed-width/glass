//! Typed inline hooks retained for the injected DLL's lifetime.

use std::sync::OnceLock;

use retour::{Error, Function, GenericDetour, HookableWith};

pub(super) struct Detour<T: Function>(OnceLock<GenericDetour<T>>);

impl<T: HookableWith<T>> Detour<T> {
    pub(super) const fn new() -> Self {
        Self(OnceLock::new())
    }

    /// # Safety
    /// Both functions must remain executable for the DLL's lifetime, and `replacement` must
    /// satisfy the target's calling contract. Installation must not race with calls to, or
    /// other modifications of, the target's prologue.
    pub(super) unsafe fn install(&'static self, target: T, replacement: T) -> retour::Result<()> {
        if self.0.get().is_some() {
            return Err(Error::AlreadyInitialized);
        }
        // SAFETY: the caller supplies live functions with matching ABIs and exclusive patching.
        let hook = unsafe { GenericDetour::new(target, replacement) }?;
        self.0.set(hook).map_err(|_| Error::AlreadyInitialized)?;
        let hook = self.0.get().expect("detour was just stored");
        // SAFETY: static storage retains the trampoline and any relay before the patch goes live,
        // including if enabling fails. The caller guarantees exclusive access to the prologue.
        unsafe { hook.enable() }
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use super::*;

    type BinaryFn = unsafe extern "system" fn(usize, usize) -> usize;

    // Keep distinct, patchable bodies and opaque calls even in release builds.
    #[inline(never)]
    unsafe extern "system" fn add(a: usize, b: usize) -> usize {
        black_box(a).wrapping_add(black_box(b))
    }

    #[inline(never)]
    unsafe extern "system" fn subtract(a: usize, b: usize) -> usize {
        black_box(a).wrapping_sub(black_box(b))
    }

    #[inline(never)]
    unsafe extern "system" fn multiply(a: usize, b: usize) -> usize {
        black_box(a).wrapping_mul(black_box(b))
    }

    #[test]
    fn installed_hook_survives_return_and_rejects_reinitialization() {
        static HOOK: Detour<BinaryFn> = Detour::new();
        let target = black_box(add as BinaryFn);
        // SAFETY: these integer-only functions are live for the process lifetime. Only this test
        // calls or patches `add`, and all calls happen outside installation.
        unsafe {
            assert_eq!(target(19, 7), 26);
            HOOK.install(target, subtract).unwrap();
            assert_eq!(target(19, 7), 12);
            assert!(matches!(
                HOOK.install(target, multiply),
                Err(Error::AlreadyInitialized)
            ));
            assert_eq!(target(19, 7), 12);
        }
    }

    #[test]
    fn failed_creation_leaves_hook_available_for_retry() {
        static HOOK: Detour<BinaryFn> = Detour::new();
        let target = black_box(multiply as BinaryFn);
        // SAFETY: only this test calls or patches `multiply`. Both functions have matching ABIs,
        // accept all integer inputs, and stay executable for the process lifetime.
        unsafe {
            assert!(matches!(
                HOOK.install(target, target),
                Err(Error::SameAddress)
            ));
            assert_eq!(target(19, 7), 133);
            HOOK.install(target, subtract).unwrap();
            assert_eq!(target(19, 7), 12);
        }
    }
}

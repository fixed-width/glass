use super::*;

fn can_open_on_another_thread() -> bool {
    std::thread::spawn(|| Clipboard::open().is_ok())
        .join()
        .unwrap()
}

fn assert_closed() {
    // SAFETY: no guard should remain; a leaked open is closed here before failing the assertion.
    assert!(
        unsafe { CloseClipboard() }.is_err(),
        "clipboard was still open"
    );
    assert!(
        can_open_on_another_thread(),
        "local clipboard access remained locked"
    );
}

#[test]
#[ignore = "on-box: needs the interactive Windows session; run serially via a scheduled task from SSH"]
fn clipboard_closes_on_error_and_unwind() {
    let result: Result<()> = (|| {
        let _clipboard = Clipboard::open()?;
        assert!(!can_open_on_another_thread());
        Err(GlassError::Backend("inspection failed".into()))
    })();
    assert!(result.is_err());
    assert_closed();

    let result = std::panic::catch_unwind(|| {
        let _clipboard = Clipboard::open().unwrap();
        assert!(!can_open_on_another_thread());
        panic!("inspection panicked");
    });
    assert!(result.is_err());
    assert_closed();
}

#[test]
#[ignore = "on-box: needs the interactive Windows session; run serially via a scheduled task from SSH"]
fn text_roundtrip_preserves_transferred_and_borrowed_memory() {
    for text in ["", "clipboard 🦀 日本語"] {
        set(text).unwrap();
        assert_eq!(get().unwrap(), text);
        // Reading closes and unlocks the borrowed data without freeing the system's allocation.
        assert_eq!(get().unwrap(), text);
        assert_closed();
    }
}

#[test]
#[ignore = "on-box: needs the interactive Windows session; run serially via a scheduled task from SSH"]
fn missing_text_closes_the_clipboard() {
    {
        let _clipboard = Clipboard::open().unwrap();
        // SAFETY: this fixture owns the open clipboard and holds no data locks.
        unsafe { EmptyClipboard() }.unwrap();
    }
    assert_eq!(get().unwrap(), "");
    assert_closed();
}

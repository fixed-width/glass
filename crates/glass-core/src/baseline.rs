use std::path::PathBuf;

use crate::error::{GlassError, Result};
use crate::frame::Frame;
use crate::image_io::{frame_from_webp, frame_to_webp};

/// Stores named baseline frames as lossless WebP files under a root directory
/// (e.g. `.glass/baselines/`).
pub struct BaselineStore {
    root: PathBuf,
}

impl BaselineStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a baseline's file path, rejecting names that could escape the
    /// root or are otherwise unsafe.
    fn path_for(&self, name: &str) -> Result<PathBuf> {
        let safe = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !safe {
            return Err(GlassError::InvalidName(name.to_string()));
        }
        Ok(self.root.join(format!("{name}.webp")))
    }

    /// Save (overwriting) a baseline.
    pub fn save(&self, name: &str, frame: &Frame) -> Result<()> {
        let path = self.path_for(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, frame_to_webp(frame)?)?;
        Ok(())
    }

    /// Load a baseline, returning `BaselineMissing` if it does not exist.
    pub fn load(&self, name: &str) -> Result<Frame> {
        let path = self.path_for(name)?;
        let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => GlassError::BaselineMissing(name.to_string()),
            _ => GlassError::Io(e),
        })?;
        frame_from_webp(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BaselineStore::new(dir.path().join("baselines"));
        let frame = Frame::solid(3, 2, [50, 60, 70, 255]);
        store.save("main", &frame).unwrap();
        assert_eq!(store.load("main").unwrap(), frame);
    }

    #[test]
    fn load_missing_reports_baseline_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BaselineStore::new(dir.path());
        assert!(matches!(store.load("nope").unwrap_err(), GlassError::BaselineMissing(_)));
    }

    #[test]
    fn rejects_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = BaselineStore::new(dir.path());
        let frame = Frame::solid(1, 1, [0, 0, 0, 255]);
        assert!(matches!(store.save("../escape", &frame).unwrap_err(), GlassError::InvalidName(_)));
        assert!(matches!(store.load("a/b").unwrap_err(), GlassError::InvalidName(_)));
    }
}

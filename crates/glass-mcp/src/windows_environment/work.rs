use super::files::Root;
use anyhow::{Context, Result, ensure};
use glass_core::{Deadline, GlassError};
use glass_windows::containment::{BoxOwnership, ContainmentJournal};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    ownership: BoxOwnership,
    closed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    version: u32,
    instance: String,
    boxes: Vec<Entry>,
}

pub(super) struct Work {
    root: Root,
    store: Mutex<Store>,
}

impl Work {
    pub fn open(root: &Root, instance: &str, create: bool) -> Result<Option<Self>> {
        let record = root.read::<Store>("containers.json")?;
        if record.is_none() && !create {
            return Ok(None);
        }
        let store = record.unwrap_or_else(|| Store {
            version: 1,
            instance: instance.into(),
            boxes: Vec::new(),
        });
        ensure!(
            store.version == 1 && store.instance == instance && store.boxes.len() <= 1024,
            "container ownership journal does not match its environment"
        );
        let copied =
            Root::open(&root.path, false)?.context("container journal directory disappeared")?;
        if create {
            copied.write("containers.json", &store)?;
        }
        Ok(Some(Self {
            root: copied,
            store: Mutex::new(store),
        }))
    }

    pub fn complete(&self) -> Result<bool> {
        Ok(self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("container ownership journal is poisoned"))?
            .boxes
            .iter()
            .all(|entry| entry.closed))
    }

    pub fn recover(&self, deadline: Deadline) -> Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("container ownership journal is poisoned"))?;
        for index in 0..store.boxes.len() {
            if store.boxes[index].closed {
                continue;
            }
            glass_windows::containment::recover_box(&store.boxes[index].ownership, deadline)?;
            store.boxes[index].closed = true;
            self.root.write("containers.json", &*store)?;
        }
        Ok(())
    }
}

impl ContainmentJournal for Work {
    fn created(&self, ownership: &BoxOwnership) -> glass_core::Result<()> {
        let action = || -> Result<()> {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("container ownership journal is poisoned"))?;
            ensure!(
                store.boxes.len() < 1024
                    && !store
                        .boxes
                        .iter()
                        .any(|entry| entry.ownership.box_name == ownership.box_name),
                "container journal is full or the box name was reused"
            );
            let mut updated = store.clone();
            updated.boxes.push(Entry {
                ownership: ownership.clone(),
                closed: false,
            });
            self.root.write("containers.json", &updated)?;
            *store = updated;
            Ok(())
        };
        action().map_err(|error| {
            GlassError::Backend(format!("record container before configuration: {error:#}"))
        })
    }

    fn closed(&self, ownership: &BoxOwnership) -> glass_core::Result<()> {
        let action = || -> Result<()> {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("container ownership journal is poisoned"))?;
            let mut updated = store.clone();
            let entry = updated
                .boxes
                .iter_mut()
                .find(|entry| entry.ownership == *ownership)
                .context("container cleanup has no matching origin record")?;
            entry.closed = true;
            self.root.write("containers.json", &updated)?;
            *store = updated;
            Ok(())
        };
        action()
            .map_err(|error| GlassError::Backend(format!("record container cleanup: {error:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_is_not_confirmed_when_its_durable_record_cannot_be_updated() {
        let scratch = tempfile::tempdir().expect("scratch");
        let root = Root::open(&scratch.path().join("state"), true)
            .expect("private directory")
            .expect("root");
        let work = Work::open(&root, "test-instance", true)
            .expect("journal")
            .expect("work");
        let ownership = BoxOwnership {
            box_name: "glass_testorigin".into(),
            provider_dir: "C:\\Program Files\\Sandboxie".into(),
            log_directory: scratch.path().join("logs"),
        };
        work.created(&ownership)
            .expect("origin must persist before configuration");
        std::fs::hard_link(
            root.path.join("containers.json"),
            scratch.path().join("alias"),
        )
        .expect("alias blocks replacement");
        assert!(work.closed(&ownership).is_err());
        assert!(!work.complete().expect("still pending"));
    }

    #[test]
    fn a_failed_origin_write_does_not_admit_or_publish_a_container() {
        let scratch = tempfile::tempdir().expect("scratch");
        let root = Root::open(&scratch.path().join("state"), true)
            .expect("private directory")
            .expect("root");
        let work = Work::open(&root, "test-instance", true)
            .expect("journal")
            .expect("work");
        std::fs::hard_link(
            root.path.join("containers.json"),
            scratch.path().join("alias"),
        )
        .expect("alias");
        let ownership = BoxOwnership {
            box_name: "glass_testorigin".into(),
            provider_dir: "C:\\Program Files\\Sandboxie".into(),
            log_directory: scratch.path().join("logs"),
        };
        assert!(work.created(&ownership).is_err());
        assert!(work.complete().expect("no container was admitted"));
    }
}

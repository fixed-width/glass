//! Fresh observation scope and synchronous invalidation across output processing.

use std::sync::{Arc, RwLock, Weak};

use super::*;

#[derive(Clone, Debug)]
pub struct ObservationGeneration(Arc<()>);

impl PartialEq for ObservationGeneration {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ObservationGeneration {}

/// Implementations release observation state without reentering the core or epoch.
pub trait ObservationInvalidation: Send + Sync {
    fn invalidate(&self);
}

struct EpochState {
    generation: ObservationGeneration,
    listeners: Vec<Weak<dyn ObservationInvalidation>>,
    context: Option<ObservationContext>,
}

#[derive(Clone)]
pub struct ObservationEpoch(Arc<RwLock<EpochState>>);

impl Default for ObservationEpoch {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(EpochState {
            generation: ObservationGeneration(Arc::new(())),
            listeners: Vec::new(),
            context: None,
        })))
    }
}

impl ObservationEpoch {
    pub fn generation(&self) -> ObservationGeneration {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .generation
            .clone()
    }

    pub fn subscribe(&self, listener: &Arc<dyn ObservationInvalidation>) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .listeners
            .push(Arc::downgrade(listener));
    }

    /// The callback runs under the epoch read guard; do not perform native work or await.
    pub fn if_current<T>(
        &self,
        generation: &ObservationGeneration,
        f: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.0.read().ok()?;
        (&state.generation == generation).then(f)
    }

    pub(super) fn invalidate(&self) {
        let mut state = self.0.write().unwrap_or_else(|e| e.into_inner());
        Self::invalidate_state(&mut state);
    }

    fn invalidate_state(state: &mut EpochState) {
        state.generation = ObservationGeneration(Arc::new(()));
        state.context = None;
        state.listeners.retain(|listener| {
            if let Some(listener) = listener.upgrade() {
                listener.invalidate();
                true
            } else {
                false
            }
        });
    }

    pub(super) fn bind(&self, mut context: ObservationContext) -> ObservationGeneration {
        let mut state = self.0.write().unwrap_or_else(|e| e.into_inner());
        context.generation = state.generation.clone();
        if state
            .context
            .as_ref()
            .is_some_and(|previous| previous != &context)
        {
            Self::invalidate_state(&mut state);
            context.generation = state.generation.clone();
        }
        let generation = context.generation.clone();
        state.context = Some(context);
        generation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationContext {
    pub generation: ObservationGeneration,
    pub backend: String,
    pub window: WindowId,
    pub pids: Vec<u32>,
    pub geometry: WindowGeometry,
    pub limits: WalkLimits,
    pub a11y_bus_addr: Option<String>,
    pub reader_scope: crate::AxObservationScope,
}

pub struct AxObservation {
    pub tree: AxTree,
    pub context: Option<ObservationContext>,
}

impl Glass {
    pub fn observation_epoch(&self) -> ObservationEpoch {
        self.observation_epoch.clone()
    }

    /// Takes one fresh accessibility snapshot and attests scope during that read when possible.
    pub fn a11y_observation(&mut self, max_nodes: Option<usize>) -> Result<AxObservation> {
        self.set_a11y_limits(max_nodes)?;
        let mut context = None;
        let tree = self.snapshot_worker(Deadline::UNBOUNDED, false, Some(&mut context))?;
        if tree.subject.is_some() {
            context = None;
        }
        if context.is_none() {
            self.observation_epoch.invalidate();
        }
        Ok(AxObservation { tree, context })
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    fn platform() -> FakePlatform {
        FakePlatform::new(100, 100).with_windows(vec![
            WindowInfo {
                id: WindowId(1),
                title: None,
                class: None,
                geometry: WindowGeometry {
                    width: 100,
                    height: 100,
                    ..Default::default()
                },
                active: true,
            },
            WindowInfo {
                id: WindowId(2),
                title: None,
                class: None,
                geometry: WindowGeometry {
                    width: 100,
                    height: 100,
                    ..Default::default()
                },
                active: false,
            },
        ])
    }

    #[test]
    fn fresh_observations_share_scope_and_update_immediate_ids() {
        let (mut glass, _, _, starts) = glass_with_a11y_seq_observed(
            platform(),
            vec![fake_tree(), fake_tree()],
            InvokeBehavior::Unsupported,
        );
        glass.start(&spec()).unwrap();
        let first = glass.a11y_observation(None).unwrap();
        let second = glass.a11y_observation(None).unwrap();
        assert_eq!(first.context, second.context);
        assert!(first.context.is_some());
        assert_eq!(starts.lock().unwrap().len(), 2);
        assert_eq!(
            glass.active.as_ref().unwrap().last_ax.as_ref(),
            Some(&second.tree)
        );
    }

    #[test]
    fn current_generation_allows_publication_and_invalidated_generation_refuses_it() {
        let epoch = ObservationEpoch::default();
        let generation = epoch.generation();
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            epoch.if_current(&generation, || {
                calls.set(calls.get() + 1);
                42
            }),
            Some(42)
        );
        epoch.invalidate();
        assert_eq!(
            epoch.if_current(&generation, || {
                calls.set(calls.get() + 1);
                7
            }),
            None
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(epoch.if_current(&epoch.generation(), || 9), Some(9));
    }

    #[test]
    fn ordinary_snapshot_preserves_a_known_observation_episode() {
        let mut glass = glass_with_a11y(platform(), fake_tree());
        glass.start(&spec()).unwrap();
        let first = glass.a11y_observation(None).unwrap().context.unwrap();
        glass.a11y_snapshot(None).unwrap();
        assert_eq!(glass.observation_epoch().generation(), first.generation);
        assert_eq!(
            glass.a11y_observation(None).unwrap().context.unwrap(),
            first
        );
    }

    #[test]
    fn geometry_reads_preserve_continuity_and_resize_attempts_break_it() {
        let mut glass = glass_with_a11y(platform(), fake_tree());
        glass.start(&spec()).unwrap();
        let first = glass.a11y_observation(None).unwrap().context.unwrap();
        glass.window(&WindowOp::Geometry).unwrap();
        assert_eq!(glass.observation_epoch().generation(), first.generation);
        glass
            .window(&WindowOp::Resize {
                width: 100,
                height: 100,
            })
            .unwrap();
        assert_ne!(glass.observation_epoch().generation(), first.generation);
    }

    #[test]
    fn selection_attempts_break_continuity_even_for_equal_geometry_and_failure() {
        let mut glass = glass_with_a11y(platform(), fake_tree());
        glass.start(&spec()).unwrap();
        let a = glass.a11y_observation(None).unwrap().context.unwrap();
        glass.select_window(WindowId(1)).unwrap();
        let reselected = glass.a11y_observation(None).unwrap().context.unwrap();
        assert_ne!(a.generation, reselected.generation);
        glass.select_window(WindowId(2)).unwrap();
        let b = glass.a11y_observation(None).unwrap().context.unwrap();
        assert_eq!(b.window, WindowId(2));
        glass.select_window(WindowId(1)).unwrap();
        assert_ne!(a, glass.a11y_observation(None).unwrap().context.unwrap());
        let before = glass.observation_epoch().generation();
        assert!(glass.select_window(WindowId(999)).is_err());
        assert_ne!(before, glass.observation_epoch().generation());
    }

    #[test]
    fn unknown_window_and_subject_mismatch_cannot_attest_scope() {
        let mut unknown = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        unknown.start(&spec()).unwrap();
        assert!(unknown.a11y_observation(None).unwrap().context.is_none());
        let mut tree = fake_tree();
        tree.subject = Some(crate::Subject {
            asked: "A".into(),
            actual: "B".into(),
        });
        let mut mismatch = glass_with_a11y(platform(), tree);
        mismatch.start(&spec()).unwrap();
        assert!(mismatch.a11y_observation(None).unwrap().context.is_none());
    }

    #[test]
    fn unavailable_post_read_scope_preserves_the_successful_fresh_tree() {
        let mut glass = glass_with_a11y(platform().with_geometry_error_at(2), fake_tree());
        glass.start(&spec()).unwrap();
        let observation = glass.a11y_observation(None).unwrap();
        assert!(observation.context.is_none());
        assert_eq!(
            glass.active.as_ref().unwrap().last_ax.as_ref(),
            Some(&observation.tree)
        );
    }

    #[test]
    fn direct_unproven_read_invalidates_the_previous_observation_episode() {
        let mut glass = glass_with_a11y(platform().with_geometry_error_at(4), fake_tree());
        glass.start(&spec()).unwrap();
        let first = glass.a11y_observation(None).unwrap();
        let generation = first.context.unwrap().generation;
        let second = glass.a11y_observation(None).unwrap();
        assert!(second.context.is_none());
        assert_eq!(
            glass.observation_epoch().if_current(&generation, || true),
            None
        );
        assert_eq!(
            glass.active.as_ref().unwrap().last_ax.as_ref(),
            Some(&second.tree)
        );
    }

    #[test]
    fn unqualified_backend_skips_unnecessary_post_read_probes() {
        let mut glass = glass_with_a11y(
            FakePlatform::new(100, 100).with_geometry_error_at(2),
            fake_tree(),
        );
        glass.start(&spec()).unwrap();
        let observation = glass.a11y_observation(None).unwrap();
        assert!(observation.context.is_none());
        assert!(observation.tree.count > 0);
    }

    #[test]
    fn changing_geometry_during_read_cannot_attest_scope() {
        let platform = platform()
            .resized_to(WindowGeometry {
                width: 100,
                height: 100,
                ..Default::default()
            })
            .resized_to(WindowGeometry {
                width: 120,
                height: 100,
                ..Default::default()
            });
        let mut glass = glass_with_a11y(platform, fake_tree());
        glass.start(&spec()).unwrap();
        assert!(glass.a11y_observation(None).unwrap().context.is_none());
    }

    #[test]
    fn lifecycle_invalidation_releases_registered_state_and_blocks_publication() {
        struct Listener(std::sync::atomic::AtomicUsize);
        impl super::ObservationInvalidation for Listener {
            fn invalidate(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let listener = Arc::new(Listener(std::sync::atomic::AtomicUsize::new(0)));
        let mut glass = glass_with_a11y(platform(), fake_tree());
        glass.start(&spec()).unwrap();
        let epoch = glass.observation_epoch();
        let erased: Arc<dyn super::ObservationInvalidation> = listener.clone();
        epoch.subscribe(&erased);
        let generation = epoch.generation();
        glass.stop().unwrap();
        assert_eq!(listener.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(epoch.if_current(&generation, || true), None);
    }
}

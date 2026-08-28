use crate::messaging_timeout::{
    AxMessaging, TimeoutOwner, with_doctor_timeout_by as production_doctor,
    with_window_query_by as production_window_query,
};
use glass_core::{BoundDispatch, BoundKind, Deadline, GlassError, Result, Whose};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Element {
    SystemWide,
}

#[derive(Default)]
struct State {
    global_timeout: Option<f32>,
    sets: Vec<f32>,
    fail_next_restore: bool,
}

#[derive(Default)]
struct FakeAx {
    state: Mutex<State>,
}

impl FakeAx {
    fn fail_next_restore(&self) {
        self.state.lock().unwrap().fail_next_restore = true;
    }

    fn sets(&self) -> Vec<f32> {
        self.state.lock().unwrap().sets.clone()
    }

    fn active_timeout(&self) -> Option<f32> {
        self.state.lock().unwrap().global_timeout
    }
}

impl AxMessaging for FakeAx {
    type Element = Element;

    fn system_wide_element(&self) -> Self::Element {
        Element::SystemWide
    }

    fn set_messaging_timeout(&self, element: &Self::Element, seconds: f32) -> Result<()> {
        assert_eq!(*element, Element::SystemWide);
        let mut state = self.state.lock().unwrap();
        state.sets.push(seconds);
        if seconds == 0.0 && state.fail_next_restore {
            state.fail_next_restore = false;
            return Err(GlassError::Backend(
                "synthetic AX timeout restore failure".into(),
            ));
        }
        state.global_timeout = (seconds != 0.0).then_some(seconds);
        Ok(())
    }
}

fn assert_same_thread_reentry(error: &GlassError) {
    assert!(
        matches!(
            error.cause(),
            GlassError::Backend(message)
                if message == "macOS AX timeout owner rejected same-thread reentry"
        ),
        "unexpected reentry error: {error}"
    );
    assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
}

#[test]
fn bounded_same_thread_reentry_is_rejected_immediately() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();
    let nested_ran = Mutex::new(false);
    let started = Instant::now();

    let error = owner
        .with_deadline_by(&ax, Deadline::UNBOUNDED, false, |_outer| {
            owner.with_deadline_by(&ax, Deadline::from_millis(2_000), false, |_nested| {
                *nested_ran.lock().unwrap() = true;
                Ok(())
            })
        })
        .unwrap_err();

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "bounded same-thread reentry waited for its deadline"
    );
    assert!(!*nested_ran.lock().unwrap());
    assert_same_thread_reentry(&error);
}

#[test]
fn unbounded_same_thread_reentry_is_rejected_immediately() {
    let owner = Arc::new(TimeoutOwner::new());
    let ax = Arc::new(FakeAx::default());
    let nested_ran = Arc::new(Mutex::new(false));
    let (result_tx, result_rx) = mpsc::channel();
    let worker_owner = Arc::clone(&owner);
    let worker_ax = Arc::clone(&ax);
    let worker_nested_ran = Arc::clone(&nested_ran);
    let worker = thread::spawn(move || {
        let started = Instant::now();
        let result = worker_owner.with_deadline_by(
            worker_ax.as_ref(),
            Deadline::UNBOUNDED,
            false,
            |_outer| {
                worker_owner.with_deadline_by(
                    worker_ax.as_ref(),
                    Deadline::UNBOUNDED,
                    false,
                    |_nested| {
                        *worker_nested_ran.lock().unwrap() = true;
                        Ok(())
                    },
                )
            },
        );
        result_tx.send((started.elapsed(), result)).unwrap();
    });

    let (elapsed, result) = result_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("unbounded same-thread reentry self-deadlocked");
    assert!(
        elapsed < Duration::from_millis(500),
        "unbounded same-thread reentry did not fail immediately"
    );
    let error = result.unwrap_err();
    assert!(!*nested_ran.lock().unwrap());
    assert_same_thread_reentry(&error);
    worker.join().unwrap();
}

#[test]
fn an_exact_object_observes_the_timeout_installed_for_its_message() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();

    owner
        .with_deadline_by(&ax, Deadline::from_millis(1_000), false, |scope| {
            scope.message("exact AX window read", || {
                assert!(ax.active_timeout().is_some());
                Ok(())
            })
        })
        .unwrap();

    assert_eq!(ax.active_timeout(), None);
}

#[test]
fn unbounded_messages_are_serialized_without_changing_the_global_timeout() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();

    owner
        .with_deadline_by(&ax, Deadline::UNBOUNDED, false, |scope| {
            scope.message("unbounded AX read", || {
                assert_eq!(ax.active_timeout(), None);
                Ok(())
            })
        })
        .unwrap();

    assert!(ax.sets().is_empty());
}

#[test]
fn cumulative_budget_refreshes_before_each_message_and_refuses_later_dispatch() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();
    let calls = Mutex::new(0_u8);

    let error = owner
        .with_deadline_by(&ax, Deadline::from_millis(500), false, |scope| {
            for _ in 0..3 {
                scope.message("synthetic AX read", || {
                    *calls.lock().unwrap() += 1;
                    thread::sleep(Duration::from_millis(300));
                    Ok(())
                })?;
            }
            Ok(())
        })
        .unwrap_err();

    assert_eq!(*calls.lock().unwrap(), 2, "a post-deadline AX read ran");
    assert_eq!(error.bound(), Some(BoundKind::TimedOut));
    assert_eq!(error.bound_owner(), Some(Whose::Caller));
    assert_eq!(
        error.bound_dispatch(),
        Some(BoundDispatch::MayHaveDispatched)
    );
    let positive: Vec<_> = ax
        .sets()
        .into_iter()
        .filter(|seconds| *seconds > 0.0)
        .collect();
    assert_eq!(
        positive.len(),
        2,
        "the timeout was not refreshed per AX read"
    );
    assert!(
        positive[1] < positive[0],
        "the second AX read inherited a stale timeout: {positive:?}"
    );
    assert_eq!(ax.active_timeout(), None);
}

#[test]
fn doctor_holds_the_same_owner_while_a_window_scope_expires() {
    let owner = Arc::new(TimeoutOwner::new());
    let ax = Arc::new(FakeAx::default());
    let (doctor_entered_tx, doctor_entered_rx) = mpsc::channel();
    let (release_doctor_tx, release_doctor_rx) = mpsc::channel();
    let doctor_owner = Arc::clone(&owner);
    let doctor_ax = Arc::clone(&ax);
    let doctor = thread::spawn(move || {
        doctor_owner
            .with_doctor_timeout_by(doctor_ax.as_ref(), |scope| {
                scope.message("glass_doctor AX probe", || {
                    doctor_entered_tx.send(()).unwrap();
                    release_doctor_rx.recv().unwrap();
                    Ok(())
                })
            })
            .unwrap();
    });
    doctor_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let operation_ran = Mutex::new(false);
    let error = owner
        .with_deadline_by(ax.as_ref(), Deadline::from_millis(25), false, |scope| {
            scope.message("window AX read", || {
                *operation_ran.lock().unwrap() = true;
                Ok(())
            })
        })
        .unwrap_err();

    assert!(!*operation_ran.lock().unwrap());
    assert_eq!(error.bound(), Some(BoundKind::NotStarted));
    assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    release_doctor_tx.send(()).unwrap();
    doctor.join().unwrap();
}

#[test]
fn doctor_budget_expires_while_waiting_for_an_unbounded_scope() {
    let owner = Arc::new(TimeoutOwner::new());
    let ax = Arc::new(FakeAx::default());
    let (window_entered_tx, window_entered_rx) = mpsc::channel();
    let (release_window_tx, release_window_rx) = mpsc::channel();
    let window_owner = Arc::clone(&owner);
    let window_ax = Arc::clone(&ax);
    let window = thread::spawn(move || {
        window_owner
            .with_deadline_by(window_ax.as_ref(), Deadline::UNBOUNDED, false, |scope| {
                scope.message("unbounded window AX read", || {
                    window_entered_tx.send(()).unwrap();
                    release_window_rx.recv().unwrap();
                    Ok(())
                })
            })
            .unwrap();
    });
    window_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let probe_ran = Arc::new(Mutex::new(false));
    let doctor_owner = Arc::clone(&owner);
    let doctor_ax = Arc::clone(&ax);
    let doctor_probe_ran = Arc::clone(&probe_ran);
    let (result_tx, result_rx) = mpsc::channel();
    let doctor = thread::spawn(move || {
        result_tx
            .send(doctor_owner.with_timeout_by(
                doctor_ax.as_ref(),
                Duration::from_millis(25),
                |scope| {
                    scope.message("glass_doctor AX probe", || {
                        *doctor_probe_ran.lock().unwrap() = true;
                        Ok(())
                    })
                },
            ))
            .unwrap();
    });

    let error = result_rx
        .recv_timeout(Duration::from_millis(150))
        .expect("doctor remained blocked behind an unbounded AX scope")
        .unwrap_err();
    assert!(!*probe_ran.lock().unwrap());
    assert_eq!(error.bound(), Some(BoundKind::NotStarted));
    assert_eq!(error.bound_owner(), Some(Whose::Caller));
    assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    release_window_tx.send(()).unwrap();
    window.join().unwrap();
    doctor.join().unwrap();
}

#[test]
fn doctor_waits_for_a_window_restore_before_installing_its_timeout() {
    let owner = Arc::new(TimeoutOwner::new());
    let ax = Arc::new(FakeAx::default());
    let (window_entered_tx, window_entered_rx) = mpsc::channel();
    let (release_window_tx, release_window_rx) = mpsc::channel();
    let window_owner = Arc::clone(&owner);
    let window_ax = Arc::clone(&ax);
    let window = thread::spawn(move || {
        window_owner
            .with_deadline_by(
                window_ax.as_ref(),
                Deadline::from_millis(2_000),
                false,
                |scope| {
                    scope.message("window AX read", || {
                        window_entered_tx.send(()).unwrap();
                        release_window_rx.recv().unwrap();
                        Ok(())
                    })
                },
            )
            .unwrap();
    });
    window_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (doctor_entered_tx, doctor_entered_rx) = mpsc::channel();
    let doctor_owner = Arc::clone(&owner);
    let doctor_ax = Arc::clone(&ax);
    let doctor = thread::spawn(move || {
        doctor_owner
            .with_doctor_timeout_by(doctor_ax.as_ref(), |scope| {
                scope.message("glass_doctor AX probe", || {
                    doctor_entered_tx.send(()).unwrap();
                    Ok(())
                })
            })
            .unwrap();
    });

    assert!(
        doctor_entered_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "glass_doctor overlapped the active window timeout scope"
    );
    release_window_tx.send(()).unwrap();
    window.join().unwrap();
    doctor_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    doctor.join().unwrap();

    let sets = ax.sets();
    assert_eq!(
        sets.len(),
        4,
        "expected window set/reset then doctor set/reset"
    );
    assert!(sets[0] > 0.0);
    assert_eq!(sets[1], 0.0);
    assert!(
        sets[2] > 0.0 && sets[2] < 2.0,
        "doctor did not receive the remaining gate-wait budget: {sets:?}"
    );
    assert_eq!(sets[3], 0.0);
}

#[test]
fn production_doctor_and_window_wrappers_share_one_owner_in_both_interleavings() {
    let ax = Arc::new(FakeAx::default());
    let (doctor_entered_tx, doctor_entered_rx) = mpsc::channel();
    let (release_doctor_tx, release_doctor_rx) = mpsc::channel();
    let doctor_ax = Arc::clone(&ax);
    let doctor = thread::spawn(move || {
        production_doctor(doctor_ax.as_ref(), |scope| {
            scope.message("production doctor probe", || {
                doctor_entered_tx.send(()).unwrap();
                release_doctor_rx.recv().unwrap();
                Ok(())
            })
        })
        .unwrap();
    });
    doctor_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let window_ran = Mutex::new(false);
    let error = production_window_query(
        ax.as_ref(),
        Deadline::from_millis(25),
        || Ok(()),
        |(), scope| {
            scope.message("production window read", || {
                *window_ran.lock().unwrap() = true;
                Ok(())
            })
        },
    )
    .unwrap_err();
    assert!(!*window_ran.lock().unwrap());
    assert_eq!(error.bound(), Some(BoundKind::TimedOut));
    assert_eq!(
        error.bound_dispatch(),
        Some(BoundDispatch::MayHaveDispatched)
    );
    release_doctor_tx.send(()).unwrap();
    doctor.join().unwrap();

    let (window_entered_tx, window_entered_rx) = mpsc::channel();
    let (release_window_tx, release_window_rx) = mpsc::channel();
    let window_ax = Arc::clone(&ax);
    let window = thread::spawn(move || {
        production_window_query(
            window_ax.as_ref(),
            Deadline::from_millis(2_000),
            || Ok(()),
            |(), scope| {
                scope.message("production window read", || {
                    window_entered_tx.send(()).unwrap();
                    release_window_rx.recv().unwrap();
                    Ok(())
                })
            },
        )
        .unwrap();
    });
    window_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (second_doctor_entered_tx, second_doctor_entered_rx) = mpsc::channel();
    let second_doctor_ax = Arc::clone(&ax);
    let second_doctor = thread::spawn(move || {
        production_doctor(second_doctor_ax.as_ref(), |scope| {
            scope.message("production doctor probe", || {
                second_doctor_entered_tx.send(()).unwrap();
                Ok(())
            })
        })
        .unwrap();
    });
    assert!(
        second_doctor_entered_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "production doctor wrapper bypassed the window owner's gate"
    );
    release_window_tx.send(()).unwrap();
    window.join().unwrap();
    second_doctor_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    second_doctor.join().unwrap();
}

#[test]
fn failed_restore_contaminates_the_owner_and_blocks_later_scopes() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();
    ax.fail_next_restore();

    let first = owner
        .with_deadline_by(&ax, Deadline::from_millis(1_000), false, |scope| {
            scope.message("window AX read", || Ok(()))
        })
        .unwrap_err();
    assert!(first.to_string().contains("restore"), "{first}");
    assert_eq!(
        first.bound_dispatch(),
        Some(BoundDispatch::MayHaveDispatched),
        "the AX read dispatched before restoration failed"
    );

    let later_ran = Mutex::new(false);
    let later = owner
        .with_doctor_timeout_by(&ax, |scope| {
            scope.message("glass_doctor AX probe", || {
                *later_ran.lock().unwrap() = true;
                Ok(())
            })
        })
        .unwrap_err();
    assert!(!*later_ran.lock().unwrap());
    assert!(later.to_string().contains("contaminated"), "{later}");
    assert!(later.to_string().contains("restart"), "{later}");
}

#[test]
fn failed_restore_during_unwind_still_blocks_later_scopes() {
    let owner = TimeoutOwner::new();
    let ax = FakeAx::default();
    ax.fail_next_restore();

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = owner.with_deadline_by(
            &ax,
            Deadline::from_millis(1_000),
            false,
            |scope| -> Result<()> {
                scope.message("window AX read", || -> Result<()> {
                    panic!("synthetic operation panic")
                })
            },
        );
    }));
    assert!(panic.is_err());

    let later = owner
        .with_deadline_by(&ax, Deadline::from_millis(1_000), false, |_scope| Ok(()))
        .unwrap_err();
    assert!(later.to_string().contains("contaminated"), "{later}");
}

#[test]
fn failed_restore_wakes_every_waiter_to_report_contamination() {
    let owner = Arc::new(TimeoutOwner::new());
    let ax = Arc::new(FakeAx::default());
    ax.fail_next_restore();
    let (blocker_entered_tx, blocker_entered_rx) = mpsc::channel();
    let (release_blocker_tx, release_blocker_rx) = mpsc::channel();
    let blocker_owner = Arc::clone(&owner);
    let blocker_ax = Arc::clone(&ax);
    let blocker = thread::spawn(move || {
        blocker_owner
            .with_deadline_by(
                blocker_ax.as_ref(),
                Deadline::from_millis(2_000),
                false,
                |scope| {
                    scope.message("window AX read", || {
                        blocker_entered_tx.send(()).unwrap();
                        release_blocker_rx.recv().unwrap();
                        Ok(())
                    })
                },
            )
            .unwrap_err()
    });
    blocker_entered_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (waiters_started_tx, waiters_started_rx) = mpsc::channel();
    let (results_tx, results_rx) = mpsc::channel();
    let mut waiters = Vec::new();
    for _ in 0..2 {
        let waiter_owner = Arc::clone(&owner);
        let waiter_ax = Arc::clone(&ax);
        let started = waiters_started_tx.clone();
        let results = results_tx.clone();
        waiters.push(thread::spawn(move || {
            started.send(()).unwrap();
            let result = waiter_owner.with_deadline_by(
                waiter_ax.as_ref(),
                Deadline::from_millis(500),
                false,
                |_scope| Ok(()),
            );
            results.send(result).unwrap();
        }));
    }
    waiters_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    waiters_started_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    thread::sleep(Duration::from_millis(25));

    release_blocker_tx.send(()).unwrap();
    let blocker_error = blocker.join().unwrap();
    assert!(blocker_error.to_string().contains("contaminated"));

    let first = results_rx
        .recv_timeout(Duration::from_millis(150))
        .expect("first waiter stayed asleep")
        .unwrap_err();
    let second = results_rx
        .recv_timeout(Duration::from_millis(150))
        .expect("second waiter stayed asleep")
        .unwrap_err();
    assert!(first.to_string().contains("contaminated"), "{first}");
    assert!(second.to_string().contains("contaminated"), "{second}");
    for waiter in waiters {
        waiter.join().unwrap();
    }
}

use std::thread;
use std::time::Duration;

use super::{
    ChildOutcome, ChildStatus, ForceTermination, InterruptDelivery, InterruptKind, InterruptPath,
    ProcessFailure,
};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const FORCE_CONFIRM_POLLS: usize = 600;
const FAILURE_RECHECK_POLLS: usize = 20;
const FORCE_ATTEMPTS: usize = 2;

pub(super) trait Backend {
    type Event: Copy;

    fn pending_events(&mut self) -> Vec<Self::Event>;
    fn event_kind(&self, event: Self::Event) -> InterruptKind;
    fn probe(&mut self) -> Probe;
    fn deliver_first(&mut self, event: Self::Event) -> InterruptDelivery;
    fn force(&mut self) -> ForceTermination;
    fn begin_finalization(&mut self) -> Result<(), Vec<Self::Event>>;
    fn timeout_failure(&self) -> ProcessFailure;
    fn disarm(&mut self);

    fn pause(&mut self) {
        thread::sleep(POLL_INTERVAL);
    }
}

pub(super) enum Probe {
    Running,
    ProvenDead(ChildStatus),
    Uncertain(ProcessFailure),
}

pub(super) enum DriverResult {
    Proven {
        child: ChildOutcome,
        interrupt: InterruptPath,
        failures: Vec<ProcessFailure>,
        permit: CleanupPermit,
    },
    Uncertain {
        failures: Vec<ProcessFailure>,
        interrupt: InterruptPath,
    },
}

pub(super) struct CleanupPermit {
    _proof: (),
}

impl CleanupPermit {
    pub(super) const fn without_child() -> Self {
        Self { _proof: () }
    }

    const fn after_proof() -> Self {
        Self { _proof: () }
    }
}

pub(super) fn supervise<B: Backend>(backend: &mut B) -> DriverResult {
    let mut interrupt = InterruptPath::None;

    loop {
        for event in backend.pending_events() {
            match handle_event(backend, event, interrupt) {
                EventResult::Continue(path) => interrupt = path,
                EventResult::Complete(result) => return result,
            }
        }

        match backend.probe() {
            Probe::Running => backend.pause(),
            Probe::ProvenDead(status) => {
                return proven(backend, ChildOutcome::Exited(status), interrupt, Vec::new());
            }
            Probe::Uncertain(failure) => {
                let primary = failure.clone();
                let forced = force_and_confirm(backend, vec![failure]);
                return match forced.status {
                    Some(_status) => proven(
                        backend,
                        ChildOutcome::Failed(primary),
                        interrupt,
                        forced.failures,
                    ),
                    None => uncertain(backend, interrupt, forced.failures),
                };
            }
        }
    }
}

enum EventResult {
    Continue(InterruptPath),
    Complete(DriverResult),
}

fn handle_event<B: Backend>(backend: &mut B, event: B::Event, path: InterruptPath) -> EventResult {
    let kind = backend.event_kind(event);
    match backend.probe() {
        Probe::ProvenDead(status) => {
            let path = match path {
                InterruptPath::None => InterruptPath::Graceful {
                    interrupt: kind,
                    delivery: backend.deliver_first(event),
                },
                path => record_already_exited(path, kind),
            };
            EventResult::Complete(proven(
                backend,
                ChildOutcome::Exited(status),
                path,
                Vec::new(),
            ))
        }
        Probe::Uncertain(failure) => {
            let delivery_failure = failure.clone();
            let forced = force_and_confirm(backend, vec![failure]);
            let path = record_force(
                path,
                kind,
                forced.termination.clone(),
                Some(delivery_failure),
            );
            match forced.status {
                Some(status) => EventResult::Complete(proven(
                    backend,
                    ChildOutcome::Exited(status),
                    path,
                    forced.failures,
                )),
                None => EventResult::Complete(uncertain(backend, path, forced.failures)),
            }
        }
        Probe::Running => match path {
            InterruptPath::None => {
                let delivery = backend.deliver_first(event);
                let path = InterruptPath::Graceful {
                    interrupt: kind,
                    delivery: delivery.clone(),
                };
                if let InterruptDelivery::Failed(failure) = delivery {
                    match backend.probe() {
                        Probe::ProvenDead(status) => EventResult::Complete(proven(
                            backend,
                            ChildOutcome::Exited(status),
                            InterruptPath::Graceful {
                                interrupt: kind,
                                delivery: InterruptDelivery::ChildAlreadyExited,
                            },
                            Vec::new(),
                        )),
                        Probe::Running => EventResult::Continue(path),
                        Probe::Uncertain(probe_failure) => {
                            let forced = force_and_confirm(backend, vec![failure, probe_failure]);
                            match forced.status {
                                Some(status) => EventResult::Complete(proven(
                                    backend,
                                    ChildOutcome::Exited(status),
                                    path,
                                    forced.failures,
                                )),
                                None => {
                                    EventResult::Complete(uncertain(backend, path, forced.failures))
                                }
                            }
                        }
                    }
                } else {
                    EventResult::Continue(path)
                }
            }
            InterruptPath::Graceful { .. } | InterruptPath::Forced { .. } => {
                let forced = force_and_confirm(backend, Vec::new());
                let path = record_force(path, kind, forced.termination.clone(), None);
                match forced.status {
                    Some(status) => EventResult::Complete(proven(
                        backend,
                        ChildOutcome::Exited(status),
                        path,
                        forced.failures,
                    )),
                    None => EventResult::Complete(uncertain(backend, path, forced.failures)),
                }
            }
        },
    }
}

struct ForceResult {
    status: Option<ChildStatus>,
    termination: ForceTermination,
    failures: Vec<ProcessFailure>,
}

fn force_and_confirm<B: Backend>(
    backend: &mut B,
    mut failures: Vec<ProcessFailure>,
) -> ForceResult {
    let mut last = None;

    for _ in 0..FORCE_ATTEMPTS {
        let termination = backend.force();
        last = Some(termination.clone());
        let polls = if matches!(termination, ForceTermination::Terminated) {
            FORCE_CONFIRM_POLLS
        } else {
            FAILURE_RECHECK_POLLS
        };
        let failed = match &termination {
            ForceTermination::Failed(failure) => Some(failure.clone()),
            _ => None,
        };
        let mut probe_failed = false;

        for _ in 0..polls {
            match backend.probe() {
                Probe::ProvenDead(status) => {
                    return ForceResult {
                        status: Some(status),
                        termination: if matches!(termination, ForceTermination::Terminated) {
                            ForceTermination::Terminated
                        } else {
                            ForceTermination::ChildAlreadyExited
                        },
                        failures,
                    };
                }
                Probe::Running => backend.pause(),
                Probe::Uncertain(failure) => {
                    if let Some(failed) = failed.clone() {
                        failures.push(failed);
                    }
                    failures.push(failure);
                    probe_failed = true;
                    break;
                }
            }
        }

        if !probe_failed {
            if let Some(failed) = failed {
                failures.push(failed);
            } else if matches!(termination, ForceTermination::Terminated) {
                failures.push(backend.timeout_failure());
            }
        }
    }

    ForceResult {
        status: None,
        termination: last.unwrap_or_else(|| ForceTermination::Failed(backend.timeout_failure())),
        failures,
    }
}

fn proven<B: Backend>(
    backend: &mut B,
    child: ChildOutcome,
    mut interrupt: InterruptPath,
    failures: Vec<ProcessFailure>,
) -> DriverResult {
    loop {
        match backend.begin_finalization() {
            Ok(()) => {
                backend.disarm();
                return DriverResult::Proven {
                    child,
                    interrupt,
                    failures,
                    permit: CleanupPermit::after_proof(),
                };
            }
            Err(events) => {
                for event in events {
                    interrupt = record_already_exited(interrupt, backend.event_kind(event));
                }
            }
        }
    }
}

fn uncertain<B: Backend>(
    backend: &mut B,
    mut interrupt: InterruptPath,
    mut failures: Vec<ProcessFailure>,
) -> DriverResult {
    loop {
        match backend.begin_finalization() {
            Ok(()) => {
                if failures.is_empty() {
                    failures.push(backend.timeout_failure());
                }
                return DriverResult::Uncertain {
                    failures,
                    interrupt,
                };
            }
            Err(events) => {
                for event in events {
                    let kind = backend.event_kind(event);
                    let initial_failure = failures.first().cloned();
                    let forced = force_and_confirm(backend, failures);
                    interrupt =
                        record_force(interrupt, kind, forced.termination.clone(), initial_failure);
                    if let Some(status) = forced.status {
                        return proven(
                            backend,
                            ChildOutcome::Exited(status),
                            interrupt,
                            forced.failures,
                        );
                    }
                    failures = forced.failures;
                }
            }
        }
    }
}

fn record_already_exited(path: InterruptPath, kind: InterruptKind) -> InterruptPath {
    match path {
        InterruptPath::None => InterruptPath::Graceful {
            interrupt: kind,
            delivery: InterruptDelivery::ChildAlreadyExited,
        },
        InterruptPath::Graceful {
            interrupt: first,
            delivery,
        } => InterruptPath::Forced {
            first,
            delivery,
            second: kind,
            termination: ForceTermination::ChildAlreadyExited,
        },
        forced @ InterruptPath::Forced { .. } => forced,
    }
}

fn record_force(
    path: InterruptPath,
    kind: InterruptKind,
    termination: ForceTermination,
    initial_failure: Option<ProcessFailure>,
) -> InterruptPath {
    match path {
        InterruptPath::None => InterruptPath::Forced {
            first: kind,
            delivery: InterruptDelivery::Failed(initial_failure.unwrap_or_else(
                || match &termination {
                    ForceTermination::Failed(failure) => failure.clone(),
                    _ => unreachable!("forcing without a first event requires failure context"),
                },
            )),
            second: kind,
            termination,
        },
        InterruptPath::Graceful {
            interrupt: first,
            delivery,
        } => InterruptPath::Forced {
            first,
            delivery,
            second: kind,
            termination,
        },
        InterruptPath::Forced {
            first,
            delivery,
            second,
            ..
        } => InterruptPath::Forced {
            first,
            delivery,
            second,
            termination,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::path::Path;

    use super::*;
    use crate::process::ProcessStage;

    struct ScriptedBackend {
        events: VecDeque<Vec<InterruptKind>>,
        probes: VecDeque<Probe>,
        force: VecDeque<ForceTermination>,
        delivery: InterruptDelivery,
        finalization_events: VecDeque<Vec<InterruptKind>>,
        finalized: bool,
        disarmed: bool,
    }

    impl Backend for ScriptedBackend {
        type Event = InterruptKind;

        fn pending_events(&mut self) -> Vec<Self::Event> {
            self.events.pop_front().unwrap_or_default()
        }

        fn event_kind(&self, event: Self::Event) -> InterruptKind {
            event
        }

        fn probe(&mut self) -> Probe {
            self.probes.pop_front().unwrap_or(Probe::Running)
        }

        fn deliver_first(&mut self, _event: Self::Event) -> InterruptDelivery {
            self.delivery.clone()
        }

        fn force(&mut self) -> ForceTermination {
            self.force.pop_front().unwrap_or_else(|| {
                ForceTermination::Failed(failure(ProcessStage::ForceTermination))
            })
        }

        fn begin_finalization(&mut self) -> Result<(), Vec<Self::Event>> {
            if let Some(events) = self.finalization_events.pop_front() {
                if !events.is_empty() {
                    return Err(events);
                }
            }
            self.finalized = true;
            Ok(())
        }

        fn timeout_failure(&self) -> ProcessFailure {
            failure(ProcessStage::Wait)
        }

        fn disarm(&mut self) {
            self.disarmed = true;
        }

        fn pause(&mut self) {}
    }

    #[test]
    fn uncertain_liveness_never_produces_cleanup_permission() {
        let wait = failure(ProcessStage::Wait);
        let force_one = failure(ProcessStage::ForceTermination);
        let force_two = failure(ProcessStage::ForceTermination);
        let mut backend = ScriptedBackend {
            events: VecDeque::new(),
            probes: VecDeque::from([
                Probe::Uncertain(wait.clone()),
                Probe::Running,
                Probe::Running,
            ]),
            force: VecDeque::from([
                ForceTermination::Failed(force_one.clone()),
                ForceTermination::Failed(force_two.clone()),
            ]),
            delivery: InterruptDelivery::DeliveredByPlatform,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Uncertain { failures, .. } = result else {
            panic!("uncertain child liveness must not produce termination proof");
        };
        assert_eq!(failures, [wait, force_one, force_two]);
        assert!(backend.finalized);
        assert!(!backend.disarmed);
    }

    #[test]
    fn uncertain_preflight_event_forces_and_confirms_death() {
        let wait = failure(ProcessStage::Wait);
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt]]),
            probes: VecDeque::from([
                Probe::Uncertain(wait.clone()),
                Probe::ProvenDead(ChildStatus::Exited(0)),
            ]),
            force: VecDeque::from([ForceTermination::Terminated]),
            delivery: InterruptDelivery::DeliveredByPlatform,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            failures,
            interrupt,
            ..
        } = result
        else {
            panic!("post-force death proof should permit orderly finalization");
        };
        assert_eq!(failures, [wait]);
        assert!(matches!(interrupt, InterruptPath::Forced { .. }));
        assert!(backend.finalized);
        assert!(backend.disarmed);
    }

    #[test]
    fn platform_delivery_is_retained_when_the_child_exits_before_event_drain() {
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Break]]),
            probes: VecDeque::from([Probe::ProvenDead(ChildStatus::Exited(0))]),
            force: VecDeque::new(),
            delivery: InterruptDelivery::DeliveredByPlatform,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven { interrupt, .. } = result else {
            panic!("a reaped child is proven dead");
        };
        assert_eq!(
            interrupt,
            InterruptPath::Graceful {
                interrupt: InterruptKind::Break,
                delivery: InterruptDelivery::DeliveredByPlatform,
            }
        );
        assert!(backend.finalized);
        assert!(backend.disarmed);
    }

    #[test]
    fn event_during_uncertain_finalization_is_forced_and_recorded() {
        let wait = failure(ProcessStage::Wait);
        let force_one = failure(ProcessStage::ForceTermination);
        let force_two = failure(ProcessStage::ForceTermination);
        let mut probes = VecDeque::from([Probe::Uncertain(wait.clone())]);
        probes.extend((0..FAILURE_RECHECK_POLLS * 2).map(|_| Probe::Running));
        probes.push_back(Probe::ProvenDead(ChildStatus::Exited(0)));
        let mut backend = ScriptedBackend {
            events: VecDeque::new(),
            probes,
            force: VecDeque::from([
                ForceTermination::Failed(force_one.clone()),
                ForceTermination::Failed(force_two.clone()),
                ForceTermination::Terminated,
            ]),
            delivery: InterruptDelivery::DeliveredByPlatform,
            finalization_events: VecDeque::from([vec![InterruptKind::Interrupt], Vec::new()]),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            failures,
            interrupt,
            ..
        } = result
        else {
            panic!("the finalization event should trigger force and a death proof");
        };
        assert_eq!(failures, [wait.clone(), force_one, force_two]);
        assert_eq!(
            interrupt,
            InterruptPath::Forced {
                first: InterruptKind::Interrupt,
                delivery: InterruptDelivery::Failed(wait),
                second: InterruptKind::Interrupt,
                termination: ForceTermination::Terminated,
            }
        );
        assert!(backend.finalized);
        assert!(backend.disarmed);
    }

    fn failure(stage: ProcessStage) -> ProcessFailure {
        ProcessFailure::from_io(
            stage,
            Path::new("fixture-agent"),
            None,
            &io::Error::other("fixture failure"),
        )
    }
}

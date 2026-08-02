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
    // Death proof seals the process identity: this classification must not perform delivery.
    fn classify_after_proof(&self, event: Self::Event) -> InterruptDelivery;
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
                let forced =
                    force_and_confirm(backend, vec![failure], ProvenChild::Failed(primary));
                let interrupt = record_failure_force(interrupt, forced.termination.clone());
                return match forced.status {
                    Some(status) => proven(
                        backend,
                        forced.proven_child.resolve(status),
                        interrupt,
                        forced.failures,
                    ),
                    None => uncertain(backend, interrupt, forced.failures, forced.proven_child),
                };
            }
        }
    }
}

pub(super) fn terminate_after_failure<B: Backend>(
    backend: &mut B,
    failure: ProcessFailure,
) -> DriverResult {
    let primary = failure.clone();
    let forced = force_and_confirm(backend, vec![failure], ProvenChild::Failed(primary));
    match forced.status {
        Some(status) => proven(
            backend,
            forced.proven_child.resolve(status),
            InterruptPath::None,
            forced.failures,
        ),
        None => uncertain(
            backend,
            InterruptPath::None,
            forced.failures,
            forced.proven_child,
        ),
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
                    delivery: backend.classify_after_proof(event),
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
            let primary = failure.clone();
            let delivery_failure = failure.clone();
            let forced = force_and_confirm(backend, vec![failure], ProvenChild::Failed(primary));
            let path = record_event_force(
                path,
                kind,
                forced.termination.clone(),
                Some(delivery_failure),
            );
            match forced.status {
                Some(status) => EventResult::Complete(proven(
                    backend,
                    forced.proven_child.resolve(status),
                    path,
                    forced.failures,
                )),
                None => EventResult::Complete(uncertain(
                    backend,
                    path,
                    forced.failures,
                    forced.proven_child,
                )),
            }
        }
        Probe::Running => match path {
            InterruptPath::None => handle_first_running_event(backend, event, kind),
            InterruptPath::Graceful { .. } | InterruptPath::Forced { .. } => {
                let forced = force_and_confirm(backend, Vec::new(), ProvenChild::Exited);
                let path = record_event_force(path, kind, forced.termination.clone(), None);
                match forced.status {
                    Some(status) => EventResult::Complete(proven(
                        backend,
                        forced.proven_child.resolve(status),
                        path,
                        forced.failures,
                    )),
                    None => EventResult::Complete(uncertain(
                        backend,
                        path,
                        forced.failures,
                        forced.proven_child,
                    )),
                }
            }
        },
    }
}

fn handle_first_running_event<B: Backend>(
    backend: &mut B,
    event: B::Event,
    kind: InterruptKind,
) -> EventResult {
    let delivery = backend.deliver_first(event);
    let path = InterruptPath::Graceful {
        interrupt: kind,
        delivery: delivery.clone(),
    };
    let InterruptDelivery::Failed(failure) = delivery else {
        return EventResult::Continue(path);
    };

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
        Probe::Running => {
            complete_after_delivery_failure(backend, path, vec![failure], ProvenChild::Exited)
        }
        Probe::Uncertain(probe_failure) => {
            let primary = probe_failure.clone();
            complete_after_delivery_failure(
                backend,
                path,
                vec![failure, probe_failure],
                ProvenChild::Failed(primary),
            )
        }
    }
}

fn complete_after_delivery_failure<B: Backend>(
    backend: &mut B,
    path: InterruptPath,
    failures: Vec<ProcessFailure>,
    proven_child: ProvenChild,
) -> EventResult {
    let forced = force_and_confirm(backend, failures, proven_child);
    let path = record_failure_force(path, forced.termination.clone());
    match forced.status {
        Some(status) => EventResult::Complete(proven(
            backend,
            forced.proven_child.resolve(status),
            path,
            forced.failures,
        )),
        None => EventResult::Complete(uncertain(
            backend,
            path,
            forced.failures,
            forced.proven_child,
        )),
    }
}

struct ForceResult {
    status: Option<ChildStatus>,
    termination: ForceTermination,
    failures: Vec<ProcessFailure>,
    proven_child: ProvenChild,
}

fn force_and_confirm<B: Backend>(
    backend: &mut B,
    mut failures: Vec<ProcessFailure>,
    mut proven_child: ProvenChild,
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
                    if let Some(failed) = failed.clone() {
                        failures.push(failed);
                    }
                    return ForceResult {
                        status: Some(status),
                        termination,
                        failures,
                        proven_child,
                    };
                }
                Probe::Running => backend.pause(),
                Probe::Uncertain(failure) => {
                    proven_child.record_failure(&failure);
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
                let timeout = backend.timeout_failure();
                proven_child.record_failure(&timeout);
                failures.push(timeout);
            }
        }
    }

    ForceResult {
        status: None,
        termination: last.unwrap_or_else(|| ForceTermination::Failed(backend.timeout_failure())),
        failures,
        proven_child,
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
    mut proven_child: ProvenChild,
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
                let mut proven_status = None;
                for event in events {
                    let kind = backend.event_kind(event);
                    if proven_status.is_some() {
                        interrupt = record_already_exited(interrupt, kind);
                        continue;
                    }
                    let initial_failure = failures.first().cloned();
                    let forced = force_and_confirm(backend, failures, proven_child);
                    interrupt = record_event_force(
                        interrupt,
                        kind,
                        forced.termination.clone(),
                        initial_failure,
                    );
                    proven_status = forced.status;
                    failures = forced.failures;
                    proven_child = forced.proven_child;
                }
                if let Some(status) = proven_status {
                    return proven(backend, proven_child.resolve(status), interrupt, failures);
                }
            }
        }
    }
}

enum ProvenChild {
    Exited,
    Failed(ProcessFailure),
}

impl ProvenChild {
    fn record_failure(&mut self, failure: &ProcessFailure) {
        // A later death proof authorizes cleanup but cannot erase the first liveness failure.
        if matches!(self, Self::Exited) {
            *self = Self::Failed(failure.clone());
        }
    }

    fn resolve(self, status: ChildStatus) -> ChildOutcome {
        match self {
            Self::Exited => ChildOutcome::Exited(status),
            Self::Failed(failure) => ChildOutcome::Failed(failure),
        }
    }
}

pub(super) fn record_already_exited(path: InterruptPath, kind: InterruptKind) -> InterruptPath {
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
            second: Some(kind),
            termination: ForceTermination::ChildAlreadyExited,
        },
        InterruptPath::Forced {
            first,
            delivery,
            second: None,
            termination,
        } => InterruptPath::Forced {
            first,
            delivery,
            second: Some(kind),
            termination,
        },
        forced @ InterruptPath::Forced {
            second: Some(_), ..
        } => forced,
    }
}

fn record_event_force(
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
            second: None,
            termination,
        },
        InterruptPath::Graceful {
            interrupt: first,
            delivery,
        } => InterruptPath::Forced {
            first,
            delivery,
            second: Some(kind),
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
            second: second.or(Some(kind)),
            termination,
        },
    }
}

fn record_failure_force(path: InterruptPath, termination: ForceTermination) -> InterruptPath {
    match path {
        InterruptPath::Graceful {
            interrupt: first,
            delivery,
        } => InterruptPath::Forced {
            first,
            delivery,
            second: None,
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
        InterruptPath::None => InterruptPath::None,
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
        post_proof_delivery: InterruptDelivery,
        delivery_calls: usize,
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

        fn classify_after_proof(&self, _event: Self::Event) -> InterruptDelivery {
            self.post_proof_delivery.clone()
        }

        fn deliver_first(&mut self, _event: Self::Event) -> InterruptDelivery {
            self.delivery_calls += 1;
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
            post_proof_delivery: InterruptDelivery::DeliveredByPlatform,
            delivery_calls: 0,
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
            post_proof_delivery: InterruptDelivery::DeliveredByPlatform,
            delivery_calls: 0,
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
        assert!(matches!(
            interrupt,
            InterruptPath::Forced { second: None, .. }
        ));
        assert!(backend.finalized);
        assert!(backend.disarmed);
    }

    #[test]
    fn spawned_failure_uses_bounded_force_and_never_invents_cleanup_permission() {
        let attach = failure(ProcessStage::ContainmentSetup);
        let force_one = failure(ProcessStage::ForceTermination);
        let force_two = failure(ProcessStage::ForceTermination);
        let mut backend = ScriptedBackend {
            events: VecDeque::new(),
            probes: VecDeque::from([Probe::Running, Probe::Running]),
            force: VecDeque::from([
                ForceTermination::Failed(force_one.clone()),
                ForceTermination::Failed(force_two.clone()),
            ]),
            delivery: InterruptDelivery::DeliveredByPlatform,
            post_proof_delivery: InterruptDelivery::DeliveredByPlatform,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = terminate_after_failure(&mut backend, attach.clone());

        let DriverResult::Uncertain { failures, .. } = result else {
            panic!("an unproven spawned failure must defer cleanup");
        };
        assert_eq!(failures, [attach, force_one, force_two]);
        assert!(backend.finalized);
        assert!(!backend.disarmed);
    }

    #[test]
    fn platform_delivery_is_retained_when_the_child_exits_before_event_drain() {
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Break]]),
            probes: VecDeque::from([Probe::ProvenDead(ChildStatus::Exited(0))]),
            force: VecDeque::new(),
            delivery: InterruptDelivery::DeliveredByPlatform,
            post_proof_delivery: InterruptDelivery::DeliveredByPlatform,
            delivery_calls: 0,
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
        assert_eq!(backend.delivery_calls, 0);
    }

    #[test]
    fn forward_only_event_after_death_proof_is_classified_without_delivery() {
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt]]),
            probes: VecDeque::from([Probe::ProvenDead(ChildStatus::Exited(0))]),
            force: VecDeque::new(),
            delivery: InterruptDelivery::Failed(failure(ProcessStage::ForwardInterrupt)),
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
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
                interrupt: InterruptKind::Interrupt,
                delivery: InterruptDelivery::ChildAlreadyExited,
            }
        );
        assert_eq!(backend.delivery_calls, 0);
    }

    #[test]
    fn failed_delivery_to_a_running_child_forces_and_confirms_death() {
        let delivery = failure(ProcessStage::ForwardInterrupt);
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt]]),
            probes: VecDeque::from([
                Probe::Running,
                Probe::Running,
                Probe::ProvenDead(ChildStatus::Exited(0)),
            ]),
            force: VecDeque::from([ForceTermination::Terminated]),
            delivery: InterruptDelivery::Failed(delivery.clone()),
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            child,
            interrupt,
            failures,
            ..
        } = result
        else {
            panic!("force followed by death proof should complete supervision");
        };
        assert_eq!(child, ChildOutcome::Exited(ChildStatus::Exited(0)));
        assert_eq!(failures.as_slice(), std::slice::from_ref(&delivery));
        assert_eq!(
            interrupt,
            InterruptPath::Forced {
                first: InterruptKind::Interrupt,
                delivery: InterruptDelivery::Failed(delivery),
                second: None,
                termination: ForceTermination::Terminated,
            }
        );
    }

    #[test]
    fn failed_delivery_without_death_proof_defers_cleanup() {
        let delivery = failure(ProcessStage::ForwardInterrupt);
        let force_one = failure(ProcessStage::ForceTermination);
        let force_two = failure(ProcessStage::ForceTermination);
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt]]),
            probes: VecDeque::from([Probe::Running, Probe::Running]),
            force: VecDeque::from([
                ForceTermination::Failed(force_one.clone()),
                ForceTermination::Failed(force_two.clone()),
            ]),
            delivery: InterruptDelivery::Failed(delivery.clone()),
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Uncertain {
            interrupt,
            failures,
        } = result
        else {
            panic!("failed force without death proof must remain uncertain");
        };
        assert_eq!(failures, [delivery.clone(), force_one, force_two]);
        assert!(matches!(
            interrupt,
            InterruptPath::Forced {
                delivery: InterruptDelivery::Failed(failure),
                second: None,
                ..
            } if failure == delivery
        ));
    }

    #[test]
    fn post_interrupt_probe_failure_records_the_internal_force_path() {
        let wait = failure(ProcessStage::Wait);
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt]]),
            probes: VecDeque::from([
                Probe::Running,
                Probe::Uncertain(wait.clone()),
                Probe::ProvenDead(ChildStatus::Exited(9)),
            ]),
            force: VecDeque::from([ForceTermination::Terminated]),
            delivery: InterruptDelivery::Forwarded,
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            child, interrupt, ..
        } = result
        else {
            panic!("death proof should complete supervision");
        };
        assert_eq!(child, ChildOutcome::Failed(wait));
        assert!(matches!(
            interrupt,
            InterruptPath::Forced {
                second: None,
                termination: ForceTermination::Terminated,
                ..
            }
        ));
    }

    #[test]
    fn wait_failure_during_force_remains_primary_after_later_proof() {
        let wait = failure(ProcessStage::Wait);
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt, InterruptKind::Terminate]]),
            probes: VecDeque::from([
                Probe::Running,
                Probe::Running,
                Probe::Uncertain(wait.clone()),
                Probe::ProvenDead(ChildStatus::Exited(9)),
            ]),
            force: VecDeque::from([ForceTermination::Terminated, ForceTermination::Terminated]),
            delivery: InterruptDelivery::Forwarded,
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            child, failures, ..
        } = result
        else {
            panic!("later death proof should complete supervision");
        };
        assert_eq!(child, ChildOutcome::Failed(wait.clone()));
        assert_eq!(failures, [wait]);
    }

    #[test]
    fn force_timeout_remains_primary_after_later_finalization_proof() {
        let timeout = failure(ProcessStage::Wait);
        let mut probes = VecDeque::from([Probe::Running, Probe::Running]);
        probes.extend((0..FORCE_CONFIRM_POLLS * FORCE_ATTEMPTS).map(|_| Probe::Running));
        probes.push_back(Probe::ProvenDead(ChildStatus::Exited(9)));
        let mut backend = ScriptedBackend {
            events: VecDeque::from([vec![InterruptKind::Interrupt, InterruptKind::Terminate]]),
            probes,
            force: VecDeque::from([
                ForceTermination::Terminated,
                ForceTermination::Terminated,
                ForceTermination::Terminated,
            ]),
            delivery: InterruptDelivery::Forwarded,
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::from([vec![InterruptKind::Break], Vec::new()]),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            child, failures, ..
        } = result
        else {
            panic!("a finalization event should trigger a later death proof");
        };
        assert_eq!(child, ChildOutcome::Failed(timeout.clone()));
        assert_eq!(failures, [timeout.clone(), timeout]);
        assert!(backend.finalized);
        assert!(backend.disarmed);
    }

    #[test]
    fn force_failure_remains_recorded_when_the_next_probe_proves_death() {
        let force = failure(ProcessStage::ForceTermination);
        let mut backend = ScriptedBackend {
            events: VecDeque::new(),
            probes: VecDeque::from([Probe::ProvenDead(ChildStatus::Exited(0))]),
            force: VecDeque::from([ForceTermination::Failed(force.clone())]),
            delivery: InterruptDelivery::Forwarded,
            post_proof_delivery: InterruptDelivery::ChildAlreadyExited,
            delivery_calls: 0,
            finalization_events: VecDeque::new(),
            finalized: false,
            disarmed: false,
        };

        let result = force_and_confirm(&mut backend, Vec::new(), ProvenChild::Exited);

        assert_eq!(result.status, Some(ChildStatus::Exited(0)));
        assert_eq!(result.termination, ForceTermination::Failed(force.clone()));
        assert_eq!(result.failures, [force]);
    }

    #[test]
    fn event_during_uncertain_finalization_is_forced_and_recorded() {
        let wait = failure(ProcessStage::Wait);
        let force_one = failure(ProcessStage::ForceTermination);
        let force_two = failure(ProcessStage::ForceTermination);
        let mut probes = VecDeque::from([Probe::Uncertain(wait.clone())]);
        probes.extend((0..FAILURE_RECHECK_POLLS * 2).map(|_| Probe::Running));
        probes.push_back(Probe::ProvenDead(ChildStatus::Exited(9)));
        let mut backend = ScriptedBackend {
            events: VecDeque::new(),
            probes,
            force: VecDeque::from([
                ForceTermination::Failed(force_one.clone()),
                ForceTermination::Failed(force_two.clone()),
                ForceTermination::Terminated,
            ]),
            delivery: InterruptDelivery::DeliveredByPlatform,
            post_proof_delivery: InterruptDelivery::DeliveredByPlatform,
            delivery_calls: 0,
            finalization_events: VecDeque::from([
                vec![InterruptKind::Interrupt, InterruptKind::Terminate],
                Vec::new(),
            ]),
            finalized: false,
            disarmed: false,
        };

        let result = supervise(&mut backend);

        let DriverResult::Proven {
            child,
            failures,
            interrupt,
            ..
        } = result
        else {
            panic!("the finalization event should trigger force and a death proof");
        };
        assert_eq!(child, ChildOutcome::Failed(wait.clone()));
        assert_eq!(failures, [wait.clone(), force_one, force_two]);
        assert_eq!(
            interrupt,
            InterruptPath::Forced {
                first: InterruptKind::Interrupt,
                delivery: InterruptDelivery::Failed(wait),
                second: Some(InterruptKind::Terminate),
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

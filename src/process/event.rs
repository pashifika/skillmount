use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use super::InterruptKind;

const PHASE_MASK: u32 = 0b11;
const PHASE_INACTIVE: u32 = 0;
const PHASE_ARMED: u32 = 1;
const PHASE_ACTIVE: u32 = 2;
const PHASE_FINALIZING: u32 = 3;
const COUNT_SHIFT: u32 = 2;
const COUNT_MASK: u32 = 0b11 << COUNT_SHIFT;
const FIRST_SHIFT: u32 = 4;
const FIRST_MASK: u32 = 0b11 << FIRST_SHIFT;
const SECOND_SHIFT: u32 = 6;
const SECOND_MASK: u32 = 0b11 << SECOND_SHIFT;
const GENERATION_SHIFT: u32 = 8;
const GENERATION_MASK: u32 = u32::MAX << GENERATION_SHIFT;
const MAX_GENERATION: u32 = GENERATION_MASK >> GENERATION_SHIFT;

pub(super) struct EventLedger {
    state: AtomicU32,
}

impl EventLedger {
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicU32::new(PHASE_INACTIVE),
        }
    }

    pub(super) fn acquire(&'static self) -> io::Result<EventSession> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & PHASE_MASK != PHASE_INACTIVE {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another child-process supervision session is already active",
                ));
            }
            if generation(observed) == MAX_GENERATION {
                return Err(io::Error::other(
                    "child-process event-session generation space is exhausted",
                ));
            }

            let armed = (observed & GENERATION_MASK) | PHASE_ARMED;
            match self.state.compare_exchange_weak(
                observed,
                armed,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
        Ok(EventSession {
            ledger: self,
            leased: true,
        })
    }

    pub(super) fn record(&self, kind: InterruptKind) -> bool {
        let observed = self.state.load(Ordering::SeqCst);
        self.record_from(observed, generation(observed), kind)
    }

    fn record_from(&self, mut observed: u32, lease_generation: u32, kind: InterruptKind) -> bool {
        loop {
            if generation(observed) != lease_generation || observed & PHASE_MASK != PHASE_ACTIVE {
                return false;
            }

            let count = (observed & COUNT_MASK) >> COUNT_SHIFT;
            let next_count = count.saturating_add(1).min(3);
            let mut next = (observed & !COUNT_MASK) | (next_count << COUNT_SHIFT);
            if count == 0 {
                next = (next & !FIRST_MASK) | (encode(kind) << FIRST_SHIFT);
            } else if count == 1 {
                next = (next & !SECOND_MASK) | (encode(kind) << SECOND_SHIFT);
            }

            match self.state.compare_exchange_weak(
                observed,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => {
                    if generation(actual) != lease_generation {
                        return false;
                    }
                    observed = actual;
                }
            }
        }
    }

    fn activate(&self) -> io::Result<()> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            match observed & PHASE_MASK {
                PHASE_ACTIVE => return Ok(()),
                PHASE_ARMED => {
                    let active = (observed & !PHASE_MASK) | PHASE_ACTIVE;
                    match self.state.compare_exchange_weak(
                        observed,
                        active,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => return Ok(()),
                        Err(actual) => observed = actual,
                    }
                }
                _ => {
                    return Err(io::Error::other(
                        "child-process event session cannot be activated in its current phase",
                    ));
                }
            }
        }
    }

    fn take(&self) -> Vec<InterruptKind> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & PHASE_MASK != PHASE_ACTIVE {
                return Vec::new();
            }
            let cleared = observed & (GENERATION_MASK | PHASE_MASK);
            match self.state.compare_exchange_weak(
                observed,
                cleared,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return decode_events(observed),
                Err(actual) => observed = actual,
            }
        }
    }

    fn begin_finalization(&self) -> Result<(), Vec<InterruptKind>> {
        loop {
            let observed = self.state.load(Ordering::SeqCst);
            match observed & PHASE_MASK {
                PHASE_ARMED => {
                    let finalizing = (observed & !PHASE_MASK) | PHASE_FINALIZING;
                    if self
                        .state
                        .compare_exchange(observed, finalizing, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                PHASE_ACTIVE => {
                    if observed & COUNT_MASK != 0 {
                        let events = self.take();
                        if !events.is_empty() {
                            return Err(events);
                        }
                        continue;
                    }
                    let finalizing = (observed & !PHASE_MASK) | PHASE_FINALIZING;
                    if self
                        .state
                        .compare_exchange(observed, finalizing, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn release(&self) {
        let observed = self.state.load(Ordering::SeqCst);
        let next_generation = generation(observed).saturating_add(1);
        self.state.store(
            next_generation << GENERATION_SHIFT | PHASE_INACTIVE,
            Ordering::SeqCst,
        );
    }
}

pub(super) struct EventSession {
    ledger: &'static EventLedger,
    leased: bool,
}

impl EventSession {
    pub(super) fn activate(&mut self) -> io::Result<()> {
        self.ledger.activate()
    }

    pub(super) fn pending(&self) -> Vec<InterruptKind> {
        self.ledger.take()
    }

    pub(super) fn begin_finalization(&mut self) -> Result<(), Vec<InterruptKind>> {
        self.ledger.begin_finalization()
    }
}

impl Drop for EventSession {
    fn drop(&mut self) {
        if self.leased {
            self.ledger.release();
            self.leased = false;
        }
    }
}

const fn generation(state: u32) -> u32 {
    (state & GENERATION_MASK) >> GENERATION_SHIFT
}

const fn encode(kind: InterruptKind) -> u32 {
    match kind {
        InterruptKind::Interrupt => 1,
        InterruptKind::Terminate => 2,
        InterruptKind::Break => 3,
    }
}

fn decode(encoded: u32) -> InterruptKind {
    match encoded {
        1 => InterruptKind::Interrupt,
        2 => InterruptKind::Terminate,
        3 => InterruptKind::Break,
        _ => unreachable!("event ledger contains an invalid interrupt kind"),
    }
}

fn decode_events(state: u32) -> Vec<InterruptKind> {
    let count = (state & COUNT_MASK) >> COUNT_SHIFT;
    let mut events = Vec::with_capacity(usize::try_from(count).unwrap_or(3));
    if count >= 1 {
        events.push(decode((state & FIRST_MASK) >> FIRST_SHIFT));
    }
    if count >= 2 {
        events.push(decode((state & SECOND_MASK) >> SECOND_SHIFT));
    }
    if count >= 3 {
        events.push(decode((state & SECOND_MASK) >> SECOND_SHIFT));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_preserves_the_first_two_handler_occurrences_and_saturates_retries() {
        static LEDGER: EventLedger = EventLedger::new();
        let mut session = LEDGER.acquire().expect("acquire event session");
        session.activate().expect("activate event session");

        assert!(LEDGER.record(InterruptKind::Interrupt));
        assert!(LEDGER.record(InterruptKind::Terminate));
        assert!(LEDGER.record(InterruptKind::Break));
        assert!(LEDGER.record(InterruptKind::Break));

        assert_eq!(
            session.pending(),
            [
                InterruptKind::Interrupt,
                InterruptKind::Terminate,
                InterruptKind::Terminate,
            ]
        );
    }

    #[test]
    fn finalization_drains_prior_events_then_returns_later_events_to_the_platform() {
        static LEDGER: EventLedger = EventLedger::new();
        let mut session = LEDGER.acquire().expect("acquire event session");
        session.activate().expect("activate event session");
        assert!(LEDGER.record(InterruptKind::Interrupt));

        assert_eq!(
            session.begin_finalization(),
            Err(vec![InterruptKind::Interrupt])
        );
        assert_eq!(session.begin_finalization(), Ok(()));
        assert!(!LEDGER.record(InterruptKind::Terminate));

        drop(session);
        let mut next = LEDGER.acquire().expect("reuse process dispatcher");
        next.activate().expect("activate reused dispatcher");
    }

    #[test]
    fn armed_session_returns_events_to_platform_until_the_child_is_exposed() {
        static LEDGER: EventLedger = EventLedger::new();
        let mut session = LEDGER.acquire().expect("acquire armed event session");

        assert!(!LEDGER.record(InterruptKind::Interrupt));
        session.activate().expect("expose child to event delivery");
        assert!(LEDGER.record(InterruptKind::Interrupt));
        assert_eq!(session.pending(), [InterruptKind::Interrupt]);
    }

    #[test]
    fn stale_handler_cannot_record_into_a_later_session() {
        static LEDGER: EventLedger = EventLedger::new();
        let mut first = LEDGER.acquire().expect("acquire first session");
        first.activate().expect("activate first session");
        let stale = LEDGER.state.load(Ordering::SeqCst);
        let stale_generation = generation(stale);
        assert_eq!(first.begin_finalization(), Ok(()));
        drop(first);

        let mut second = LEDGER.acquire().expect("acquire second session");
        second.activate().expect("activate second session");

        assert!(!LEDGER.record_from(stale, stale_generation, InterruptKind::Terminate));
        assert!(second.pending().is_empty());
    }
}

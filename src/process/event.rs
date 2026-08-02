use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use super::InterruptKind;

const PHASE_MASK: u32 = 0b11;
const PHASE_INACTIVE: u32 = 0;
const PHASE_ACTIVE: u32 = 1;
const PHASE_FINALIZING: u32 = 2;
const COUNT_SHIFT: u32 = 2;
const COUNT_MASK: u32 = 0b11 << COUNT_SHIFT;
const FIRST_SHIFT: u32 = 4;
const FIRST_MASK: u32 = 0b11 << FIRST_SHIFT;
const SECOND_SHIFT: u32 = 6;
const SECOND_MASK: u32 = 0b11 << SECOND_SHIFT;

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
        self.state
            .compare_exchange(
                PHASE_INACTIVE,
                PHASE_ACTIVE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another child-process supervision session is already active",
                )
            })?;
        Ok(EventSession {
            ledger: self,
            active: true,
        })
    }

    pub(super) fn record(&self, kind: InterruptKind) -> bool {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & PHASE_MASK != PHASE_ACTIVE {
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
                Err(actual) => observed = actual,
            }
        }
    }

    fn take(&self) -> Vec<InterruptKind> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & PHASE_MASK != PHASE_ACTIVE {
                return Vec::new();
            }
            let cleared = observed & PHASE_MASK;
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
            if observed & PHASE_MASK != PHASE_ACTIVE {
                return Ok(());
            }
            if observed & COUNT_MASK != 0 {
                let events = self.take();
                if !events.is_empty() {
                    return Err(events);
                }
                continue;
            }
            if self
                .state
                .compare_exchange(
                    observed,
                    PHASE_FINALIZING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn release(&self) {
        self.state.store(PHASE_INACTIVE, Ordering::SeqCst);
    }
}

pub(super) struct EventSession {
    ledger: &'static EventLedger,
    active: bool,
}

impl EventSession {
    pub(super) fn pending(&self) -> Vec<InterruptKind> {
        self.ledger.take()
    }

    pub(super) fn begin_finalization(&mut self) -> Result<(), Vec<InterruptKind>> {
        self.ledger.begin_finalization()
    }
}

impl Drop for EventSession {
    fn drop(&mut self) {
        if self.active {
            self.ledger.release();
            self.active = false;
        }
    }
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
        let session = LEDGER.acquire().expect("acquire event session");

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
        assert!(LEDGER.record(InterruptKind::Interrupt));

        assert_eq!(
            session.begin_finalization(),
            Err(vec![InterruptKind::Interrupt])
        );
        assert_eq!(session.begin_finalization(), Ok(()));
        assert!(!LEDGER.record(InterruptKind::Terminate));

        drop(session);
        let _next = LEDGER.acquire().expect("reuse process dispatcher");
    }
}

use sol_lib::NamedInternalEvent;
use sol_lib::internal_event::{ComponentEventsDropped, INTENTIONAL, InternalEvent};

#[derive(Debug, NamedInternalEvent)]
pub struct DedupeEventsDropped {
    pub count: usize,
}

impl InternalEvent for DedupeEventsDropped {
    fn emit(self) {
        emit!(ComponentEventsDropped::<INTENTIONAL> {
            count: self.count,
            reason: "Events have been found in cache for deduplication.",
        });
    }
}

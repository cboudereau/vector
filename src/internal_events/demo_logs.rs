use sol_lib::NamedInternalEvent;
use sol_lib::internal_event::InternalEvent;

#[derive(Debug, NamedInternalEvent)]
pub struct DemoLogsEventProcessed;

impl InternalEvent for DemoLogsEventProcessed {
    fn emit(self) {
        trace!(message = "Received one event.");
    }
}

use metrics::counter;
use vector_lib::NamedInternalEvent;
use vector_lib::internal_event::{
    ComponentEventsDropped, InternalEvent, UNINTENTIONAL, error_stage, error_type,
};

#[derive(Debug, NamedInternalEvent)]
pub struct StatsdInvalidMetricError {
    pub error: crate::Error,
}

impl InternalEvent for StatsdInvalidMetricError {
    fn emit(self) {
        let reason = "Invalid metric type received.";
        error!(
            message = reason,
            error_code = "invalid_metric",
            error_type = error_type::ENCODER_FAILED,
            stage = error_stage::PROCESSING,
            error = ?self.error,
        );
        counter!(
            "component_errors_total",
            "error_code" => "invalid_metric",
            "error_type" => error_type::ENCODER_FAILED,
            "stage" => error_stage::PROCESSING,
        )
        .increment(1);

        emit!(ComponentEventsDropped::<UNINTENTIONAL> { reason, count: 1 });
    }
}

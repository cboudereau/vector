// Canonical field names for OTel log records.
pub const BODY: &str = "body";
pub const TIME_UNIX_NANO: &str = "time_unix_nano";
pub const OBSERVED_TIME_UNIX_NANO: &str = "observed_time_unix_nano";
pub const SEVERITY_TEXT: &str = "severity_text";
pub const SEVERITY_NUMBER: &str = "severity_number";
pub const LOG_TRACE_ID: &str = "trace_id";
pub const LOG_SPAN_ID: &str = "span_id";
pub const LOG_FLAGS: &str = "flags";

// Canonical field names for OTel spans.
pub const START_TIME_UNIX_NANO: &str = "start_time_unix_nano";
pub const END_TIME_UNIX_NANO: &str = "end_time_unix_nano";
pub const PARENT_SPAN_ID: &str = "parent_span_id";
pub const SPAN_TRACE_ID: &str = "trace_id";
pub const SPAN_SPAN_ID: &str = "span_id";
pub const SPAN_STATUS: &str = "status";
pub const SPAN_KIND: &str = "kind";
pub const SPAN_EVENTS: &str = "events";
pub const SPAN_LINKS: &str = "links";
pub const SPAN_FLAGS: &str = "flags";

// Common OTel structure fields.
pub const ATTRIBUTES: &str = "attributes";
pub const DROPPED_ATTRIBUTES_COUNT: &str = "dropped_attributes_count";
pub const RESOURCE: &str = "resource";
pub const SCOPE: &str = "scope";
pub const NAME: &str = "name";
pub const VERSION: &str = "version";

// Span status sub-fields.
pub const STATUS_CODE: &str = "code";
pub const STATUS_MESSAGE: &str = "message";

// Metric-specific field names.
pub const DESCRIPTION: &str = "description";
pub const UNIT: &str = "unit";
pub const METRIC_NAMESPACE: &str = "metric.namespace";

use async_graphql::Object;
use chrono::{DateTime, Utc};

use crate::{
    config::ComponentKey,
    event::{MetricValue, OtelMetric},
};

pub struct ErrorsTotal(OtelMetric);

impl ErrorsTotal {
    pub const fn new(m: OtelMetric) -> Self {
        Self(m)
    }
}

#[Object]
impl ErrorsTotal {
    /// Metric timestamp
    pub async fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.0.timestamp()
    }

    /// Total error count
    pub async fn errors_total(&self) -> f64 {
        match self.0.value() {
            MetricValue::Counter { value } => value,
            _ => 0.00,
        }
    }
}

impl From<OtelMetric> for ErrorsTotal {
    fn from(m: OtelMetric) -> Self {
        Self(m)
    }
}

pub struct ComponentErrorsTotal {
    component_key: ComponentKey,
    metric: OtelMetric,
}

impl ComponentErrorsTotal {
    /// Returns a new `ComponentErrorsTotal` struct, which is a GraphQL type. The
    /// component id is hoisted for clear field resolution in the resulting payload
    pub fn new(metric: OtelMetric) -> Self {
        let component_key = metric.tag_value("component_id").expect(
            "Returned a metric without a `component_id`, which shouldn't happen. Please report.",
        );
        let component_key = ComponentKey::from(component_key);

        Self {
            component_key,
            metric,
        }
    }
}

#[Object]
impl ComponentErrorsTotal {
    /// Component id
    async fn component_id(&self) -> &str {
        self.component_key.id()
    }

    /// Errors processed metric
    async fn metric(&self) -> ErrorsTotal {
        ErrorsTotal::new(self.metric.clone())
    }
}

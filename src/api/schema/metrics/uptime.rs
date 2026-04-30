use async_graphql::Object;
use chrono::{DateTime, Utc};

use crate::event::{MetricView, OtelMetric};

pub struct Uptime(OtelMetric);

impl Uptime {
    pub const fn new(m: OtelMetric) -> Self {
        Self(m)
    }
}

#[Object]
impl Uptime {
    /// Metric timestamp
    pub async fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.0.timestamp()
    }

    /// Number of seconds the Vector instance has been alive
    pub async fn seconds(&self) -> f64 {
        match self.0.view() {
            MetricView::Gauge { value } => value,
            _ => 0.00,
        }
    }
}

impl From<OtelMetric> for Uptime {
    fn from(m: OtelMetric) -> Self {
        Self(m)
    }
}

use vector_core::event::{Metric, MetricKind, MetricTags, OtelMetric};
use vector_vrl_metrics::MetricsStorage;

pub(crate) fn test_vrl_metrics_storage() -> MetricsStorage {
    let storage = MetricsStorage::default();
    let m = Metric::new(
        "utilization",
        MetricKind::Absolute,
        vector_core::event::MetricValue::Gauge { value: 0.5 },
    )
    .with_tags(Some(MetricTags::from_iter([(
        "component_id".to_string(),
        "test".to_string(),
    )])));
    let (s, d, md) = m.into_parts();
    storage.cache.store(
        vec![OtelMetric::from_metric_parts(s, d, md)]
            .into(),
    );
    storage
}

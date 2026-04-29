use chrono::{TimeZone, Utc};
use vector_core::event::{
    Event, EventMetadata, MetricKind, MetricTags,
    OtelMetric,
    metric::{Bucket, Quantile, TagValue},
};

use super::proto::{
    common::v1::{InstrumentationScope, KeyValue},
    metrics::v1::{
        AggregationTemporality, ExponentialHistogram, ExponentialHistogramDataPoint, Gauge,
        Histogram, HistogramDataPoint, NumberDataPoint, ResourceMetrics, Sum, Summary,
        SummaryDataPoint,
        metric::Data,
        number_data_point::Value as NumberDataPointValue,
    },
    resource::v1::Resource,
};

impl ResourceMetrics {
    pub fn into_event_iter(self) -> impl Iterator<Item = Event> {
        let resource = self.resource.clone();

        self.scope_metrics
            .into_iter()
            .flat_map(move |scope_metrics| {
                let scope = scope_metrics.scope;
                let resource = resource.clone();

                scope_metrics.metrics.into_iter().flat_map(move |metric| {
                    let metric_name = metric.name.clone();
                    match metric.data {
                        Some(Data::Gauge(g)) => {
                            Self::convert_gauge(g, &resource, &scope, &metric_name)
                        }
                        Some(Data::Sum(s)) => Self::convert_sum(s, &resource, &scope, &metric_name),
                        Some(Data::Histogram(h)) => {
                            Self::convert_histogram(h, &resource, &scope, &metric_name)
                        }
                        Some(Data::ExponentialHistogram(e)) => {
                            Self::convert_exp_histogram(e, &resource, &scope, &metric_name)
                        }
                        Some(Data::Summary(su)) => {
                            Self::convert_summary(su, &resource, &scope, &metric_name)
                        }
                        _ => Vec::new(),
                    }
                })
            })
    }

    /// Convert into an iterator of `Event::OtelMetric`, preserving the proto
    /// structs with zero field-level conversion. One event per OTel `Metric`.
    pub fn into_otel_event_iter(self) -> impl Iterator<Item = Event> {
        let resource = proto_convert_resource(self.resource);

        self.scope_metrics
            .into_iter()
            .flat_map(move |scope_metrics| {
                let scope = proto_convert_scope(scope_metrics.scope);
                let resource = resource.clone();

                scope_metrics.metrics.into_iter().map(move |metric| {
                    let otel_metric = proto_convert_metric(metric);
                    Event::Metric(OtelMetric::from_parts(
                        otel_metric,
                        resource.clone(),
                        scope.clone(),
                        EventMetadata::default(),
                    ))
                })
            })
    }

    fn convert_gauge(
        gauge: Gauge,
        resource: &Option<Resource>,
        scope: &Option<InstrumentationScope>,
        metric_name: &str,
    ) -> Vec<Event> {
        let resource = resource.clone();
        let scope = scope.clone();
        let metric_name = metric_name.to_string();

        gauge
            .data_points
            .into_iter()
            .map(move |point| {
                GaugeMetric {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    point,
                }
                .into_metric(metric_name.clone())
            })
            .collect()
    }

    fn convert_sum(
        sum: Sum,
        resource: &Option<Resource>,
        scope: &Option<InstrumentationScope>,
        metric_name: &str,
    ) -> Vec<Event> {
        let resource = resource.clone();
        let scope = scope.clone();
        let metric_name = metric_name.to_string();

        sum.data_points
            .into_iter()
            .map(move |point| {
                SumMetric {
                    aggregation_temporality: sum.aggregation_temporality,
                    resource: resource.clone(),
                    scope: scope.clone(),
                    is_monotonic: sum.is_monotonic,
                    point,
                }
                .into_metric(metric_name.clone())
            })
            .collect()
    }

    fn convert_histogram(
        histogram: Histogram,
        resource: &Option<Resource>,
        scope: &Option<InstrumentationScope>,
        metric_name: &str,
    ) -> Vec<Event> {
        let resource = resource.clone();
        let scope = scope.clone();
        let metric_name = metric_name.to_string();

        histogram
            .data_points
            .into_iter()
            .map(move |point| {
                HistogramMetric {
                    aggregation_temporality: histogram.aggregation_temporality,
                    resource: resource.clone(),
                    scope: scope.clone(),
                    point,
                }
                .into_metric(metric_name.clone())
            })
            .collect()
    }

    fn convert_exp_histogram(
        histogram: ExponentialHistogram,
        resource: &Option<Resource>,
        scope: &Option<InstrumentationScope>,
        metric_name: &str,
    ) -> Vec<Event> {
        let resource = resource.clone();
        let scope = scope.clone();
        let metric_name = metric_name.to_string();

        histogram
            .data_points
            .into_iter()
            .map(move |point| {
                ExpHistogramMetric {
                    aggregation_temporality: histogram.aggregation_temporality,
                    resource: resource.clone(),
                    scope: scope.clone(),
                    point,
                }
                .into_metric(metric_name.clone())
            })
            .collect()
    }

    fn convert_summary(
        summary: Summary,
        resource: &Option<Resource>,
        scope: &Option<InstrumentationScope>,
        metric_name: &str,
    ) -> Vec<Event> {
        let resource = resource.clone();
        let scope = scope.clone();
        let metric_name = metric_name.to_string();

        summary
            .data_points
            .into_iter()
            .map(move |point| {
                SummaryMetric {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    point,
                }
                .into_metric(metric_name.clone())
            })
            .collect()
    }
}

struct GaugeMetric {
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    point: NumberDataPoint,
}

struct SumMetric {
    aggregation_temporality: i32,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    point: NumberDataPoint,
    is_monotonic: bool,
}

struct SummaryMetric {
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    point: SummaryDataPoint,
}

struct HistogramMetric {
    aggregation_temporality: i32,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    point: HistogramDataPoint,
}

struct ExpHistogramMetric {
    aggregation_temporality: i32,
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    point: ExponentialHistogramDataPoint,
}

pub fn build_metric_tags(
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    attributes: &[KeyValue],
) -> MetricTags {
    let mut tags = MetricTags::default();

    if let Some(res) = resource {
        for attr in res.attributes {
            if let Some(value) = &attr.value
                && let Some(pb_value) = &value.value
            {
                tags.insert(
                    format!("resource.{}", attr.key.clone()),
                    TagValue::from(pb_value.clone()),
                );
            }
        }
    }

    if let Some(scope) = scope {
        if !scope.name.is_empty() {
            tags.insert("scope.name".to_string(), scope.name);
        }
        if !scope.version.is_empty() {
            tags.insert("scope.version".to_string(), scope.version);
        }
        for attr in scope.attributes {
            if let Some(value) = &attr.value
                && let Some(pb_value) = &value.value
            {
                tags.insert(
                    format!("scope.{}", attr.key.clone()),
                    TagValue::from(pb_value.clone()),
                );
            }
        }
    }

    for attr in attributes {
        if let Some(value) = &attr.value
            && let Some(pb_value) = &value.value
        {
            tags.insert(attr.key.clone(), TagValue::from(pb_value.clone()));
        }
    }

    tags
}

impl SumMetric {
    fn into_metric(self, metric_name: String) -> Event {
        let timestamp = Some(Utc.timestamp_nanos(self.point.time_unix_nano as i64));
        let value = self.point.value.to_f64().unwrap_or(0.0);
        let attributes = build_metric_tags(self.resource, self.scope, &self.point.attributes);
        let kind = if self.aggregation_temporality == AggregationTemporality::Delta as i32 {
            MetricKind::Incremental
        } else {
            MetricKind::Absolute
        };

        let otel = if self.is_monotonic {
            OtelMetric::new_counter(metric_name, kind, value)
        } else {
            OtelMetric::new_gauge(metric_name, value)
        };
        Event::Metric(otel.with_tags(Some(attributes)).with_timestamp(timestamp))
    }
}

impl GaugeMetric {
    fn into_metric(self, metric_name: String) -> Event {
        let timestamp = Some(Utc.timestamp_nanos(self.point.time_unix_nano as i64));
        let value = self.point.value.to_f64().unwrap_or(0.0);
        let attributes = build_metric_tags(self.resource, self.scope, &self.point.attributes);

        Event::Metric(
            OtelMetric::new_gauge(metric_name, value)
                .with_tags(Some(attributes))
                .with_timestamp(timestamp),
        )
    }
}

impl HistogramMetric {
    fn into_metric(self, metric_name: String) -> Event {
        let timestamp = Some(Utc.timestamp_nanos(self.point.time_unix_nano as i64));
        let attributes = build_metric_tags(self.resource, self.scope, &self.point.attributes);
        let buckets = match self.point.bucket_counts.len() {
            0 => Vec::new(),
            n => {
                let mut buckets = Vec::with_capacity(n);

                for (i, &count) in self.point.bucket_counts.iter().enumerate() {
                    // there are n+1 buckets, since we have -Inf, +Inf on the sides
                    let upper_limit = self
                        .point
                        .explicit_bounds
                        .get(i)
                        .copied()
                        .unwrap_or(f64::INFINITY);
                    buckets.push(Bucket { count, upper_limit });
                }

                buckets
            }
        };

        let kind = if self.aggregation_temporality == AggregationTemporality::Delta as i32 {
            MetricKind::Incremental
        } else {
            MetricKind::Absolute
        };

        Event::Metric(
            OtelMetric::new_histogram(metric_name, kind, &buckets, self.point.count, self.point.sum.unwrap_or(0.0))
                .with_tags(Some(attributes))
                .with_timestamp(timestamp),
        )
    }
}

impl ExpHistogramMetric {
    fn into_metric(self, metric_name: String) -> Event {
        let timestamp = Some(Utc.timestamp_nanos(self.point.time_unix_nano as i64));
        let attributes = build_metric_tags(self.resource, self.scope, &self.point.attributes);

        let scale = self.point.scale;
        let base = 2f64.powf(2f64.powi(-scale));

        let mut buckets = Vec::new();

        if let Some(negative_buckets) = self.point.negative {
            for (i, &count) in negative_buckets.bucket_counts.iter().enumerate() {
                let index = negative_buckets.offset + i as i32;
                let upper_limit = -base.powi(index);
                buckets.push(Bucket { count, upper_limit });
            }
        }

        if self.point.zero_count > 0 {
            buckets.push(Bucket {
                count: self.point.zero_count,
                upper_limit: 0.0,
            });
        }

        if let Some(positive_buckets) = self.point.positive {
            for (i, &count) in positive_buckets.bucket_counts.iter().enumerate() {
                let index = positive_buckets.offset + i as i32;
                let upper_limit = base.powi(index + 1);
                buckets.push(Bucket { count, upper_limit });
            }
        }

        let kind = if self.aggregation_temporality == AggregationTemporality::Delta as i32 {
            MetricKind::Incremental
        } else {
            MetricKind::Absolute
        };

        Event::Metric(
            OtelMetric::new_histogram(metric_name, kind, &buckets, self.point.count, self.point.sum.unwrap_or(0.0))
                .with_tags(Some(attributes))
                .with_timestamp(timestamp),
        )
    }
}

impl SummaryMetric {
    fn into_metric(self, metric_name: String) -> Event {
        let timestamp = Some(Utc.timestamp_nanos(self.point.time_unix_nano as i64));
        let attributes = build_metric_tags(self.resource, self.scope, &self.point.attributes);

        let quantiles: Vec<Quantile> = self
            .point
            .quantile_values
            .iter()
            .map(|q| Quantile {
                quantile: q.quantile,
                value: q.value,
            })
            .collect();

        Event::Metric(
            OtelMetric::new_summary(metric_name, &quantiles, self.point.count, self.point.sum)
                .with_tags(Some(attributes))
                .with_timestamp(timestamp),
        )
    }
}

pub trait ToF64 {
    fn to_f64(self) -> Option<f64>;
}

impl ToF64 for Option<NumberDataPointValue> {
    fn to_f64(self) -> Option<f64> {
        match self {
            Some(NumberDataPointValue::AsDouble(f)) => Some(f),
            Some(NumberDataPointValue::AsInt(i)) => Some(i as f64),
            None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Vector Metric → OTel ExportMetricsServiceRequest
// ---------------------------------------------------------------------------

// --- Proto type conversion helpers (opentelemetry-proto ↔ otel-proto-types) ---

fn proto_convert_resource(
    r: Option<Resource>,
) -> Option<upstream_opentelemetry_proto::tonic::resource::v1::Resource> {
    use prost::Message;
    let r = r?;
    let bytes = r.encode_to_vec();
    upstream_opentelemetry_proto::tonic::resource::v1::Resource::decode(bytes::Bytes::from(bytes)).ok()
}

fn proto_convert_scope(
    s: Option<InstrumentationScope>,
) -> Option<upstream_opentelemetry_proto::tonic::common::v1::InstrumentationScope> {
    use prost::Message;
    let s = s?;
    let bytes = s.encode_to_vec();
    upstream_opentelemetry_proto::tonic::common::v1::InstrumentationScope::decode(bytes::Bytes::from(bytes)).ok()
}

fn proto_convert_metric(
    m: super::proto::metrics::v1::Metric,
) -> upstream_opentelemetry_proto::tonic::metrics::v1::Metric {
    use prost::Message;
    let bytes = m.encode_to_vec();
    upstream_opentelemetry_proto::tonic::metrics::v1::Metric::decode(bytes::Bytes::from(bytes))
        .expect("Metric proto decode failed on same-schema message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{
        common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value},
        metrics::v1::{
            AggregationTemporality, Gauge, Histogram, HistogramDataPoint, NumberDataPoint,
            ResourceMetrics, ScopeMetrics, Sum, metric,
            number_data_point::Value as NDPValue,
        },
        resource::v1::Resource,
    };

    fn make_resource_metrics() -> ResourceMetrics {
        ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_string(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("metric-svc".to_string())),
                    }),
                }],
                dropped_attributes_count: 0,
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(InstrumentationScope {
                    name: "metric-lib".to_string(),
                    version: "2.0.0".to_string(),
                    attributes: vec![],
                    dropped_attributes_count: 0,
                }),
                metrics: vec![
                    super::super::proto::metrics::v1::Metric {
                        name: "request.count".to_string(),
                        description: "Total requests".to_string(),
                        unit: "1".to_string(),
                        data: Some(metric::Data::Sum(Sum {
                            data_points: vec![NumberDataPoint {
                                attributes: vec![KeyValue {
                                    key: "method".to_string(),
                                    value: Some(AnyValue {
                                        value: Some(any_value::Value::StringValue(
                                            "GET".to_string(),
                                        )),
                                    }),
                                }],
                                start_time_unix_nano: 1_000_000_000,
                                time_unix_nano: 2_000_000_000,
                                value: Some(NDPValue::AsInt(42)),
                                exemplars: vec![],
                                flags: 0,
                            }],
                            aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            is_monotonic: true,
                        })),
                    },
                    super::super::proto::metrics::v1::Metric {
                        name: "cpu.usage".to_string(),
                        description: String::new(),
                        unit: "%".to_string(),
                        data: Some(metric::Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint {
                                attributes: vec![],
                                start_time_unix_nano: 0,
                                time_unix_nano: 3_000_000_000,
                                value: Some(NDPValue::AsDouble(75.5)),
                                exemplars: vec![],
                                flags: 0,
                            }],
                        })),
                    },
                ],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    #[test]
    fn otel_metric_event_iter_preserves_metric_names() {
        let rm = make_resource_metrics();
        let events: Vec<_> = rm.into_otel_event_iter().collect();
        assert_eq!(events.len(), 2, "one event per OTel Metric");

        let m0 = events[0].as_otel_metric();
        assert_eq!(m0.metric().name, "request.count");
        assert_eq!(m0.metric().description, "Total requests");
        assert_eq!(m0.metric().unit, "1");

        let m1 = events[1].as_otel_metric();
        assert_eq!(m1.metric().name, "cpu.usage");
        assert_eq!(m1.metric().unit, "%");
    }

    #[test]
    fn otel_metric_event_iter_preserves_resource() {
        let rm = make_resource_metrics();
        let events: Vec<_> = rm.into_otel_event_iter().collect();

        let m = events[0].as_otel_metric();
        let resource = m.resource_proto().expect("resource must be present");
        assert_eq!(resource.attributes.len(), 1);
        assert_eq!(resource.attributes[0].key, "service.name");
    }

    #[test]
    fn otel_metric_event_iter_preserves_scope() {
        let rm = make_resource_metrics();
        let events: Vec<_> = rm.into_otel_event_iter().collect();

        let m = events[0].as_otel_metric();
        let scope = m.scope().expect("scope must be present");
        assert_eq!(scope.name, "metric-lib");
        assert_eq!(scope.version, "2.0.0");
    }

    #[test]
    fn otel_metric_event_iter_preserves_data_points() {
        let rm = make_resource_metrics();
        let events: Vec<_> = rm.into_otel_event_iter().collect();

        let m0 = events[0].as_otel_metric();
        match &m0.metric().data {
            Some(upstream_opentelemetry_proto::tonic::metrics::v1::metric::Data::Sum(sum)) => {
                assert_eq!(sum.data_points.len(), 1);
                assert!(sum.is_monotonic);
                assert_eq!(sum.data_points[0].time_unix_nano, 2_000_000_000);
            }
            other => panic!("expected Sum, got {:?}", other),
        }

        let m1 = events[1].as_otel_metric();
        match &m1.metric().data {
            Some(upstream_opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(gauge)) => {
                assert_eq!(gauge.data_points.len(), 1);
                assert_eq!(gauge.data_points[0].time_unix_nano, 3_000_000_000);
            }
            other => panic!("expected Gauge, got {:?}", other),
        }
    }

    #[test]
    fn otel_metric_event_iter_no_resource_no_scope() {
        let rm = ResourceMetrics {
            resource: None,
            scope_metrics: vec![ScopeMetrics {
                scope: None,
                metrics: vec![super::super::proto::metrics::v1::Metric {
                    name: "bare.metric".to_string(),
                    description: String::new(),
                    unit: String::new(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            attributes: vec![],
                            start_time_unix_nano: 0,
                            time_unix_nano: 0,
                            value: Some(NDPValue::AsDouble(1.0)),
                            exemplars: vec![],
                            flags: 0,
                        }],
                    })),
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        };
        let events: Vec<_> = rm.into_otel_event_iter().collect();
        assert_eq!(events.len(), 1);
        let m = events[0].as_otel_metric();
        assert!(m.resource().is_none());
        assert!(m.scope().is_none());
        assert_eq!(m.metric().name, "bare.metric");
    }
}

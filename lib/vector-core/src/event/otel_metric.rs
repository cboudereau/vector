use opentelemetry_proto::tonic::common::v1::{
    KeyValue, any_value::Value as OtelValueKind,
};
use opentelemetry_proto::tonic::common::v1::AnyValue;
pub use opentelemetry_proto::tonic::metrics::v1::summary_data_point::ValueAtQuantile;
use opentelemetry_proto::tonic::metrics::v1::Metric as OtelMetricProto;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use serde::Serialize;
use vector_buffers::EventCount;
use vector_common::{
    EventDataEq,
    byte_size_of::ByteSizeOf,
    finalization::{EventFinalizers, Finalizable},
    internal_event::TaggedEventsSent,
    json_size::JsonSize,
    request_metadata::GetEventCountTags,
};

use super::{
    BatchNotifier, EstimatedJsonEncodedSizeOf, EventFinalizer, EventMetadata,
    otel_fields as f,
};
use super::otel_attributes::OtelAttributes;
use super::otel_event::{
    int_value, otel_value_to_tag_string, resource_to_proto, scope_to_proto, string_value,
};

// -- MetricView --

/// Zero-copy view into an `OtelMetric`'s data. Variant names follow OTLP types
/// (`Sum`, `Gauge`, `Histogram`, `Summary`, `ExponentialHistogram`) with
/// Vector-specific extensions (`Set`, `Distribution`).
///
/// Scalars are copied (cheap). Histogram bounds/counts and summary quantiles
/// are borrowed from the proto. Set values must allocate (stored in attributes).
#[derive(Debug)]
pub enum MetricView<'a> {
    Sum { value: f64 },
    Gauge { value: f64 },
    Set { values: Vec<String> },
    Distribution { bounds: &'a [f64], counts: &'a [u64] },
    Histogram { bounds: &'a [f64], counts: &'a [u64], count: u64, sum: f64 },
    Summary { quantiles: &'a [ValueAtQuantile], count: u64, sum: f64 },
    ExponentialHistogram { count: u64, sum: f64 },
}

impl MetricView<'_> {
    pub fn as_name(&self) -> &'static str {
        match self {
            Self::Sum { .. } => f::METRIC_TYPE_COUNTER,
            Self::Gauge { .. } => f::METRIC_TYPE_GAUGE,
            Self::Set { .. } => f::METRIC_TYPE_SET,
            Self::Distribution { .. } => f::METRIC_TYPE_DISTRIBUTION,
            Self::Histogram { .. } => f::METRIC_TYPE_HISTOGRAM,
            Self::Summary { .. } => f::METRIC_TYPE_SUMMARY,
            Self::ExponentialHistogram { .. } => "exponential histogram",
        }
    }
}

impl std::fmt::Display for MetricView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sum { value } => write!(f, "counter: {value}"),
            Self::Gauge { value } => write!(f, "gauge: {value}"),
            Self::Set { values } => write!(f, "set: {} values", values.len()),
            Self::Distribution { bounds, .. } => write!(f, "distribution: {} buckets", bounds.len()),
            Self::Histogram { bounds, count, sum, .. } => {
                write!(f, "histogram: {} buckets, count={count}, sum={sum}", bounds.len())
            }
            Self::Summary { quantiles, count, sum } => {
                write!(f, "summary: {} quantiles, count={count}, sum={sum}", quantiles.len())
            }
            Self::ExponentialHistogram { count, sum } => {
                write!(f, "exponential histogram: count={count}, sum={sum}")
            }
        }
    }
}

// -- OtelMetric --

#[derive(Clone, Debug, PartialEq)]
pub struct OtelMetric {
    pub(crate) metric: OtelMetricProto,
    pub(crate) dp_attrs: Vec<OtelAttributes>,
    pub(crate) resource: Option<Resource>,
    pub(crate) resource_attrs: OtelAttributes,
    pub(crate) scope: Option<InstrumentationScope>,
    pub(crate) scope_attrs: OtelAttributes,
    pub(crate) metadata: EventMetadata,
}

fn extract_dp_attrs(metric: &mut OtelMetricProto) -> Vec<OtelAttributes> {
    use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
    match metric.data.as_mut() {
        Some(MetricData::Sum(s)) => s.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Gauge(g)) => g.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Histogram(h)) => h.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::Summary(s)) => s.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        Some(MetricData::ExponentialHistogram(e)) => e.data_points.iter_mut()
            .map(|dp| OtelAttributes::from_key_values(std::mem::take(&mut dp.attributes))).collect(),
        None => vec![],
    }
}

fn populate_dp_attrs(metric: &mut OtelMetricProto, dp_attrs: &[OtelAttributes]) {
    use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
    macro_rules! write_back {
        ($data_points:expr) => {
            for (dp, attrs) in $data_points.iter_mut().zip(dp_attrs.iter()) {
                dp.attributes = attrs.to_key_values();
            }
        };
    }
    if let Some(data) = metric.data.as_mut() {
        match data {
            MetricData::Sum(s) => write_back!(s.data_points),
            MetricData::Gauge(g) => write_back!(g.data_points),
            MetricData::Histogram(h) => write_back!(h.data_points),
            MetricData::Summary(s) => write_back!(s.data_points),
            MetricData::ExponentialHistogram(e) => write_back!(e.data_points),
        }
    }
}

impl OtelMetric {
    pub fn new(mut metric: OtelMetricProto) -> Self {
        let dp_attrs = extract_dp_attrs(&mut metric);
        Self {
            metric,
            dp_attrs,
            resource: None,
            resource_attrs: OtelAttributes::new(),
            scope: None,
            scope_attrs: OtelAttributes::new(),
            metadata: EventMetadata::default(),
        }
    }

    /// Convenience constructor for a counter metric.
    /// Builds the OTLP proto directly without going through legacy Metric.
    pub fn new_counter(name: impl Into<String>, kind: super::MetricKind, value: f64) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, number_data_point::Value as NDPValue, NumberDataPoint, Sum,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Sum(Sum {
                data_points: vec![NumberDataPoint {
                    value: Some(NDPValue::AsDouble(value)),
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
                is_monotonic: true,
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for a gauge metric.
    /// Builds the OTLP proto directly without going through legacy Metric.
    pub fn new_gauge(name: impl Into<String>, value: f64) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            metric::Data, number_data_point::Value as NDPValue, Gauge, NumberDataPoint,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Gauge(Gauge {
                data_points: vec![NumberDataPoint {
                    value: Some(NDPValue::AsDouble(value)),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for an aggregated histogram metric.
    pub fn new_histogram(
        name: impl Into<String>,
        kind: super::MetricKind,
        buckets: &[super::metric::Bucket],
        count: u64,
        sum: f64,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, Histogram, HistogramDataPoint,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let n = buckets.len();
        let mut explicit_bounds = Vec::with_capacity(n);
        let mut bucket_counts = Vec::with_capacity(n);
        for b in buckets.iter() {
            bucket_counts.push(b.count);
            explicit_bounds.push(b.upper_limit);
        }
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    count,
                    sum: Some(sum),
                    bucket_counts,
                    explicit_bounds,
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for an aggregated summary metric.
    pub fn new_summary(
        name: impl Into<String>,
        quantiles: &[super::metric::Quantile],
        count: u64,
        sum: f64,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            metric::Data, Summary, SummaryDataPoint,
            summary_data_point::ValueAtQuantile,
        };
        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Summary(Summary {
                data_points: vec![SummaryDataPoint {
                    count,
                    sum,
                    quantile_values: quantiles
                        .iter()
                        .map(|q| ValueAtQuantile {
                            quantile: q.quantile,
                            value: q.value,
                        })
                        .collect(),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        Self::new(proto)
    }

    /// Convenience constructor for a set metric.
    /// OTel has no native Set type; represented as a Gauge whose value
    /// is the cardinality (number of unique values).
    pub fn new_set(name: impl Into<String>, cardinality: usize) -> Self {
        Self::new_gauge(name, cardinality as f64)
    }

    /// Convenience constructor for a distribution metric from samples.
    /// Represented as an OTLP Histogram with vector.metric_type=distribution
    /// and vector.statistic attribute indicating histogram vs summary.
    ///
    /// `statistic` should be `"histogram"` or `"summary"`.
    pub fn new_distribution_from_samples(
        name: impl Into<String>,
        kind: super::MetricKind,
        samples: &[super::metric::Sample],
        statistic: &str,
    ) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::{
            self as otel_metrics, metric::Data, Histogram, HistogramDataPoint,
        };
        let temporality = match kind {
            super::MetricKind::Incremental => otel_metrics::AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => otel_metrics::AggregationTemporality::Cumulative as i32,
        };
        let count = samples.iter().map(|s| s.rate).sum::<u32>() as u64;
        let sum: f64 = samples.iter().map(|s| s.value * s.rate as f64).sum();
        let explicit_bounds: Vec<f64> = samples.iter().map(|s| s.value).collect();
        let bucket_counts: Vec<u64> = samples.iter().map(|s| s.rate as u64).collect();

        let proto = OtelMetricProto {
            name: name.into(),
            data: Some(Data::Histogram(Histogram {
                data_points: vec![HistogramDataPoint {
                    count,
                    sum: Some(sum),
                    bucket_counts,
                    explicit_bounds,
                    ..Default::default()
                }],
                aggregation_temporality: temporality,
            })),
            ..Default::default()
        };
        let mut m = Self::new(proto);
        m.set_data_point_attribute(
            f::VECTOR_METRIC_TYPE.to_string(),
            string_value(f::METRIC_TYPE_DISTRIBUTION),
        );
        m.set_data_point_attribute(
            f::VECTOR_STATISTIC.to_string(),
            string_value(statistic),
        );
        m
    }

    /// Convenience constructor for a distribution metric (empty, no samples).
    /// Use `new_distribution_from_samples` when you have sample data.
    pub fn new_distribution(name: impl Into<String>, kind: super::MetricKind) -> Self {
        Self::new_distribution_from_samples(name, kind, &[], f::METRIC_TYPE_HISTOGRAM)
    }

    /// Convenience constructor for a set metric with its values.
    /// Represented as an OTLP Gauge with vector.metric_type=set,
    /// vector.set_values attribute, and cardinality as the numeric value.
    pub fn new_set_from_values(
        name: impl Into<String>,
        kind: super::MetricKind,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let mut values: Vec<String> = values.into_iter().map(Into::into).collect();
        values.sort();
        values.dedup();
        let cardinality = values.len() as f64;
        let mut m = Self::new_gauge(name, cardinality);
        m.set_data_point_attribute(
            f::VECTOR_METRIC_TYPE.to_string(),
            string_value(f::METRIC_TYPE_SET),
        );
        m.set_data_point_attribute(
            f::VECTOR_METRIC_KIND.to_string(),
            string_value(match kind {
                super::MetricKind::Incremental => f::METRIC_KIND_INCREMENTAL,
                super::MetricKind::Absolute => f::METRIC_KIND_ABSOLUTE,
            }),
        );
        let set_values: Vec<AnyValue> = values.iter().map(|v| string_value(v)).collect();
        m.set_data_point_attribute(
            f::VECTOR_SET_VALUES.to_string(),
            AnyValue {
                value: Some(OtelValueKind::ArrayValue(
                    opentelemetry_proto::tonic::common::v1::ArrayValue { values: set_values },
                )),
            },
        );
        m
    }

    /// Convenience constructor for a delta/signed gauge (e.g. statsd +/-).
    /// Represented as an OTLP Gauge with vector.metric_kind=incremental attribute.
    pub fn new_gauge_delta(name: impl Into<String>, value: f64) -> Self {
        let mut m = Self::new_gauge(name, value);
        m.set_data_point_attribute(
            f::VECTOR_METRIC_KIND.to_string(),
            string_value(f::METRIC_KIND_INCREMENTAL),
        );
        m
    }

    pub fn from_parts(
        mut metric: OtelMetricProto,
        mut resource: Option<Resource>,
        mut scope: Option<InstrumentationScope>,
        metadata: EventMetadata,
    ) -> Self {
        let dp_attrs = extract_dp_attrs(&mut metric);
        let resource_attrs = resource.as_mut()
            .map(|r| OtelAttributes::from_key_values(std::mem::take(&mut r.attributes)))
            .unwrap_or_default();
        let scope_attrs = scope.as_mut()
            .map(|s| OtelAttributes::from_key_values(std::mem::take(&mut s.attributes)))
            .unwrap_or_default();
        Self {
            metric,
            dp_attrs,
            resource,
            resource_attrs,
            scope,
            scope_attrs,
            metadata,
        }
    }

    pub fn into_parts(
        mut self,
    ) -> (
        OtelMetricProto,
        Option<Resource>,
        Option<InstrumentationScope>,
        EventMetadata,
    ) {
        populate_dp_attrs(&mut self.metric, &self.dp_attrs);
        let resource = self.resource.map(|mut r| {
            r.attributes = self.resource_attrs.to_key_values();
            r
        });
        let scope = self.scope.map(|mut s| {
            s.attributes = self.scope_attrs.to_key_values();
            s
        });
        (self.metric, resource, scope, self.metadata)
    }

    pub fn metric_proto(&self) -> OtelMetricProto {
        let mut m = self.metric.clone();
        populate_dp_attrs(&mut m, &self.dp_attrs);
        m
    }

    pub fn metric(&self) -> &OtelMetricProto {
        &self.metric
    }

    pub fn metric_mut(&mut self) -> &mut OtelMetricProto {
        &mut self.metric
    }

    pub fn resource(&self) -> Option<&Resource> {
        self.resource.as_ref()
    }

    pub fn resource_proto(&self) -> Option<Resource> {
        resource_to_proto(self.resource.as_ref(), &self.resource_attrs)
    }

    pub fn scope_proto(&self) -> Option<InstrumentationScope> {
        scope_to_proto(self.scope.as_ref(), &self.scope_attrs)
    }

    pub fn set_resource(&mut self, mut resource: Resource) {
        self.resource_attrs = OtelAttributes::from_key_values(std::mem::take(&mut resource.attributes));
        self.resource = Some(resource);
    }

    pub fn resource_attrs(&self) -> &OtelAttributes {
        &self.resource_attrs
    }

    pub fn scope_attrs(&self) -> &OtelAttributes {
        &self.scope_attrs
    }

    pub fn scope(&self) -> Option<&InstrumentationScope> {
        self.scope.as_ref()
    }

    pub fn set_scope(&mut self, mut scope: InstrumentationScope) {
        self.scope_attrs = OtelAttributes::from_key_values(std::mem::take(&mut scope.attributes));
        self.scope = Some(scope);
    }

    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut EventMetadata {
        &mut self.metadata
    }

    pub fn name(&self) -> &str {
        &self.metric.name
    }

    pub fn description(&self) -> &str {
        &self.metric.description
    }

    pub fn unit(&self) -> &str {
        &self.metric.unit
    }

    pub fn first_dp_attrs(&self) -> Option<&OtelAttributes> {
        self.dp_attrs.first()
    }

    pub fn set_data_point_attribute(&mut self, key: String, value: AnyValue) {
        for attrs in &mut self.dp_attrs {
            attrs.insert(key.clone(), value.clone());
        }
    }

    pub fn reduce_tags_to_single(&mut self) {
        for attrs in &mut self.dp_attrs {
            let updates: Vec<(String, AnyValue)> = attrs.iter()
                .filter_map(|(key, val)| {
                    if let Some(OtelValueKind::ArrayValue(arr)) = &val.value {
                        let last = arr.values.iter().rev().find_map(|v| match &v.value {
                            Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                            _ => None,
                        });
                        last.map(|s| (key.clone(), AnyValue {
                            value: Some(OtelValueKind::StringValue(s)),
                        }))
                    } else {
                        None
                    }
                })
                .collect();
            for (key, val) in updates {
                attrs.insert(key, val);
            }
        }
    }

    pub fn remove_data_point_attribute(&mut self, key: &str) -> Option<AnyValue> {
        let mut removed = None;
        for attrs in &mut self.dp_attrs {
            removed = removed.or(attrs.remove(key));
        }
        removed
    }

    /// Replace a tag: remove existing attribute then set new value.
    pub fn replace_tag(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        self.remove_data_point_attribute(&key);
        self.set_data_point_attribute(key, AnyValue {
            value: Some(OtelValueKind::StringValue(value.into())),
        });
    }

    pub fn resource_attribute(&self, key: &str) -> Option<&AnyValue> {
        self.resource_attrs.get(key)
    }

    pub fn set_resource_attribute(&mut self, key: String, value: AnyValue) {
        if self.resource.is_none() {
            self.resource = Some(Resource {
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            });
        }
        self.resource_attrs.insert(key, value);
    }

    // -----------------------------------------------------------------------
    // Metric accessors
    //
    // `value()`, `kind()`, `tag_value()` read proto directly via
    // `extract_metric_data()`. `timestamp()` and `namespace()` also read
    // proto directly. `tags()` is a broken stub — see its doc comment.
    // -----------------------------------------------------------------------

    /// Get the metric timestamp from the first data point.
    pub fn timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.to_legacy_metric_ref_timestamp()
    }

    /// Get the interval between start_time and end_time in milliseconds.
    pub fn interval_ms(&self) -> Option<std::num::NonZeroU32> {
        self.reconstruct_interval_ms()
    }

    fn to_legacy_metric_ref_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        use opentelemetry_proto::tonic::metrics::v1::metric;
        let data = self.metric.data.as_ref()?;
        let nanos = match data {
            metric::Data::Gauge(g) => g.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Sum(s) => s.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Histogram(h) => h.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::ExponentialHistogram(h) => h.data_points.first().map(|dp| dp.time_unix_nano),
            metric::Data::Summary(s) => s.data_points.first().map(|dp| dp.time_unix_nano),
        }?;
        if nanos == 0 { return None; }
        let secs = (nanos / 1_000_000_000) as i64;
        let nsecs = (nanos % 1_000_000_000) as u32;
        chrono::DateTime::from_timestamp(secs, nsecs)
    }

    /// Set the timestamp on all data points.
    ///
    /// Note: `None` sets `time_unix_nano` to 0, which `timestamp()` reads back
    /// as `None`. A `Some(ts)` at the Unix epoch (1970-01-01T00:00:00Z) also
    /// produces nanos == 0 and will round-trip as `None`.
    pub fn set_timestamp(&mut self, ts: Option<chrono::DateTime<chrono::Utc>>) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let nanos = ts.map(|t| t.timestamp_nanos_opt().unwrap_or(0) as u64).unwrap_or(0);
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => { for dp in &mut s.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Gauge(g) => { for dp in &mut g.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Histogram(h) => { for dp in &mut h.data_points { dp.time_unix_nano = nanos; } }
                MetricData::Summary(s) => { for dp in &mut s.data_points { dp.time_unix_nano = nanos; } }
                MetricData::ExponentialHistogram(e) => { for dp in &mut e.data_points { dp.time_unix_nano = nanos; } }
            }
        }
    }

    pub fn set_kind(&mut self, kind: super::MetricKind) {
        use opentelemetry_proto::tonic::metrics::v1::{metric::Data as MetricData, AggregationTemporality};
        let temp = match kind {
            super::MetricKind::Incremental => AggregationTemporality::Delta as i32,
            super::MetricKind::Absolute => AggregationTemporality::Cumulative as i32,
        };
        if let Some(data) = self.metric.data.as_mut() {
            match data {
                MetricData::Sum(s) => s.aggregation_temporality = temp,
                MetricData::Histogram(h) => h.aggregation_temporality = temp,
                MetricData::ExponentialHistogram(e) => e.aggregation_temporality = temp,
                MetricData::Gauge(_) | MetricData::Summary(_) => {
                    if self.is_set() {
                        let kind_str = match kind {
                            super::MetricKind::Incremental => f::METRIC_KIND_INCREMENTAL,
                            super::MetricKind::Absolute => f::METRIC_KIND_ABSOLUTE,
                        };
                        self.set_data_point_attribute(
                            f::VECTOR_METRIC_KIND.to_string(),
                            string_value(kind_str),
                        );
                    } else {
                        match kind {
                            super::MetricKind::Incremental => {
                                self.set_data_point_attribute(
                                    f::VECTOR_METRIC_KIND.to_string(),
                                    string_value(f::METRIC_KIND_INCREMENTAL),
                                );
                            }
                            super::MetricKind::Absolute => {
                                self.remove_data_point_attribute(f::VECTOR_METRIC_KIND);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn set_namespace(&mut self, namespace: impl Into<String>) {
        self.set_resource_attribute(f::METRIC_NAMESPACE.to_string(), string_value(&namespace.into()));
    }

    /// Builder-style: set the metric namespace (stored as `metric.namespace` resource attribute).
    pub fn with_namespace(mut self, namespace: Option<impl Into<String>>) -> Self {
        if let Some(ns) = namespace {
            if self.resource.is_none() {
                self.resource = Some(Resource {
                    attributes: Vec::new(),
                    dropped_attributes_count: 0,
                });
            }
            self.resource_attrs.insert(f::METRIC_NAMESPACE.to_string(), string_value(&ns.into()));
        }
        self
    }

    /// Builder-style: set the timestamp on all data points.
    pub fn with_timestamp(mut self, ts: Option<chrono::DateTime<chrono::Utc>>) -> Self {
        self.set_timestamp(ts);
        self
    }

    /// Builder-style: set interval_ms by adjusting start_time_unix_nano relative to time_unix_nano.
    pub fn with_interval_ms(mut self, interval: Option<std::num::NonZeroU32>) -> Self {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as ProtoMetricData;
        let Some(interval) = interval else { return self };
        if let Some(data) = self.metric.data.as_mut() {
            let adjust = |start: &mut u64, end: u64| {
                if end > 0 {
                    *start = end.saturating_sub(u64::from(interval.get()) * 1_000_000);
                }
            };
            match data {
                ProtoMetricData::Sum(s) => {
                    if let Some(dp) = s.data_points.first_mut() { adjust(&mut dp.start_time_unix_nano, dp.time_unix_nano); }
                }
                ProtoMetricData::Gauge(g) => {
                    if let Some(dp) = g.data_points.first_mut() { adjust(&mut dp.start_time_unix_nano, dp.time_unix_nano); }
                }
                ProtoMetricData::Histogram(h) => {
                    if let Some(dp) = h.data_points.first_mut() { adjust(&mut dp.start_time_unix_nano, dp.time_unix_nano); }
                }
                ProtoMetricData::Summary(s) => {
                    if let Some(dp) = s.data_points.first_mut() { adjust(&mut dp.start_time_unix_nano, dp.time_unix_nano); }
                }
                ProtoMetricData::ExponentialHistogram(e) => {
                    if let Some(dp) = e.data_points.first_mut() { adjust(&mut dp.start_time_unix_nano, dp.time_unix_nano); }
                }
            }
        }
        self
    }

    /// Builder-style: set tags as data point attributes from OtelAttributes.
    pub fn with_tags(mut self, tags: Option<OtelAttributes>) -> Self {
        let Some(tags) = tags else { return self };
        for (key, value) in tags.inner {
            self.remove_data_point_attribute(&key);
            self.set_data_point_attribute(key, value);
        }
        self
    }

    pub fn with_metadata(mut self, metadata: EventMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build OtelAttributes from proto data point, resource, and scope attributes.
    ///
    /// Returns an owned `OtelAttributes` because the tags are assembled from
    /// multiple proto fields. Returns `None` if there are no tags at all.
    pub fn tags(&self) -> Option<OtelAttributes> {
        let mut attrs = OtelAttributes::new();

        // Resource attributes (prefixed with "resource.")
        for (key, val) in self.resource_attrs.iter() {
            if key == f::METRIC_NAMESPACE {
                continue;
            }
            if val.value.is_some() {
                attrs.insert(format!("resource.{}", key), val.clone());
            }
        }

        // Scope attributes
        if let Some(ref scope) = self.scope {
            if !scope.name.is_empty() {
                attrs.insert("scope.name".to_string(), string_value(&scope.name));
            }
            if !scope.version.is_empty() {
                attrs.insert("scope.version".to_string(), string_value(&scope.version));
            }
        }

        // Data point attributes
        if let Some(dp) = self.dp_attrs.first() {
            for (key, val) in dp.iter() {
                if key.starts_with(f::VECTOR_PREFIX) {
                    continue;
                }
                attrs.insert(key.clone(), val.clone());
            }
        }

        attrs.as_option()
    }

    /// Get the metric namespace from the `metric.namespace` resource attribute.
    pub fn namespace(&self) -> Option<&str> {
        self.resource_attrs.get(f::METRIC_NAMESPACE)
            .and_then(|av| match &av.value {
                Some(OtelValueKind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
    }

    /// Get the metric kind directly from proto.
    pub fn kind(&self) -> super::MetricKind {
        use opentelemetry_proto::tonic::metrics::v1::{metric, AggregationTemporality};
        match self.metric.data.as_ref() {
            Some(metric::Data::Sum(sum)) => {
                if sum.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    super::MetricKind::Incremental
                } else {
                    super::MetricKind::Absolute
                }
            }
            Some(metric::Data::Gauge(_)) => {
                let is_incremental = self.dp_attrs.first()
                    .and_then(|a| a.get(f::VECTOR_METRIC_KIND))
                    .and_then(|av| av.value.as_ref())
                    .map(|v| v == &OtelValueKind::StringValue(f::METRIC_KIND_INCREMENTAL.into()))
                    .unwrap_or(false);
                if is_incremental {
                    super::MetricKind::Incremental
                } else {
                    super::MetricKind::Absolute
                }
            }
            Some(metric::Data::Histogram(hist)) => {
                if hist.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    super::MetricKind::Incremental
                } else {
                    super::MetricKind::Absolute
                }
            }
            Some(metric::Data::ExponentialHistogram(exp)) => {
                if exp.aggregation_temporality == AggregationTemporality::Delta as i32 {
                    super::MetricKind::Incremental
                } else {
                    super::MetricKind::Absolute
                }
            }
            Some(metric::Data::Summary(_)) | None => super::MetricKind::Absolute,
        }
    }

    pub fn tag_value(&self, key: &str) -> Option<String> {
        if let Some(dp) = self.dp_attrs.first() {
            if let Some(av) = dp.get(key) {
                if let Some(ref v) = av.value {
                    return Some(otel_value_to_tag_string(v));
                }
            }
        }
        // Check resource attributes (prefixed with "resource." in legacy)
        if let Some(stripped) = key.strip_prefix("resource.") {
            if let Some(av) = self.resource_attrs.get(stripped) {
                if let Some(ref v) = av.value {
                    return Some(otel_value_to_tag_string(v));
                }
            }
        }
        // Check scope attributes
        if let Some(ref scope) = self.scope {
            match key {
                "scope.name" if !scope.name.is_empty() => return Some(scope.name.clone()),
                "scope.version" if !scope.version.is_empty() => return Some(scope.version.clone()),
                _ => {}
            }
        }
        None
    }

    /// Check whether a tag with the given name matches the given value.
    pub fn tag_matches(&self, name: &str, value: &str) -> bool {
        self.tag_value(name)
            .filter(|v| v == value)
            .is_some()
    }

    /// Decompose this OtelMetric into legacy metric parts without creating
    /// an intermediate Metric. Used by aggregate and other transforms that
    /// store MetricSeries/MetricData separately.
    /// Build a `MetricSeries` key for this metric (name + namespace + tags).
    /// This is the grouping key used by aggregate/normalization.
    pub fn metric_series(&self) -> super::metric::MetricIdentity {
        super::metric::MetricIdentity {
            name: self.metric.name.clone(),
            namespace: self.namespace().map(|s| s.to_string()),
            tags: self.tags(),
        }
    }

    fn reconstruct_interval_ms(&self) -> Option<std::num::NonZeroU32> {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
        let dp_times = match self.metric.data.as_ref()? {
            MetricData::Sum(s) => s.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Gauge(g) => g.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Histogram(h) => h.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::Summary(s) => s.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
            MetricData::ExponentialHistogram(e) => e.data_points.first().map(|p| (p.start_time_unix_nano, p.time_unix_nano)),
        };
        dp_times.and_then(|(start, end)| {
            if start > 0 && end > start {
                let diff_ms = (end - start) / 1_000_000;
                std::num::NonZeroU32::new(diff_ms as u32)
            } else {
                None
            }
        })
    }

    pub fn add_finalizer(&mut self, finalizer: EventFinalizer) {
        self.metadata.add_finalizer(finalizer);
    }

    #[must_use]
    pub fn with_batch_notifier(mut self, batch: &BatchNotifier) -> Self {
        self.metadata = self.metadata.with_batch_notifier(batch);
        self
    }

    #[must_use]
    pub fn with_batch_notifier_option(mut self, batch: &Option<BatchNotifier>) -> Self {
        self.metadata = self.metadata.with_batch_notifier_option(batch);
        self
    }

    /// Merge set values from `other` into this set metric.
    /// Combines the `vector.set_values` arrays (deduplicating) and updates
    /// the numeric cardinality value.
    fn merge_set_values(&mut self, other: &Self) {
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;
        use std::collections::BTreeSet;

        let extract_set = |m: &Self| -> BTreeSet<String> {
            m.dp_attrs.first()
                .and_then(|a| a.get(f::VECTOR_SET_VALUES))
                .and_then(|av| match &av.value {
                    Some(OtelValueKind::ArrayValue(arr)) => {
                        Some(arr.values.iter().filter_map(|v| match &v.value {
                            Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                            _ => None,
                        }).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };

        let mut merged = extract_set(self);
        merged.extend(extract_set(other));

        let cardinality = merged.len() as f64;
        let set_values: Vec<AnyValue> = merged.iter().map(|v| string_value(v)).collect();
        self.set_data_point_attribute(
            f::VECTOR_SET_VALUES.to_string(),
            AnyValue {
                value: Some(OtelValueKind::ArrayValue(
                    opentelemetry_proto::tonic::common::v1::ArrayValue { values: set_values },
                )),
            },
        );

        if let Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(g)) =
            self.metric.data.as_mut()
        {
            if let Some(dp) = g.data_points.first_mut() {
                dp.value = Some(NDPValue::AsDouble(cardinality));
            }
        }
    }

    fn subtract_distribution(&mut self, other: &Self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;

        let (Some(MD::Histogram(h)), Some(MD::Histogram(oh))) =
            (self.metric.data.as_mut(), other.metric.data.as_ref())
        else {
            return false;
        };

        for (dp, odp) in h.data_points.iter_mut().zip(oh.data_points.iter()) {
            let other_pairs: Vec<(f64, u64)> = odp
                .explicit_bounds
                .iter()
                .copied()
                .zip(odp.bucket_counts.iter().copied())
                .collect();

            let self_pairs: Vec<(f64, u64)> = dp
                .explicit_bounds
                .iter()
                .copied()
                .zip(dp.bucket_counts.iter().copied())
                .collect();

            let filtered: Vec<(f64, u64)> = self_pairs
                .iter()
                .copied()
                .filter(|pair| other_pairs.iter().all(|op| *pair != *op))
                .collect();

            dp.explicit_bounds = filtered.iter().map(|(b, _)| *b).collect();
            dp.bucket_counts = filtered.iter().map(|(_, c)| *c).collect();
            dp.count = dp.bucket_counts.iter().sum();
            dp.sum = Some(
                dp.explicit_bounds
                    .iter()
                    .zip(dp.bucket_counts.iter())
                    .map(|(b, c)| b * (*c as f64))
                    .sum(),
            );
        }
        true
    }

    fn subtract_set_values(&mut self, other: &Self) {
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;
        use std::collections::BTreeSet;

        let extract_set = |m: &Self| -> BTreeSet<String> {
            m.dp_attrs.first()
                .and_then(|a| a.get(f::VECTOR_SET_VALUES))
                .and_then(|av| match &av.value {
                    Some(OtelValueKind::ArrayValue(arr)) => {
                        Some(arr.values.iter().filter_map(|v| match &v.value {
                            Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                            _ => None,
                        }).collect())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };

        let mut self_set = extract_set(self);
        let other_set = extract_set(other);
        for item in &other_set {
            self_set.remove(item);
        }

        let cardinality = self_set.len() as f64;
        let set_values: Vec<AnyValue> = self_set.iter().map(|v| string_value(v)).collect();
        self.set_data_point_attribute(
            f::VECTOR_SET_VALUES.to_string(),
            AnyValue {
                value: Some(OtelValueKind::ArrayValue(
                    opentelemetry_proto::tonic::common::v1::ArrayValue { values: set_values },
                )),
            },
        );

        if let Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(g)) =
            self.metric.data.as_mut()
        {
            if let Some(dp) = g.data_points.first_mut() {
                dp.value = Some(NDPValue::AsDouble(cardinality));
            }
        }
    }

    /// Compress a distribution-type histogram in place by sorting bounds and
    /// merging bucket counts for duplicate bound values.
    pub fn compress_distribution(&mut self) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        if let Some(MD::Histogram(h)) = self.metric.data.as_mut() {
            for dp in &mut h.data_points {
                if dp.explicit_bounds.len() != dp.bucket_counts.len() {
                    continue;
                }
                let mut pairs: Vec<(f64, u64)> = dp
                    .explicit_bounds
                    .iter()
                    .copied()
                    .zip(dp.bucket_counts.iter().copied())
                    .collect();
                pairs.sort_by(|a, b| a.0.total_cmp(&b.0));

                let mut bounds = Vec::with_capacity(pairs.len());
                let mut counts = Vec::with_capacity(pairs.len());
                for (val, rate) in pairs {
                    if let Some(last) = bounds.last()
                        && *last == val
                    {
                        *counts.last_mut().unwrap() += rate;
                    } else {
                        bounds.push(val);
                        counts.push(rate);
                    }
                }
                dp.explicit_bounds = bounds;
                dp.bucket_counts = counts;
            }
        }
    }

    /// Add the data from `other` to this metric.
    ///
    /// Both metrics must have the same data type (Sum+Sum, Gauge+Gauge, etc.).
    /// For Histogram, bucket layouts (explicit_bounds) must match.
    /// Returns `false` if the types are incompatible.
    #[must_use]
    pub fn add(&mut self, other: &Self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        if self.is_set() && other.is_set() {
            self.merge_set_values(other);
            return true;
        }

        match (self.metric.data.as_mut(), other.metric.data.as_ref()) {
            (Some(MD::Sum(s)), Some(MD::Sum(o))) => {
                for (dp, odp) in s.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v += ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v += ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Gauge(g)), Some(MD::Gauge(o))) => {
                for (dp, odp) in g.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v += ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v += ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Histogram(h)), Some(MD::Histogram(oh))) => {
                for (dp, odp) in h.data_points.iter_mut().zip(oh.data_points.iter()) {
                    if dp.explicit_bounds != odp.explicit_bounds
                        || dp.bucket_counts.len() != odp.bucket_counts.len()
                    {
                        return false;
                    }
                    for (bc, obc) in dp.bucket_counts.iter_mut().zip(odp.bucket_counts.iter()) {
                        *bc += obc;
                    }
                    dp.count += odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) + odp.sum.unwrap_or(0.0));
                }
                true
            }
            (Some(MD::Summary(_)), Some(MD::Summary(_))) => {
                // Summaries (quantile sketches) cannot be meaningfully added
                false
            }
            (Some(MD::ExponentialHistogram(eh)), Some(MD::ExponentialHistogram(oeh))) => {
                for (dp, odp) in eh.data_points.iter_mut().zip(oeh.data_points.iter()) {
                    if dp.scale != odp.scale {
                        return false;
                    }
                    dp.count += odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) + odp.sum.unwrap_or(0.0));
                    dp.zero_count += odp.zero_count;
                    if let (Some(pos), Some(opos)) = (&mut dp.positive, &odp.positive) {
                        if pos.offset == opos.offset && pos.bucket_counts.len() == opos.bucket_counts.len() {
                            for (bc, obc) in pos.bucket_counts.iter_mut().zip(opos.bucket_counts.iter()) {
                                *bc += obc;
                            }
                        } else {
                            return false;
                        }
                    }
                    if let (Some(neg), Some(oneg)) = (&mut dp.negative, &odp.negative) {
                        if neg.offset == oneg.offset && neg.bucket_counts.len() == oneg.bucket_counts.len() {
                            for (bc, obc) in neg.bucket_counts.iter_mut().zip(oneg.bucket_counts.iter()) {
                                *bc += obc;
                            }
                        } else {
                            return false;
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Subtract the data of `other` from this metric.
    ///
    /// Both metrics must have the same data type. For counters (Sum),
    /// this is monotonic: returns `false` if subtraction would go negative.
    #[must_use]
    pub fn subtract(&mut self, other: &Self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        if self.is_set() && other.is_set() {
            self.subtract_set_values(other);
            return true;
        }
        if self.is_set() || other.is_set() {
            return false;
        }

        if self.is_distribution() && other.is_distribution() {
            return self.subtract_distribution(other);
        }

        match (self.metric.data.as_mut(), other.metric.data.as_ref()) {
            (Some(MD::Sum(s)), Some(MD::Sum(o))) => {
                for (dp, odp) in s.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => {
                            if *v < *ov { return false; }
                            *v -= ov;
                        }
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => {
                            if *v < *ov { return false; }
                            *v -= ov;
                        }
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Gauge(g)), Some(MD::Gauge(o))) => {
                for (dp, odp) in g.data_points.iter_mut().zip(o.data_points.iter()) {
                    match (&mut dp.value, &odp.value) {
                        (Some(NDPValue::AsDouble(v)), Some(NDPValue::AsDouble(ov))) => *v -= ov,
                        (Some(NDPValue::AsInt(v)), Some(NDPValue::AsInt(ov))) => *v -= ov,
                        _ => return false,
                    }
                }
                true
            }
            (Some(MD::Histogram(h)), Some(MD::Histogram(oh))) => {
                for (dp, odp) in h.data_points.iter_mut().zip(oh.data_points.iter()) {
                    if dp.explicit_bounds != odp.explicit_bounds
                        || dp.bucket_counts.len() != odp.bucket_counts.len()
                        || dp.count < odp.count
                    {
                        return false;
                    }
                    for (bc, obc) in dp.bucket_counts.iter_mut().zip(odp.bucket_counts.iter()) {
                        if *bc < *obc { return false; }
                        *bc -= obc;
                    }
                    dp.count -= odp.count;
                    dp.sum = Some(dp.sum.unwrap_or(0.0) - odp.sum.unwrap_or(0.0));
                }
                true
            }
            _ => false,
        }
    }

    /// Zero out all data point values in this metric.
    pub fn zero(&mut self) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_mut() {
            Some(MD::Sum(s)) => {
                for dp in &mut s.data_points {
                    match &mut dp.value {
                        Some(NDPValue::AsDouble(v)) => *v = 0.0,
                        Some(NDPValue::AsInt(v)) => *v = 0,
                        _ => {}
                    }
                }
            }
            Some(MD::Gauge(g)) => {
                for dp in &mut g.data_points {
                    match &mut dp.value {
                        Some(NDPValue::AsDouble(v)) => *v = 0.0,
                        Some(NDPValue::AsInt(v)) => *v = 0,
                        _ => {}
                    }
                }
            }
            Some(MD::Histogram(h)) => {
                for dp in &mut h.data_points {
                    for bc in &mut dp.bucket_counts { *bc = 0; }
                    dp.count = 0;
                    dp.sum = Some(0.0);
                }
            }
            Some(MD::Summary(s)) => {
                for dp in &mut s.data_points {
                    for qv in &mut dp.quantile_values { qv.value = 0.0; }
                    dp.count = 0;
                    dp.sum = 0.0;
                }
            }
            Some(MD::ExponentialHistogram(eh)) => {
                for dp in &mut eh.data_points {
                    dp.count = 0;
                    dp.sum = Some(0.0);
                    dp.zero_count = 0;
                    if let Some(ref mut pos) = dp.positive {
                        for bc in &mut pos.bucket_counts { *bc = 0; }
                    }
                    if let Some(ref mut neg) = dp.negative {
                        for bc in &mut neg.bucket_counts { *bc = 0; }
                    }
                }
            }
            None => {}
        }
    }

    /// Set the first data point value (Sum or Gauge only).
    pub fn set_first_value(&mut self, val: f64) {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_mut() {
            Some(MD::Sum(s)) => {
                if let Some(dp) = s.data_points.first_mut() {
                    dp.value = Some(NDPValue::AsDouble(val));
                }
            }
            Some(MD::Gauge(g)) => {
                if let Some(dp) = g.data_points.first_mut() {
                    dp.value = Some(NDPValue::AsDouble(val));
                }
            }
            _ => {}
        }
    }

    /// Get the first data point value as f64, if this is a Sum or Gauge.
    pub fn first_value_as_f64(&self) -> Option<f64> {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;

        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.data_points.first().and_then(|dp| match &dp.value {
                Some(NDPValue::AsDouble(v)) => Some(*v),
                Some(NDPValue::AsInt(v)) => Some(*v as f64),
                _ => None,
            }),
            Some(MD::Gauge(g)) => g.data_points.first().and_then(|dp| match &dp.value {
                Some(NDPValue::AsDouble(v)) => Some(*v),
                Some(NDPValue::AsInt(v)) => Some(*v as f64),
                _ => None,
            }),
            _ => None,
        }
    }

    /// Check if this metric is a delta (incremental) type.
    /// Only Sum, Histogram, and ExponentialHistogram have AggregationTemporality.
    /// Gauge and Summary are point-in-time and neither delta nor cumulative.
    pub fn is_delta(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::{AggregationTemporality, metric::Data as MD};
        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.aggregation_temporality == AggregationTemporality::Delta as i32,
            Some(MD::Histogram(h)) => h.aggregation_temporality == AggregationTemporality::Delta as i32,
            Some(MD::ExponentialHistogram(eh)) => eh.aggregation_temporality == AggregationTemporality::Delta as i32,
            _ => false,
        }
    }

    /// Check if this metric is cumulative. Gauge and Summary have no temporality
    /// and return `false` (per OTel spec and otelcol-contrib behavior).
    pub fn is_cumulative(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::{AggregationTemporality, metric::Data as MD};
        match self.metric.data.as_ref() {
            Some(MD::Sum(s)) => s.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            Some(MD::Histogram(h)) => h.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            Some(MD::ExponentialHistogram(eh)) => eh.aggregation_temporality == AggregationTemporality::Cumulative as i32,
            _ => false,
        }
    }

    /// Check if this metric type carries an aggregation temporality field.
    /// Sum, Histogram, and ExponentialHistogram have temporality.
    /// Gauge and Summary do not.
    pub fn has_temporality(&self) -> bool {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MD;
        matches!(self.metric.data.as_ref(), Some(MD::Sum(_) | MD::Histogram(_) | MD::ExponentialHistogram(_)))
    }

    /// Check if this metric is a Gauge type.
    pub fn is_gauge(&self) -> bool {
        matches!(self.metric.data.as_ref(), Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Gauge(_)))
    }

    /// Check if this metric is a Sum type.
    pub fn is_sum(&self) -> bool {
        matches!(self.metric.data.as_ref(), Some(opentelemetry_proto::tonic::metrics::v1::metric::Data::Sum(_)))
    }

    /// Check if this metric is a Set (stored as Gauge with vector.metric_type=set attribute).
    pub fn is_set(&self) -> bool {
        self.dp_attrs.first()
            .and_then(|attrs| attrs.get(f::VECTOR_METRIC_TYPE))
            .and_then(|av| av.value.as_ref())
            .is_some_and(|v| matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == f::METRIC_TYPE_SET))
    }

    /// Check if this metric is a Distribution (stored as Histogram with vector.metric_type=distribution attribute).
    pub fn is_distribution(&self) -> bool {
        self.dp_attrs.first()
            .and_then(|attrs| attrs.get(f::VECTOR_METRIC_TYPE))
            .and_then(|av| av.value.as_ref())
            .is_some_and(|v| matches!(v, opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) if s == f::METRIC_TYPE_DISTRIBUTION))
    }

    /// Returns the distribution statistic type string ("histogram" or "summary").
    /// Returns "histogram" by default (including for non-distribution metrics).
    pub fn distribution_statistic(&self) -> &str {
        self.dp_attrs.first()
            .and_then(|attrs| attrs.get(f::VECTOR_STATISTIC))
            .and_then(|av| match &av.value {
                Some(OtelValueKind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or(f::METRIC_TYPE_HISTOGRAM)
    }

    pub fn is_distribution_summary(&self) -> bool {
        self.distribution_statistic() == f::METRIC_TYPE_SUMMARY
    }

    /// Returns a zero-copy view of this metric's data, borrowing from the
    /// underlying proto where possible.
    pub fn view(&self) -> MetricView<'_> {
        use opentelemetry_proto::tonic::metrics::v1::{
            metric::Data, number_data_point::Value as NDPValue,
        };

        match self.metric.data.as_ref() {
            Some(Data::Sum(sum)) => {
                let val = sum.data_points.first()
                    .and_then(|p| p.value.as_ref())
                    .map(|v| match v { NDPValue::AsDouble(f) => *f, NDPValue::AsInt(i) => *i as f64 })
                    .unwrap_or(0.0);
                if sum.is_monotonic {
                    MetricView::Sum { value: val }
                } else {
                    MetricView::Gauge { value: val }
                }
            }
            Some(Data::Gauge(_gauge)) => {
                if self.is_set() {
                    let values = self.dp_attrs.first()
                        .and_then(|a| a.get(f::VECTOR_SET_VALUES))
                        .and_then(|av| match &av.value {
                            Some(OtelValueKind::ArrayValue(arr)) => {
                                Some(arr.values.iter().filter_map(|v| match &v.value {
                                    Some(OtelValueKind::StringValue(s)) => Some(s.clone()),
                                    _ => None,
                                }).collect())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    MetricView::Set { values }
                } else {
                    let val = _gauge.data_points.first()
                        .and_then(|p| p.value.as_ref())
                        .map(|v| match v { NDPValue::AsDouble(f) => *f, NDPValue::AsInt(i) => *i as f64 })
                        .unwrap_or(0.0);
                    MetricView::Gauge { value: val }
                }
            }
            Some(Data::Histogram(hist)) => {
                let dp = hist.data_points.first();
                if self.is_distribution() {
                    match dp {
                        Some(p) => MetricView::Distribution {
                            bounds: &p.explicit_bounds,
                            counts: &p.bucket_counts,
                        },
                        None => MetricView::Distribution { bounds: &[], counts: &[] },
                    }
                } else {
                    match dp {
                        Some(p) => MetricView::Histogram {
                            bounds: &p.explicit_bounds,
                            counts: &p.bucket_counts,
                            count: p.count,
                            sum: p.sum.unwrap_or(0.0),
                        },
                        None => MetricView::Histogram { bounds: &[], counts: &[], count: 0, sum: 0.0 },
                    }
                }
            }
            Some(Data::Summary(summary)) => {
                match summary.data_points.first() {
                    Some(p) => MetricView::Summary {
                        quantiles: &p.quantile_values,
                        count: p.count,
                        sum: p.sum,
                    },
                    None => MetricView::Summary { quantiles: &[], count: 0, sum: 0.0 },
                }
            }
            Some(Data::ExponentialHistogram(exp)) => {
                let dp = exp.data_points.first();
                MetricView::ExponentialHistogram {
                    count: dp.map(|p| p.count).unwrap_or(0),
                    sum: dp.and_then(|p| p.sum).unwrap_or(0.0),
                }
            }
            None => MetricView::Gauge { value: 0.0 },
        }
    }

    /// Convert this metric to an `AnyValue::KvlistValue` suitable for use as
    /// an OtelLog body. Includes name, description, unit, and data (sum/gauge/
    /// histogram/summary/exponentialHistogram). Does NOT include resource/scope
    /// — those should be transferred directly to the OtelLog.
    pub fn to_log_body(&self) -> AnyValue {
        use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;

        let metric = self.metric_proto();
        let mut kvs: Vec<KeyValue> = Vec::new();

        kvs.push(KeyValue { key: f::NAME.into(), value: Some(string_value(&metric.name)) });
        if !metric.description.is_empty() {
            kvs.push(KeyValue { key: f::DESCRIPTION.into(), value: Some(string_value(&metric.description)) });
        }
        if !metric.unit.is_empty() {
            kvs.push(KeyValue { key: f::UNIT.into(), value: Some(string_value(&metric.unit)) });
        }

        if let Some(ref data) = metric.data {
            match data {
                MetricData::Sum(sum) => {
                    kvs.push(KeyValue { key: f::METRIC_TYPE_SUM.into(), value: Some(sum_to_any_value(sum)) });
                }
                MetricData::Gauge(gauge) => {
                    kvs.push(KeyValue { key: f::METRIC_TYPE_GAUGE.into(), value: Some(gauge_to_any_value(gauge)) });
                }
                MetricData::Histogram(hist) => {
                    kvs.push(KeyValue { key: f::METRIC_TYPE_HISTOGRAM.into(), value: Some(histogram_to_any_value(hist)) });
                }
                MetricData::Summary(summary) => {
                    kvs.push(KeyValue { key: f::METRIC_TYPE_SUMMARY.into(), value: Some(summary_to_any_value(summary)) });
                }
                MetricData::ExponentialHistogram(exp) => {
                    kvs.push(KeyValue { key: f::EXPONENTIAL_HISTOGRAM_CC.into(), value: Some(exp_histogram_to_any_value(exp)) });
                }
            }
        }

        AnyValue {
            value: Some(OtelValueKind::KvlistValue(
                opentelemetry_proto::tonic::common::v1::KeyValueList { values: kvs },
            )),
        }
    }
}

// -- Helper functions for to_log_body --

fn double_value(d: f64) -> AnyValue {
    AnyValue { value: Some(OtelValueKind::DoubleValue(d)) }
}

fn bool_value(b: bool) -> AnyValue {
    AnyValue { value: Some(OtelValueKind::BoolValue(b)) }
}

fn kvlist_any_value(kvs: Vec<KeyValue>) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::KvlistValue(
            opentelemetry_proto::tonic::common::v1::KeyValueList { values: kvs },
        )),
    }
}

fn array_any_value(values: Vec<AnyValue>) -> AnyValue {
    AnyValue {
        value: Some(OtelValueKind::ArrayValue(
            opentelemetry_proto::tonic::common::v1::ArrayValue { values },
        )),
    }
}

fn attrs_to_any_value(attrs: &[KeyValue]) -> AnyValue {
    array_any_value(
        attrs.iter().map(|kv| {
            let mut inner = vec![
                KeyValue { key: f::KEY.into(), value: Some(string_value(&kv.key)) },
            ];
            if let Some(ref v) = kv.value {
                inner.push(KeyValue { key: f::VALUE.into(), value: Some(v.clone()) });
            }
            kvlist_any_value(inner)
        }).collect()
    )
}

fn number_dp_to_any_value(
    dp: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint,
) -> AnyValue {
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NDPValue;
    let mut kvs = Vec::new();
    if let Some(ref v) = dp.value {
        match v {
            NDPValue::AsDouble(d) => kvs.push(KeyValue { key: f::AS_DOUBLE.into(), value: Some(double_value(*d)) }),
            NDPValue::AsInt(i) => kvs.push(KeyValue { key: f::AS_INT.into(), value: Some(int_value(*i)) }),
        }
    }
    if !dp.attributes.is_empty() {
        kvs.push(KeyValue { key: f::ATTRIBUTES.into(), value: Some(attrs_to_any_value(&dp.attributes)) });
    }
    if dp.time_unix_nano != 0 {
        kvs.push(KeyValue { key: f::TIME_UNIX_NANO_CC.into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
    }
    if dp.start_time_unix_nano != 0 {
        kvs.push(KeyValue { key: f::START_TIME_UNIX_NANO_CC.into(), value: Some(string_value(dp.start_time_unix_nano.to_string())) });
    }
    kvlist_any_value(kvs)
}

fn sum_to_any_value(sum: &opentelemetry_proto::tonic::metrics::v1::Sum) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = sum.data_points.iter().map(number_dp_to_any_value).collect();
    kvs.push(KeyValue { key: f::DATA_POINTS.into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: f::AGGREGATION_TEMPORALITY.into(), value: Some(int_value(sum.aggregation_temporality as i64)) });
    kvs.push(KeyValue { key: f::IS_MONOTONIC.into(), value: Some(bool_value(sum.is_monotonic)) });
    kvlist_any_value(kvs)
}

fn gauge_to_any_value(gauge: &opentelemetry_proto::tonic::metrics::v1::Gauge) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = gauge.data_points.iter().map(number_dp_to_any_value).collect();
    kvs.push(KeyValue { key: f::DATA_POINTS.into(), value: Some(array_any_value(dps)) });
    kvlist_any_value(kvs)
}

fn histogram_to_any_value(hist: &opentelemetry_proto::tonic::metrics::v1::Histogram) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = hist.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: f::ATTRIBUTES.into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: f::TIME_UNIX_NANO_CC.into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: f::COUNT.into(), value: Some(string_value(dp.count.to_string())) });
        if let Some(sum) = dp.sum {
            m.push(KeyValue { key: f::METRIC_TYPE_SUM.into(), value: Some(double_value(sum)) });
        }
        if !dp.bucket_counts.is_empty() {
            m.push(KeyValue { key: f::BUCKET_COUNTS.into(), value: Some(array_any_value(
                dp.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
            )) });
        }
        if !dp.explicit_bounds.is_empty() {
            m.push(KeyValue { key: f::EXPLICIT_BOUNDS.into(), value: Some(array_any_value(
                dp.explicit_bounds.iter().map(|b| double_value(*b)).collect()
            )) });
        }
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: f::DATA_POINTS.into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: f::AGGREGATION_TEMPORALITY.into(), value: Some(int_value(hist.aggregation_temporality as i64)) });
    kvlist_any_value(kvs)
}

fn summary_to_any_value(summary: &opentelemetry_proto::tonic::metrics::v1::Summary) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = summary.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: f::ATTRIBUTES.into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: f::TIME_UNIX_NANO_CC.into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: f::COUNT.into(), value: Some(string_value(dp.count.to_string())) });
        m.push(KeyValue { key: f::METRIC_TYPE_SUM.into(), value: Some(double_value(dp.sum)) });
        let qvs: Vec<AnyValue> = dp.quantile_values.iter().map(|q| {
            kvlist_any_value(vec![
                KeyValue { key: f::QUANTILE.into(), value: Some(double_value(q.quantile)) },
                KeyValue { key: f::VALUE.into(), value: Some(double_value(q.value)) },
            ])
        }).collect();
        m.push(KeyValue { key: f::QUANTILE_VALUES.into(), value: Some(array_any_value(qvs)) });
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: f::DATA_POINTS.into(), value: Some(array_any_value(dps)) });
    kvlist_any_value(kvs)
}

fn exp_histogram_to_any_value(
    exp: &opentelemetry_proto::tonic::metrics::v1::ExponentialHistogram,
) -> AnyValue {
    let mut kvs = Vec::new();
    let dps: Vec<AnyValue> = exp.data_points.iter().map(|dp| {
        let mut m = Vec::new();
        if !dp.attributes.is_empty() {
            m.push(KeyValue { key: f::ATTRIBUTES.into(), value: Some(attrs_to_any_value(&dp.attributes)) });
        }
        if dp.time_unix_nano != 0 {
            m.push(KeyValue { key: f::TIME_UNIX_NANO_CC.into(), value: Some(string_value(dp.time_unix_nano.to_string())) });
        }
        m.push(KeyValue { key: f::COUNT.into(), value: Some(string_value(dp.count.to_string())) });
        if let Some(sum) = dp.sum {
            m.push(KeyValue { key: f::METRIC_TYPE_SUM.into(), value: Some(double_value(sum)) });
        }
        m.push(KeyValue { key: f::SCALE.into(), value: Some(int_value(dp.scale as i64)) });
        m.push(KeyValue { key: f::ZERO_COUNT.into(), value: Some(string_value(dp.zero_count.to_string())) });
        if let Some(ref pos) = dp.positive {
            m.push(KeyValue { key: f::POSITIVE.into(), value: Some(kvlist_any_value(vec![
                KeyValue { key: f::OFFSET.into(), value: Some(int_value(pos.offset as i64)) },
                KeyValue { key: f::BUCKET_COUNTS.into(), value: Some(array_any_value(
                    pos.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
                )) },
            ])) });
        }
        if let Some(ref neg) = dp.negative {
            m.push(KeyValue { key: f::NEGATIVE.into(), value: Some(kvlist_any_value(vec![
                KeyValue { key: f::OFFSET.into(), value: Some(int_value(neg.offset as i64)) },
                KeyValue { key: f::BUCKET_COUNTS.into(), value: Some(array_any_value(
                    neg.bucket_counts.iter().map(|c| string_value(c.to_string())).collect()
                )) },
            ])) });
        }
        kvlist_any_value(m)
    }).collect();
    kvs.push(KeyValue { key: f::DATA_POINTS.into(), value: Some(array_any_value(dps)) });
    kvs.push(KeyValue { key: f::AGGREGATION_TEMPORALITY.into(), value: Some(int_value(exp.aggregation_temporality as i64)) });
    kvlist_any_value(kvs)
}

// -- Trait implementations --

impl ByteSizeOf for OtelMetric {
    fn allocated_bytes(&self) -> usize {
        self.metric.encoded_len()
            + self
                .resource
                .as_ref()
                .map_or(0, |r| r.encoded_len())
            + self
                .scope
                .as_ref()
                .map_or(0, |s| s.encoded_len())
            + self.metadata.allocated_bytes()
    }
}

impl EstimatedJsonEncodedSizeOf for OtelMetric {
    fn estimated_json_encoded_size_of(&self) -> JsonSize {
        // Approximate: proto encoded_len * 3 accounts for JSON overhead
        // (field names, quoting, braces). For OtelLog/OtelSpan this should
        // closely match `as_map().estimated_json_encoded_size_of()`.
        JsonSize::new(self.metric.encoded_len() * 3)
    }
}

impl EventCount for OtelMetric {
    fn event_count(&self) -> usize {
        1
    }
}

impl Finalizable for OtelMetric {
    fn take_finalizers(&mut self) -> EventFinalizers {
        self.metadata.take_finalizers()
    }
}

// Override GetEventCountTags for OtelMetric with proper source/service extraction.
impl GetEventCountTags for OtelMetric {
    fn get_tags(&self) -> TaggedEventsSent {
        use crate::config::telemetry;
        use vector_common::internal_event::OptionalTag;

        let source = if telemetry().tags().emit_source {
            self.metadata().source_id().cloned().into()
        } else {
            OptionalTag::Ignored
        };

        let service = if telemetry().tags().emit_service {
            self.resource_attribute(f::SERVICE_NAME)
                .and_then(|av| match &av.value {
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        s,
                    )) => Some(s.clone()),
                    _ => None,
                })
                .into()
        } else {
            OptionalTag::Ignored
        };

        TaggedEventsSent { source, service }
    }
}

impl EventDataEq for OtelMetric {
    fn event_data_eq(&self, other: &Self) -> bool {
        self.metric == other.metric
            && self.dp_attrs == other.dp_attrs
            && self.resource == other.resource
            && self.resource_attrs == other.resource_attrs
            && self.scope == other.scope
            && self.scope_attrs == other.scope_attrs
    }
}

impl Serialize for OtelMetric {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        use opentelemetry_proto::tonic::metrics::v1::metric;
        use super::otel_json::*;

        let metric_with_attrs = self.metric_proto();

        let mut len = 1;
        if !metric_with_attrs.description.is_empty() { len += 1; }
        if !metric_with_attrs.unit.is_empty() { len += 1; }
        if metric_with_attrs.data.is_some() { len += 1; }
        if self.resource.is_some() { len += 1; }
        if self.scope.is_some() { len += 1; }

        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry(f::NAME, &metric_with_attrs.name)?;
        if !metric_with_attrs.description.is_empty() {
            map.serialize_entry(f::DESCRIPTION, &metric_with_attrs.description)?;
        }
        if !metric_with_attrs.unit.is_empty() {
            map.serialize_entry(f::UNIT, &metric_with_attrs.unit)?;
        }

        if let Some(ref data) = metric_with_attrs.data {
            match data {
                metric::Data::Sum(sum) => {
                    map.serialize_entry(f::METRIC_TYPE_SUM, &SerializableSum(sum))?;
                }
                metric::Data::Gauge(gauge) => {
                    map.serialize_entry(f::METRIC_TYPE_GAUGE, &SerializableGauge(gauge))?;
                }
                metric::Data::Histogram(hist) => {
                    map.serialize_entry(f::METRIC_TYPE_HISTOGRAM, &SerializableHistogram(hist))?;
                }
                metric::Data::Summary(summary) => {
                    map.serialize_entry(f::METRIC_TYPE_SUMMARY, &SerializableSummary(summary))?;
                }
                metric::Data::ExponentialHistogram(exp) => {
                    map.serialize_entry(f::EXPONENTIAL_HISTOGRAM_CC, &SerializableExpHistogram(exp))?;
                }
            }
        }

        if self.resource.is_some() || !self.resource_attrs.is_empty() {
            let mut res = self.resource.clone().unwrap_or_default();
            res.attributes = self.resource_attrs.to_key_values();
            map.serialize_entry(f::RESOURCE, &SerializableResource(&res))?;
        }
        if self.scope.is_some() || !self.scope_attrs.is_empty() {
            let mut scope = self.scope.clone().unwrap_or_default();
            scope.attributes = self.scope_attrs.to_key_values();
            map.serialize_entry(f::SCOPE, &SerializableScope(&scope))?;
        }
        map.end()
    }
}

impl std::fmt::Display for OtelMetric {
    /// Display in Prometheus-like text format:
    /// `TIMESTAMP NAMESPACE_NAME{TAGS} KIND VALUE`
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ts) = self.timestamp() {
            write!(fmt, "{ts:?} ")?;
        }
        if let Some(ns) = self.namespace() {
            write!(fmt, "{ns}_")?;
        }
        write!(fmt, "{}", self.name())?;
        write!(fmt, "{{")?;
        if let Some(tags) = self.tags() {
            let mut first = true;
            for (tag, value) in tags.iter_single() {
                if !first {
                    write!(fmt, ",")?;
                }
                first = false;
                match value {
                    Some(v) => write!(fmt, "{tag}={v:?}")?,
                    None => write!(fmt, "{tag}")?,
                }
            }
        }
        write!(fmt, "}}")?;
        let kind_char = match self.kind() {
            super::MetricKind::Absolute => '=',
            super::MetricKind::Incremental => '+',
        };
        write!(fmt, " {kind_char} {}", self.view())
    }
}

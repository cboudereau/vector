use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use metrics::counter;
use vector_lib::transform::TaskTransform;

use crate::event::Event;

use super::config::TailSamplingConfig;
use super::policies::{Decision, SamplingPolicy};

/// 16-byte OTel trace ID.
pub type TraceId = [u8; 16];

/// A buffered trace: all spans seen so far, plus bookkeeping.
pub struct BufferedTrace {
    pub trace_id: TraceId,
    pub spans: Vec<Event>,
    pub first_seen: Instant,
    pub total_bytes: usize,
}

/// Simple bounded LRU cache for trace decisions.
struct DecisionCache {
    map: HashMap<TraceId, ()>,
    order: VecDeque<TraceId>,
    capacity: usize,
}

impl DecisionCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity.min(1024)),
            order: VecDeque::with_capacity(capacity.min(1024)),
            capacity,
        }
    }

    fn contains(&self, id: &TraceId) -> bool {
        self.map.contains_key(id)
    }

    fn insert(&mut self, id: TraceId) {
        if self.map.contains_key(&id) {
            return;
        }
        if self.map.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.map.insert(id, ());
        self.order.push_back(id);
    }
}

/// The tail sampling transform.
pub struct TailSampling {
    decision_wait: Duration,
    num_traces: usize,
    max_trace_size_bytes: usize,
    traces: HashMap<TraceId, BufferedTrace>,
    insertion_order: VecDeque<TraceId>,
    sampled_cache: DecisionCache,
    dropped_cache: DecisionCache,
    policies: Vec<Box<dyn SamplingPolicy>>,
}

impl TailSampling {
    pub fn new(config: TailSamplingConfig) -> crate::Result<Self> {
        let policies: Vec<Box<dyn SamplingPolicy>> =
            config.policies.iter().map(|p| p.build()).collect();

        if policies.is_empty() {
            return Err("tail_sampling requires at least one policy".into());
        }

        Ok(Self {
            decision_wait: Duration::from_secs(config.decision_wait_secs),
            num_traces: config.num_traces,
            max_trace_size_bytes: config.max_trace_size_bytes,
            traces: HashMap::new(),
            insertion_order: VecDeque::new(),
            sampled_cache: DecisionCache::new(config.decision_cache.sampled_cache_size),
            dropped_cache: DecisionCache::new(config.decision_cache.non_sampled_cache_size),
            policies,
        })
    }

    /// Extract trace_id from an event. Returns zeroed ID for non-trace events.
    fn extract_trace_id(event: &Event) -> TraceId {
        match event {
            Event::Trace(otel_span) => {
                let bytes = &otel_span.span().trace_id;
                let mut id = [0u8; 16];
                let len = bytes.len().min(16);
                id[..len].copy_from_slice(&bytes[..len]);
                id
            }
            _ => [0u8; 16],
        }
    }

    /// Handle a single incoming span event. Returns events to emit immediately.
    fn on_span(&mut self, event: Event) -> Vec<Event> {
        let trace_id = Self::extract_trace_id(&event);

        // Check decision cache for late-arriving spans.
        if self.sampled_cache.contains(&trace_id) {
            return vec![event]; // emit immediately
        }
        if self.dropped_cache.contains(&trace_id) {
            return vec![]; // drop
        }

        let byte_size = std::mem::size_of_val(&event);

        // Insert into buffer.
        let trace = self.traces.entry(trace_id).or_insert_with(|| {
            self.insertion_order.push_back(trace_id);
            BufferedTrace {
                trace_id,
                spans: Vec::new(),
                first_seen: Instant::now(),
                total_bytes: 0,
            }
        });
        trace.total_bytes += byte_size;
        trace.spans.push(event);

        // Check per-trace size limit.
        if trace.total_bytes > self.max_trace_size_bytes {
            self.traces.remove(&trace_id);
            self.insertion_order.retain(|id| *id != trace_id);
            self.dropped_cache.insert(trace_id);
            counter!("vector_tail_sampling_trace_dropped_too_early").increment(1);
            return vec![];
        }

        // Evict oldest if over capacity.
        while self.traces.len() > self.num_traces {
            if let Some(oldest_id) = self.insertion_order.pop_front() {
                self.traces.remove(&oldest_id);
                self.dropped_cache.insert(oldest_id);
                counter!("vector_tail_sampling_trace_dropped_too_early").increment(1);
            }
        }

        vec![]
    }

    /// Tick: evaluate traces that have waited long enough.
    fn on_tick(&mut self) -> Vec<Event> {
        let now = Instant::now();
        let mut to_emit = Vec::new();
        let mut to_remove = Vec::new();

        for (&trace_id, trace) in &self.traces {
            if now.duration_since(trace.first_seen) >= self.decision_wait {
                // Evaluate policies — first match wins.
                let mut decision = Decision::Pending;
                let mut matched_policy = "";
                for policy in &self.policies {
                    let d = policy.evaluate(trace);
                    if d != Decision::Pending {
                        decision = d;
                        matched_policy = policy.name();
                        break;
                    }
                }

                match decision {
                    Decision::Sample => {
                        counter!(
                            "vector_tail_sampling_traces_sampled",
                            "policy" => matched_policy.to_string(),
                            "sampled" => "true",
                        )
                        .increment(1);
                        to_emit.extend(trace.spans.iter().cloned());
                        self.sampled_cache.insert(trace_id);
                    }
                    Decision::Drop | Decision::Pending => {
                        if decision == Decision::Drop {
                            counter!(
                                "vector_tail_sampling_traces_sampled",
                                "policy" => matched_policy.to_string(),
                                "sampled" => "false",
                            )
                            .increment(1);
                        } else {
                            counter!(
                                "vector_tail_sampling_traces_sampled",
                                "policy" => "none",
                                "sampled" => "false",
                            )
                            .increment(1);
                        }
                        self.dropped_cache.insert(trace_id);
                    }
                }
                to_remove.push(trace_id);
            }
        }

        for id in to_remove {
            self.traces.remove(&id);
            self.insertion_order.retain(|i| *i != id);
        }

        to_emit
    }
}

impl TaskTransform<Event> for TailSampling {
    fn transform(
        mut self: Box<Self>,
        input_rx: Pin<Box<dyn Stream<Item = Event> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Event> + Send>>
    where
        Self: 'static,
    {
        let mut input = input_rx.fuse();
        let tick_interval = Duration::from_secs(1);

        Box::pin(async_stream::stream! {
            let mut tick = tokio::time::interval(tick_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    maybe_event = input.next() => {
                        match maybe_event {
                            Some(event) => {
                                for e in self.on_span(event) {
                                    yield e;
                                }
                            }
                            None => {
                                // Input stream ended. Flush all pending traces
                                // (evaluate policies on whatever we have).
                                for e in self.on_tick() {
                                    yield e;
                                }
                                break;
                            }
                        }
                    }
                    _ = tick.tick() => {
                        for e in self.on_tick() {
                            yield e;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, OtelSpan};
    use opentelemetry_proto::tonic::trace::v1::{Span, Status, status::StatusCode as OtelStatusCode};
    use super::super::policies::{
        PolicyConfig, AlwaysSampleConfig, StatusCodeConfig, LatencyConfig,
        SpanCountConfig,
    };

    fn make_span(trace_id: &[u8; 16], name: &str, start_ns: u64, duration_ns: u64) -> Event {
        let span = Span {
            trace_id: trace_id.to_vec(),
            span_id: vec![0, 0, 0, 0, 0, 0, 0, 1],
            parent_span_id: vec![],
            name: name.to_string(),
            kind: 0,
            start_time_unix_nano: start_ns,
            end_time_unix_nano: start_ns + duration_ns,
            attributes: vec![],
            status: Some(Status {
                message: String::new(),
                code: OtelStatusCode::Ok as i32,
            }),
            trace_state: String::new(),
            dropped_attributes_count: 0,
            events: vec![],
            dropped_events_count: 0,
            links: vec![],
            dropped_links_count: 0,
            flags: 0,
        };
        Event::Trace(OtelSpan::new(span))
    }

    fn make_error_span(trace_id: &[u8; 16]) -> Event {
        let span = Span {
            trace_id: trace_id.to_vec(),
            span_id: vec![0, 0, 0, 0, 0, 0, 0, 2],
            parent_span_id: vec![],
            name: "error_span".to_string(),
            kind: 0,
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 2_000_000_000,
            attributes: vec![],
            status: Some(Status {
                message: "error".to_string(),
                code: OtelStatusCode::Error as i32,
            }),
            trace_state: String::new(),
            dropped_attributes_count: 0,
            events: vec![],
            dropped_events_count: 0,
            links: vec![],
            dropped_links_count: 0,
            flags: 0,
        };
        Event::Trace(OtelSpan::new(span))
    }

    #[test]
    fn extract_trace_id_from_span() {
        let id: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let event = make_span(&id, "test", 0, 100);
        assert_eq!(TailSampling::extract_trace_id(&event), id);
    }

    #[test]
    fn always_sample_policy() {
        let policy = PolicyConfig::AlwaysSample(AlwaysSampleConfig { name: "all".into() }).build();
        let trace = BufferedTrace {
            trace_id: [0; 16],
            spans: vec![make_span(&[0; 16], "test", 0, 100)],
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace), Decision::Sample);
    }

    #[test]
    fn status_code_policy_matches_error() {
        let policy = PolicyConfig::StatusCode(StatusCodeConfig {
            name: "errors".into(),
            status_codes: vec!["ERROR".into()],
        }).build();
        let id = [1u8; 16];
        let trace = BufferedTrace {
            trace_id: id,
            spans: vec![make_error_span(&id)],
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace), Decision::Sample);
    }

    #[test]
    fn status_code_policy_no_match() {
        let policy = PolicyConfig::StatusCode(StatusCodeConfig {
            name: "errors".into(),
            status_codes: vec!["ERROR".into()],
        }).build();
        let id = [2u8; 16];
        let trace = BufferedTrace {
            trace_id: id,
            spans: vec![make_span(&id, "ok", 0, 100)],
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace), Decision::Pending);
    }

    #[test]
    fn latency_policy_matches() {
        let policy = PolicyConfig::Latency(LatencyConfig {
            name: "slow".into(),
            threshold_ms: 5000,
            upper_threshold_ms: None,
        }).build();
        let id = [3u8; 16];
        let trace = BufferedTrace {
            trace_id: id,
            spans: vec![make_span(&id, "slow", 1_000_000_000, 6_000_000_000)], // 6s
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace), Decision::Sample);
    }

    #[test]
    fn latency_policy_below_threshold() {
        let policy = PolicyConfig::Latency(LatencyConfig {
            name: "slow".into(),
            threshold_ms: 5000,
            upper_threshold_ms: None,
        }).build();
        let id = [4u8; 16];
        let trace = BufferedTrace {
            trace_id: id,
            spans: vec![make_span(&id, "fast", 1_000_000_000, 100_000_000)], // 100ms
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace), Decision::Pending);
    }

    #[test]
    fn span_count_policy() {
        let policy = PolicyConfig::SpanCount(SpanCountConfig {
            name: "many".into(),
            min_spans: Some(3),
            max_spans: None,
        }).build();
        let id = [5u8; 16];

        let trace_small = BufferedTrace {
            trace_id: id,
            spans: vec![make_span(&id, "a", 0, 100)],
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace_small), Decision::Pending);

        let trace_big = BufferedTrace {
            trace_id: id,
            spans: vec![
                make_span(&id, "a", 0, 100),
                make_span(&id, "b", 0, 100),
                make_span(&id, "c", 0, 100),
            ],
            first_seen: Instant::now(),
            total_bytes: 0,
        };
        assert_eq!(policy.evaluate(&trace_big), Decision::Sample);
    }

    #[test]
    fn decision_cache_eviction() {
        let mut cache = DecisionCache::new(3);
        cache.insert([1; 16]);
        cache.insert([2; 16]);
        cache.insert([3; 16]);
        assert!(cache.contains(&[1; 16]));
        cache.insert([4; 16]); // evicts [1]
        assert!(!cache.contains(&[1; 16]));
        assert!(cache.contains(&[4; 16]));
    }

    #[test]
    fn buffer_eviction_on_capacity() {
        let config = TailSamplingConfig {
            decision_wait_secs: 9999, // won't expire during test
            num_traces: 2,
            max_trace_size_bytes: usize::MAX,
            decision_cache: Default::default(),
            policies: vec![PolicyConfig::AlwaysSample(AlwaysSampleConfig { name: "all".into() })],
        };
        let mut ts = TailSampling::new(config).unwrap();

        ts.on_span(make_span(&[1; 16], "a", 0, 100));
        ts.on_span(make_span(&[2; 16], "b", 0, 100));
        assert_eq!(ts.traces.len(), 2);

        ts.on_span(make_span(&[3; 16], "c", 0, 100)); // evicts trace [1]
        assert_eq!(ts.traces.len(), 2);
        assert!(!ts.traces.contains_key(&[1; 16]));
        assert!(ts.dropped_cache.contains(&[1; 16]));
    }

    #[test]
    fn late_span_uses_sampled_cache() {
        let config = TailSamplingConfig {
            decision_wait_secs: 0, // immediate decision
            num_traces: 1000,
            max_trace_size_bytes: usize::MAX,
            decision_cache: Default::default(),
            policies: vec![PolicyConfig::AlwaysSample(AlwaysSampleConfig { name: "all".into() })],
        };
        let mut ts = TailSampling::new(config).unwrap();
        let id = [10u8; 16];

        ts.on_span(make_span(&id, "first", 0, 100));
        let emitted = ts.on_tick(); // should sample
        assert!(!emitted.is_empty());
        assert!(ts.sampled_cache.contains(&id));

        // Late span should be emitted immediately.
        let late = ts.on_span(make_span(&id, "late", 0, 100));
        assert_eq!(late.len(), 1);
    }
}

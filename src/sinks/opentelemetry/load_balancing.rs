//! Consistent-hash load balancing for the OTel gRPC sink.
//!
//! Routes events to multiple backends via consistent hashing on traceID or service name,
//! following the OTel Collector Contrib `loadbalancingexporter` pattern.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

/// Load-balancing configuration for the OTel gRPC sink.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoadBalancingConfig {
    /// Key used to route events to backends.
    #[serde(default)]
    pub routing_key: RoutingKey,

    /// Backend resolver configuration.
    pub resolver: ResolverConfig,
}

/// Routing key strategy for load balancing.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoutingKey {
    /// Hash on span trace_id — all spans from the same trace go to the same backend.
    #[default]
    TraceID,
    /// Hash on resource service.name — all events from the same service go to the same backend.
    Service,
}

/// Resolver configuration — determines how backends are discovered.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ResolverConfig {
    /// Fixed list of backend hostnames.
    Static(StaticResolverConfig),
    /// Periodic DNS resolution of a hostname.
    Dns(DnsResolverConfig),
    /// Kubernetes EndpointSlice watcher.
    #[cfg(feature = "kubernetes")]
    K8s(K8sResolverConfig),
}

/// Static resolver: fixed list of hostnames.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StaticResolverConfig {
    pub hostnames: Vec<String>,
}

/// DNS resolver: periodic A/AAAA lookup.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DnsResolverConfig {
    pub hostname: String,
    #[serde(default = "default_dns_port")]
    pub port: u16,
    #[serde(default = "default_dns_interval")]
    pub interval: String,
}

fn default_dns_port() -> u16 {
    4317
}

fn default_dns_interval() -> String {
    "5s".to_string()
}

/// Kubernetes EndpointSlice resolver.
#[cfg(feature = "kubernetes")]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct K8sResolverConfig {
    /// Service name in format "name" or "name.namespace".
    pub service: String,
    /// Ports to use from the EndpointSlice.
    #[serde(default = "default_k8s_ports")]
    pub ports: Vec<u16>,
}

#[cfg(feature = "kubernetes")]
fn default_k8s_ports() -> Vec<u16> {
    vec![4317]
}

// ---------------------------------------------------------------------------
// Consistent Hash Ring
// ---------------------------------------------------------------------------

/// CRC32-based consistent hash ring following the OTel Collector Contrib pattern.
///
/// Ring space: 0–35999 (360 degrees × 100). Each endpoint gets `vnodes` virtual
/// nodes distributed across the ring via CRC32 hashing. Lookup is O(log n) via
/// binary search.
pub struct ConsistentHashRing {
    /// Sorted ring entries: (position, endpoint_index).
    ring: Vec<(u32, usize)>,
    /// Endpoint list (index corresponds to endpoint_index in ring).
    endpoints: Vec<String>,
}

const RING_SIZE: u32 = 36_000;
const DEFAULT_VNODES: u32 = 100;

impl ConsistentHashRing {
    /// Build a new hash ring from a list of endpoints.
    pub fn new(endpoints: &[String]) -> Self {
        Self::with_vnodes(endpoints, DEFAULT_VNODES)
    }

    /// Build a hash ring with a custom number of virtual nodes per endpoint.
    pub fn with_vnodes(endpoints: &[String], vnodes: u32) -> Self {
        let mut ring = Vec::with_capacity(endpoints.len() * vnodes as usize);
        for (idx, endpoint) in endpoints.iter().enumerate() {
            for vnode in 0..vnodes {
                let key = format!("{endpoint}-{vnode}");
                let hash = crc32_hash(key.as_bytes()) % RING_SIZE;
                ring.push((hash, idx));
            }
        }
        ring.sort_by_key(|&(pos, _)| pos);
        Self {
            ring,
            endpoints: endpoints.to_vec(),
        }
    }

    /// Look up the endpoint for a given key.
    ///
    /// Returns `None` if the ring is empty.
    pub fn get(&self, key: &[u8]) -> Option<&str> {
        if self.ring.is_empty() {
            return None;
        }
        let hash = crc32_hash(key) % RING_SIZE;
        // Binary search for the first ring entry >= hash.
        let idx = match self.ring.binary_search_by_key(&hash, |&(pos, _)| pos) {
            Ok(i) => i,
            Err(i) => {
                if i >= self.ring.len() {
                    0 // wrap around
                } else {
                    i
                }
            }
        };
        let endpoint_idx = self.ring[idx].1;
        Some(&self.endpoints[endpoint_idx])
    }

    /// Number of endpoints in the ring.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// The endpoint list.
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }
}

/// CRC32 IEEE hash.
fn crc32_hash(data: &[u8]) -> u32 {
    // Use a simple CRC32 implementation. The `crc32fast` crate is in the dep tree
    // but we use a manual IEEE table to avoid adding a direct dependency.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = CRC32_TABLE[idx] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// CRC32 IEEE lookup table.
#[rustfmt::skip]
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0xEDB8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

// ---------------------------------------------------------------------------
// Routing key extraction
// ---------------------------------------------------------------------------

use crate::event::Event;

/// Extract the routing key bytes from an event based on the routing strategy.
pub fn extract_routing_key(event: &Event, routing_key: &RoutingKey) -> Vec<u8> {
    match routing_key {
        RoutingKey::TraceID => match event {
            Event::Trace(span) => span.span().trace_id.clone(),
            Event::Log(log) => service_name_from_resource(log.resource()).into_bytes(),
            Event::Metric(metric) => service_name_from_resource(metric.resource()).into_bytes(),
        },
        RoutingKey::Service => {
            let resource = match event {
                Event::Trace(span) => span.resource(),
                Event::Log(log) => log.resource(),
                Event::Metric(metric) => metric.resource(),
            };
            service_name_from_resource(resource).into_bytes()
        }
    }
}

/// Extract `service.name` from OTel resource attributes.
fn service_name_from_resource(
    resource: Option<&opentelemetry_proto::tonic::resource::v1::Resource>,
) -> String {
    resource
        .and_then(|r| {
            r.attributes.iter().find(|kv| kv.key == "service.name").and_then(|kv| {
                kv.value.as_ref().and_then(|v| {
                    if let Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)) = &v.value {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            })
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_deterministic() {
        let endpoints = vec![
            "backend-0:4317".to_string(),
            "backend-1:4317".to_string(),
            "backend-2:4317".to_string(),
        ];
        let ring = ConsistentHashRing::new(&endpoints);

        // Same key always maps to same endpoint.
        let key = b"trace-id-abc-123";
        let ep1 = ring.get(key).unwrap();
        let ep2 = ring.get(key).unwrap();
        assert_eq!(ep1, ep2);
    }

    #[test]
    fn ring_distribution() {
        let endpoints: Vec<String> = (0..3).map(|i| format!("backend-{i}:4317")).collect();
        let ring = ConsistentHashRing::new(&endpoints);

        let mut counts = [0u32; 3];
        for i in 0..3000 {
            let key = format!("trace-{i}");
            let ep = ring.get(key.as_bytes()).unwrap();
            let idx = endpoints.iter().position(|e| e == ep).unwrap();
            counts[idx] += 1;
        }

        // Each backend should get at least 15% of traffic (expect ~33%).
        for count in &counts {
            assert!(
                *count > 450,
                "Uneven distribution: {counts:?} — one backend got only {count}/3000"
            );
        }
    }

    #[test]
    fn ring_stability_on_add() {
        let endpoints_2: Vec<String> = (0..2).map(|i| format!("backend-{i}:4317")).collect();
        let endpoints_3: Vec<String> = (0..3).map(|i| format!("backend-{i}:4317")).collect();
        let ring_2 = ConsistentHashRing::new(&endpoints_2);
        let ring_3 = ConsistentHashRing::new(&endpoints_3);

        // Adding a third backend should only move ~1/3 of keys.
        let mut moved = 0;
        let total = 3000;
        for i in 0..total {
            let key = format!("trace-{i}");
            let ep_2 = ring_2.get(key.as_bytes()).unwrap();
            let ep_3 = ring_3.get(key.as_bytes()).unwrap();
            if ep_2 != ep_3 {
                moved += 1;
            }
        }

        // At most ~50% should move (ideal is ~33%).
        assert!(
            moved < total / 2,
            "Too many keys moved: {moved}/{total} — expected ~{}/{}",
            total / 3,
            total
        );
    }

    #[test]
    fn ring_empty() {
        let ring = ConsistentHashRing::new(&[]);
        assert!(ring.get(b"any-key").is_none());
        assert!(ring.is_empty());
    }

    #[test]
    fn ring_single_endpoint() {
        let ring = ConsistentHashRing::new(&["only-one:4317".to_string()]);
        assert_eq!(ring.get(b"any-key").unwrap(), "only-one:4317");
        assert_eq!(ring.get(b"another-key").unwrap(), "only-one:4317");
    }

    #[test]
    fn crc32_known_values() {
        // Verify our CRC32 implementation produces correct IEEE values.
        assert_eq!(crc32_hash(b""), 0);
        assert_eq!(crc32_hash(b"123456789"), 0xCBF4_3926);
    }
}

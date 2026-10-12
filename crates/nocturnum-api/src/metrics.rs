//! Prometheus metrics: request counts and latencies by route, and per-collection gauges read
//! at scrape time.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{MatchedPath, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

use crate::Api;
use crate::registry::Registry;

/// Latency bucket bounds in seconds.
const BUCKETS: [f64; 12] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
];

#[derive(Default)]
struct Histogram {
    /// Counts per bucket, not cumulative, the last one for everything slower.
    buckets: [u64; BUCKETS.len() + 1],
    sum: f64,
    count: u64,
}

#[derive(Default)]
pub(crate) struct Metrics {
    /// By method, route and status.
    requests: Mutex<BTreeMap<(&'static str, String, u16), u64>>,
    /// By route.
    latency: Mutex<BTreeMap<String, Histogram>>,
}

/// A label for the method, from a fixed set so clients cannot add series.
fn method_label(m: &Method) -> &'static str {
    match *m {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::PATCH => "PATCH",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

impl Metrics {
    fn record(&self, method: &'static str, route: String, status: u16, seconds: f64) {
        let mut requests = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        *requests.entry((method, route.clone(), status)).or_default() += 1;
        drop(requests);
        let mut latency = self.latency.lock().unwrap_or_else(|e| e.into_inner());
        let h = latency.entry(route).or_default();
        let i = BUCKETS
            .iter()
            .position(|b| seconds <= *b)
            .unwrap_or(BUCKETS.len());
        h.buckets[i] += 1;
        h.sum += seconds;
        h.count += 1;
    }

    /// The metrics in Prometheus text format, reading collection gauges from `registry`.
    pub(crate) fn render(&self, registry: Option<&Registry>) -> String {
        let mut out = String::new();
        let o = &mut out;
        header(
            o,
            "nocturnum_build_info",
            "gauge",
            "Always 1, labelled with the version.",
        );
        let _ = writeln!(
            o,
            "nocturnum_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        header(
            o,
            "nocturnum_ready",
            "gauge",
            "1 once collections are loaded.",
        );
        let _ = writeln!(o, "nocturnum_ready {}", u8::from(registry.is_some()));

        header(
            o,
            "nocturnum_http_requests_total",
            "counter",
            "Requests answered, by method, route and status.",
        );
        for ((method, route, status), n) in self
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = writeln!(
                o,
                "nocturnum_http_requests_total{{method=\"{method}\",route=\"{}\",status=\"{status}\"}} {n}",
                escape(route)
            );
        }
        header(
            o,
            "nocturnum_http_request_duration_seconds",
            "histogram",
            "Time to answer a request, by route.",
        );
        for (route, h) in self
            .latency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let route = escape(route);
            let mut cumulative = 0;
            for (i, bound) in BUCKETS.iter().enumerate() {
                cumulative += h.buckets[i];
                let _ = writeln!(
                    o,
                    "nocturnum_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"{bound}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                o,
                "nocturnum_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"+Inf\"}} {}",
                h.count
            );
            let _ = writeln!(
                o,
                "nocturnum_http_request_duration_seconds_sum{{route=\"{route}\"}} {}",
                h.sum
            );
            let _ = writeln!(
                o,
                "nocturnum_http_request_duration_seconds_count{{route=\"{route}\"}} {}",
                h.count
            );
        }

        let collections = registry.map(Registry::list).unwrap_or_default();
        let gauges: [(&str, &str, &str); 7] = [
            (
                "nocturnum_collection_points",
                "gauge",
                "Points in the collection.",
            ),
            (
                "nocturnum_collection_segments",
                "gauge",
                "Segments on disk.",
            ),
            (
                "nocturnum_collection_failed",
                "gauge",
                "1 once the collection has stopped taking writes until a restart.",
            ),
            (
                "nocturnum_wal_bytes",
                "gauge",
                "Bytes the write-ahead log takes on disk.",
            ),
            (
                "nocturnum_wal_last_seq",
                "gauge",
                "Sequence number of the newest logged write.",
            ),
            (
                "nocturnum_wal_fsyncs_total",
                "counter",
                "Fsyncs of the write-ahead log.",
            ),
            (
                "nocturnum_wal_fsync_seconds_total",
                "counter",
                "Time spent in fsyncs of the write-ahead log.",
            ),
        ];
        let rows: Vec<(String, [f64; 7])> = collections
            .iter()
            .map(|(name, c)| {
                let wal = c.wal_stats();
                (
                    escape(name),
                    [
                        c.len() as f64,
                        c.segment_count() as f64,
                        f64::from(u8::from(c.failure().is_some())),
                        wal.disk_bytes as f64,
                        wal.last_seq as f64,
                        wal.syncs as f64,
                        wal.sync_time.as_secs_f64(),
                    ],
                )
            })
            .collect();
        for (i, (name, kind, help)) in gauges.iter().enumerate() {
            header(o, name, kind, help);
            for (collection, values) in &rows {
                let _ = writeln!(o, "{name}{{collection=\"{collection}\"}} {}", values[i]);
            }
        }

        let schedules = registry.map(|r| r.schedules().gauges()).unwrap_or_default();
        let secs = |t: Option<SystemTime>| {
            t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
        };
        let gauges: [(&str, &str, &str); 5] = [
            (
                "nocturnum_snapshot_running",
                "gauge",
                "1 while a scheduled snapshot of the collection runs.",
            ),
            (
                "nocturnum_snapshot_next_run_timestamp_seconds",
                "gauge",
                "When the next scheduled snapshot is due.",
            ),
            (
                "nocturnum_snapshot_last_success_timestamp_seconds",
                "gauge",
                "When the last scheduled snapshot succeeded, from the newest snapshot on disk after a restart.",
            ),
            (
                "nocturnum_snapshot_failures_total",
                "counter",
                "Scheduled snapshot runs that failed, since the server started.",
            ),
            (
                "nocturnum_snapshot_damaged",
                "gauge",
                "Kept snapshots that failed their checksum at the last scheduled run.",
            ),
        ];
        let rows: Vec<(String, [Option<f64>; 5])> = schedules
            .iter()
            .map(|(name, g)| {
                (
                    escape(name),
                    [
                        Some(f64::from(u8::from(g.running))),
                        secs(g.next_run),
                        secs(g.last_success),
                        Some(g.failures as f64),
                        Some(g.damaged as f64),
                    ],
                )
            })
            .collect();
        for (i, (name, kind, help)) in gauges.iter().enumerate() {
            header(o, name, kind, help);
            for (collection, values) in &rows {
                if let Some(v) = values[i] {
                    let _ = writeln!(o, "{name}{{collection=\"{collection}\"}} {v}");
                }
            }
        }
        out
    }
}

fn header(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {name} {help}");
    let _ = writeln!(o, "# TYPE {name} {kind}");
}

fn escape(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The route a request matched, copied into the response so [`track`], which runs before
/// routing, can label it.
#[derive(Clone)]
pub(crate) struct Route(pub(crate) String);

/// Route-level middleware that passes the matched route out to [`track`].
pub(crate) async fn tag(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| Route(p.as_str().to_owned()));
    let mut response = next.run(req).await;
    if let Some(r) = route {
        response.extensions_mut().insert(r);
    }
    response
}

/// Outer middleware that counts and times every request.
pub(crate) async fn track(State(api): State<Arc<Api>>, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = method_label(req.method());
    let response = next.run(req).await;
    let route = response
        .extensions()
        .get::<Route>()
        .map_or_else(|| "unmatched".to_owned(), |r| r.0.clone());
    api.metrics.record(
        method,
        route,
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counts_and_cumulative_buckets() {
        let m = Metrics::default();
        m.record("GET", "/v1".into(), 200, 0.0005);
        m.record("GET", "/v1".into(), 200, 0.3);
        m.record("POST", "/v1".into(), 404, 30.0);
        let text = m.render(None);
        assert!(text.contains(
            "nocturnum_http_requests_total{method=\"GET\",route=\"/v1\",status=\"200\"} 2"
        ));
        assert!(text.contains(
            "nocturnum_http_request_duration_seconds_bucket{route=\"/v1\",le=\"0.001\"} 1"
        ));
        assert!(text.contains(
            "nocturnum_http_request_duration_seconds_bucket{route=\"/v1\",le=\"0.5\"} 2"
        ));
        assert!(
            text.contains(
                "nocturnum_http_request_duration_seconds_bucket{route=\"/v1\",le=\"10\"} 2"
            )
        );
        assert!(text.contains(
            "nocturnum_http_request_duration_seconds_bucket{route=\"/v1\",le=\"+Inf\"} 3"
        ));
        assert!(text.contains("nocturnum_ready 0"));
    }

    #[test]
    fn escapes_labels() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}

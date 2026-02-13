//! Observability metrics for the event bus
//!
//! Lightweight, lock-free counters and timing for publish, subscribe,
//! and error operations. No external metrics crate dependency — designed
//! to be scraped by any monitoring system.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Event bus metrics — lock-free atomic counters
///
/// Tracks publish, subscribe, and error counts plus publish latency.
/// All operations are `Relaxed` ordering for maximum throughput.
///
/// Use `EventBus::metrics()` to access, or scrape periodically.
pub struct EventMetrics {
    /// Total events published successfully
    pub publish_count: AtomicU64,

    /// Total publish failures
    pub publish_errors: AtomicU64,

    /// Total subscription operations (create/update)
    pub subscribe_count: AtomicU64,

    /// Total subscription removals
    pub unsubscribe_count: AtomicU64,

    /// Total events routed to DLQ
    pub dlq_count: AtomicU64,

    /// Total schema validation failures
    pub validation_errors: AtomicU64,

    /// Total encryption operations
    pub encrypt_count: AtomicU64,

    /// Total decryption operations
    pub decrypt_count: AtomicU64,

    /// Cumulative publish latency in microseconds (divide by publish_count for avg)
    pub publish_latency_us: AtomicU64,

    /// Maximum publish latency in microseconds
    pub publish_max_latency_us: AtomicU64,
}

impl EventMetrics {
    /// Create a new metrics instance with all counters at zero
    pub fn new() -> Self {
        Self {
            publish_count: AtomicU64::new(0),
            publish_errors: AtomicU64::new(0),
            subscribe_count: AtomicU64::new(0),
            unsubscribe_count: AtomicU64::new(0),
            dlq_count: AtomicU64::new(0),
            validation_errors: AtomicU64::new(0),
            encrypt_count: AtomicU64::new(0),
            decrypt_count: AtomicU64::new(0),
            publish_latency_us: AtomicU64::new(0),
            publish_max_latency_us: AtomicU64::new(0),
        }
    }

    /// Record a successful publish with latency
    pub fn record_publish(&self, start: Instant) {
        self.publish_count.fetch_add(1, Ordering::Relaxed);
        let latency = start.elapsed().as_micros() as u64;
        self.publish_latency_us.fetch_add(latency, Ordering::Relaxed);
        self.update_max_latency(latency);
    }

    /// Record a publish failure
    pub fn record_publish_error(&self) {
        self.publish_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a subscription operation
    pub fn record_subscribe(&self) {
        self.subscribe_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an unsubscribe operation
    pub fn record_unsubscribe(&self) {
        self.unsubscribe_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a DLQ event
    pub fn record_dlq(&self) {
        self.dlq_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a validation error
    pub fn record_validation_error(&self) {
        self.validation_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an encryption operation
    pub fn record_encrypt(&self) {
        self.encrypt_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a decryption operation
    pub fn record_decrypt(&self) {
        self.decrypt_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a snapshot of all metrics
    pub fn snapshot(&self) -> MetricsSnapshot {
        let publish_count = self.publish_count.load(Ordering::Relaxed);
        let total_latency = self.publish_latency_us.load(Ordering::Relaxed);
        let avg_latency_us = if publish_count > 0 {
            total_latency / publish_count
        } else {
            0
        };

        MetricsSnapshot {
            publish_count,
            publish_errors: self.publish_errors.load(Ordering::Relaxed),
            subscribe_count: self.subscribe_count.load(Ordering::Relaxed),
            unsubscribe_count: self.unsubscribe_count.load(Ordering::Relaxed),
            dlq_count: self.dlq_count.load(Ordering::Relaxed),
            validation_errors: self.validation_errors.load(Ordering::Relaxed),
            encrypt_count: self.encrypt_count.load(Ordering::Relaxed),
            decrypt_count: self.decrypt_count.load(Ordering::Relaxed),
            avg_publish_latency_us: avg_latency_us,
            max_publish_latency_us: self.publish_max_latency_us.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters to zero
    pub fn reset(&self) {
        self.publish_count.store(0, Ordering::Relaxed);
        self.publish_errors.store(0, Ordering::Relaxed);
        self.subscribe_count.store(0, Ordering::Relaxed);
        self.unsubscribe_count.store(0, Ordering::Relaxed);
        self.dlq_count.store(0, Ordering::Relaxed);
        self.validation_errors.store(0, Ordering::Relaxed);
        self.encrypt_count.store(0, Ordering::Relaxed);
        self.decrypt_count.store(0, Ordering::Relaxed);
        self.publish_latency_us.store(0, Ordering::Relaxed);
        self.publish_max_latency_us.store(0, Ordering::Relaxed);
    }

    /// Update max latency (lock-free CAS loop)
    fn update_max_latency(&self, latency: u64) {
        let mut current = self.publish_max_latency_us.load(Ordering::Relaxed);
        while latency > current {
            match self.publish_max_latency_us.compare_exchange_weak(
                current,
                latency,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

impl Default for EventMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time snapshot of all metrics
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSnapshot {
    pub publish_count: u64,
    pub publish_errors: u64,
    pub subscribe_count: u64,
    pub unsubscribe_count: u64,
    pub dlq_count: u64,
    pub validation_errors: u64,
    pub encrypt_count: u64,
    pub decrypt_count: u64,
    pub avg_publish_latency_us: u64,
    pub max_publish_latency_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_default_zero() {
        let m = EventMetrics::new();
        let s = m.snapshot();
        assert_eq!(s.publish_count, 0);
        assert_eq!(s.publish_errors, 0);
        assert_eq!(s.subscribe_count, 0);
        assert_eq!(s.unsubscribe_count, 0);
        assert_eq!(s.dlq_count, 0);
        assert_eq!(s.validation_errors, 0);
        assert_eq!(s.encrypt_count, 0);
        assert_eq!(s.decrypt_count, 0);
        assert_eq!(s.avg_publish_latency_us, 0);
        assert_eq!(s.max_publish_latency_us, 0);
    }

    #[test]
    fn test_record_publish() {
        let m = EventMetrics::new();
        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_micros(100));
        m.record_publish(start);

        let s = m.snapshot();
        assert_eq!(s.publish_count, 1);
        assert!(s.avg_publish_latency_us >= 50); // at least some latency
        assert!(s.max_publish_latency_us >= 50);
    }

    #[test]
    fn test_record_errors() {
        let m = EventMetrics::new();
        m.record_publish_error();
        m.record_publish_error();
        m.record_validation_error();

        let s = m.snapshot();
        assert_eq!(s.publish_errors, 2);
        assert_eq!(s.validation_errors, 1);
    }

    #[test]
    fn test_record_subscribe_unsubscribe() {
        let m = EventMetrics::new();
        m.record_subscribe();
        m.record_subscribe();
        m.record_unsubscribe();

        let s = m.snapshot();
        assert_eq!(s.subscribe_count, 2);
        assert_eq!(s.unsubscribe_count, 1);
    }

    #[test]
    fn test_record_dlq() {
        let m = EventMetrics::new();
        m.record_dlq();
        assert_eq!(m.snapshot().dlq_count, 1);
    }

    #[test]
    fn test_record_encrypt_decrypt() {
        let m = EventMetrics::new();
        m.record_encrypt();
        m.record_encrypt();
        m.record_decrypt();

        let s = m.snapshot();
        assert_eq!(s.encrypt_count, 2);
        assert_eq!(s.decrypt_count, 1);
    }

    #[test]
    fn test_max_latency_tracking() {
        let m = EventMetrics::new();

        // Simulate two publishes with different latencies
        let start1 = Instant::now();
        std::thread::sleep(std::time::Duration::from_micros(100));
        m.record_publish(start1);

        let start2 = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        m.record_publish(start2);

        let s = m.snapshot();
        assert_eq!(s.publish_count, 2);
        // Max should be from the second (longer) publish
        assert!(s.max_publish_latency_us >= 1000);
    }

    #[test]
    fn test_reset() {
        let m = EventMetrics::new();
        m.record_publish(Instant::now());
        m.record_publish_error();
        m.record_subscribe();
        m.record_dlq();

        m.reset();
        let s = m.snapshot();
        assert_eq!(s.publish_count, 0);
        assert_eq!(s.publish_errors, 0);
        assert_eq!(s.subscribe_count, 0);
        assert_eq!(s.dlq_count, 0);
    }

    #[test]
    fn test_snapshot_serializable() {
        let m = EventMetrics::new();
        m.record_publish(Instant::now());
        let s = m.snapshot();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("publishCount"));
        assert!(json.contains("avgPublishLatencyUs"));
    }

    #[test]
    fn test_concurrent_metrics() {
        use std::sync::Arc;

        let m = Arc::new(EventMetrics::new());
        let mut handles = Vec::new();

        for _ in 0..10 {
            let m = m.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    m.record_publish(Instant::now());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.snapshot().publish_count, 1000);
    }
}

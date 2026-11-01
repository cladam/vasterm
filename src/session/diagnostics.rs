use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Fixed-size, content-free measurements. Worker fields are sampled independently.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionDiagnostics {
    pub polls: u64,
    pub processed_bytes: u64,
    pub last_update_bytes: usize,
    pub max_update_bytes: usize,
    pub last_poll_duration: Duration,
    pub max_poll_duration: Duration,
    pub max_chunk_duration: Duration,
    pub budget_followups: u64,
    pub worker_wakeups: u64,
    pub output_backpressure_retries: u64,
    pub delivered_inputs: u64,
    pub last_input_latency: Duration,
    pub max_input_latency: Duration,
    pub last_input_delivered_at: Duration,
    pub elapsed: Duration,
}

#[derive(Default)]
pub(super) struct WorkerDiagnostics {
    pub wakeups: AtomicU64,
    pub backpressure_retries: AtomicU64,
    pub delivered_inputs: AtomicU64,
    pub last_input_latency_ns: AtomicU64,
    pub max_input_latency_ns: AtomicU64,
    pub last_input_delivered_at_ns: AtomicU64,
}

pub(super) fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

pub(super) fn nanoseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl WorkerDiagnostics {
    pub fn snapshot(&self, mut stats: SessionDiagnostics) -> SessionDiagnostics {
        stats.worker_wakeups = self.wakeups.load(Ordering::Relaxed);
        stats.output_backpressure_retries = self.backpressure_retries.load(Ordering::Relaxed);
        stats.delivered_inputs = self.delivered_inputs.load(Ordering::Relaxed);
        stats.last_input_latency =
            Duration::from_nanos(self.last_input_latency_ns.load(Ordering::Relaxed));
        stats.max_input_latency =
            Duration::from_nanos(self.max_input_latency_ns.load(Ordering::Relaxed));
        stats.last_input_delivered_at =
            Duration::from_nanos(self.last_input_delivered_at_ns.load(Ordering::Relaxed));
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_duration_conversion_saturate() {
        let counter = AtomicU64::new(u64::MAX);
        increment(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(nanoseconds(Duration::MAX), u64::MAX);
    }
}

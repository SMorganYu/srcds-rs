//! Lightweight in-process metrics: counters plus a bounded ring of recent tick
//! durations for p50/p95/p99.

use std::time::Duration;

const TICK_SAMPLES: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub packets_in: u64,
    pub bytes_in: u64,
    pub packets_out: u64,
    pub bytes_out: u64,
    pub malformed: u64,
    pub rate_limited: u64,
    pub challenges_issued: u64,
    pub bad_challenges: u64,
    pub queries_info: u64,
    pub queries_player: u64,
    pub queries_rules: u64,
    pub unhandled_connectionless: u64,
    pub netchannel_dropped: u64,
    pub split_dropped: u64,
    pub send_errors: u64,
    pub ticks: u64,
    pub tick_overruns: u64,
}

#[derive(Debug)]
pub struct Metrics {
    pub counters: Counters,
    tick_ns: Vec<u64>,
    next: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricsSnapshot {
    pub counters: Counters,
    pub tick_p50: Duration,
    pub tick_p95: Duration,
    pub tick_p99: Duration,
    pub tick_max: Duration,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            counters: Counters::default(),
            tick_ns: Vec::with_capacity(TICK_SAMPLES),
            next: 0,
        }
    }
}

impl Metrics {
    pub fn record_tick(&mut self, d: Duration) {
        let ns = d.as_nanos().min(u64::MAX as u128) as u64;
        if self.tick_ns.len() < TICK_SAMPLES {
            self.tick_ns.push(ns);
        } else {
            self.tick_ns[self.next] = ns;
        }
        self.next = (self.next + 1) % TICK_SAMPLES;
        self.counters.ticks += 1;
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let mut v = self.tick_ns.clone();
        v.sort_unstable();
        let pct = |p: f64| -> Duration {
            if v.is_empty() {
                return Duration::ZERO;
            }
            let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
            Duration::from_nanos(v[idx])
        };
        MetricsSnapshot {
            counters: self.counters.clone(),
            tick_p50: pct(0.50),
            tick_p95: pct(0.95),
            tick_p99: pct(0.99),
            tick_max: v
                .last()
                .map_or(Duration::ZERO, |&n| Duration::from_nanos(n)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let mut m = Metrics::default();
        for i in 1..=100u64 {
            m.record_tick(Duration::from_micros(i));
        }
        let s = m.snapshot();
        assert_eq!(s.counters.ticks, 100);
        assert_eq!(s.tick_p50, Duration::from_micros(51));
        assert_eq!(s.tick_p99, Duration::from_micros(99));
        assert_eq!(s.tick_max, Duration::from_micros(100));
    }

    #[test]
    fn ring_is_bounded() {
        let mut m = Metrics::default();
        for _ in 0..(TICK_SAMPLES * 2 + 5) {
            m.record_tick(Duration::from_micros(1));
        }
        assert_eq!(m.tick_ns.len(), TICK_SAMPLES);
    }
}

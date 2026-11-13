//! Rate limiting for unauthenticated (connectionless) traffic.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Classic token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: u32, now: Instant) -> Self {
        Self {
            capacity: capacity as f64,
            tokens: capacity as f64,
            refill_per_sec: refill_per_sec as f64,
            last: now,
        }
    }

    pub fn try_take(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-source-IP bucket with a hard cap on tracked addresses, so spoofed floods
/// cannot grow memory without bound. When the table is full, stale entries are
/// pruned; if still full, unknown addresses are refused (fail closed).
#[derive(Debug)]
pub struct PerIpLimiter {
    buckets: HashMap<IpAddr, TokenBucket>,
    max_entries: usize,
    capacity: u32,
    refill_per_sec: u32,
    idle_expiry: Duration,
}

impl PerIpLimiter {
    pub fn new(max_entries: usize, capacity: u32, refill_per_sec: u32) -> Self {
        Self {
            buckets: HashMap::new(),
            max_entries,
            capacity,
            refill_per_sec,
            idle_expiry: Duration::from_secs(10),
        }
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if !self.buckets.contains_key(&ip) && self.buckets.len() >= self.max_entries {
            let expiry = self.idle_expiry;
            self.buckets
                .retain(|_, b| now.saturating_duration_since(b.last) < expiry);
            if self.buckets.len() >= self.max_entries {
                return false;
            }
        }
        let (cap, rate) = (self.capacity, self.refill_per_sec);
        self.buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new(cap, rate, now))
            .try_take(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills() {
        let t = Instant::now();
        let mut b = TokenBucket::new(2, 1, t);
        assert!(b.try_take(t));
        assert!(b.try_take(t));
        assert!(!b.try_take(t));
        assert!(b.try_take(t + Duration::from_millis(1001)));
    }

    #[test]
    fn per_ip_is_bounded() {
        let t = Instant::now();
        let mut l = PerIpLimiter::new(2, 1, 1);
        assert!(l.allow("1.1.1.1".parse().unwrap(), t));
        assert!(l.allow("1.1.1.2".parse().unwrap(), t));
        assert!(!l.allow("1.1.1.3".parse().unwrap(), t), "table full");
        assert_eq!(l.len(), 2);
        // After idle expiry the table is pruned and new sources admitted.
        assert!(l.allow("1.1.1.3".parse().unwrap(), t + Duration::from_secs(11)));
        assert!(!l.allow("1.1.1.3".parse().unwrap(), t + Duration::from_secs(11)));
    }
}

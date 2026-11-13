//! Stateless query challenges.
//!
//! A2S queries require the client to echo a server-issued 32-bit challenge (anti
//! reflection/amplification). Instead of storing one entry per address (unbounded
//! memory under spoofed floods) the challenge is a keyed hash of the client address
//! and a coarse time epoch. A challenge is valid for the current and previous epoch.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ChallengeIssuer {
    secret: [u64; 4],
    epoch_len: Duration,
    origin: Instant,
}

impl ChallengeIssuer {
    pub fn new(epoch_len: Duration) -> Self {
        Self::with_secret(rand::random(), epoch_len)
    }

    pub fn with_secret(secret: [u64; 4], epoch_len: Duration) -> Self {
        assert!(!epoch_len.is_zero(), "epoch length must be non-zero");
        Self {
            secret,
            epoch_len,
            origin: Instant::now(),
        }
    }

    fn epoch(&self, now: Instant) -> u64 {
        let d = now.saturating_duration_since(self.origin);
        (d.as_nanos() / self.epoch_len.as_nanos()) as u64
    }

    fn compute(&self, addr: &SocketAddr, epoch: u64) -> i32 {
        // SipHash (DefaultHasher) keyed by mixing a 256-bit secret into the state
        // before the message. Not a formal MAC construction, but unpredictable to
        // remote parties that cannot observe the secret, which is the requirement.
        let mut h = DefaultHasher::new();
        self.secret.hash(&mut h);
        addr.ip().hash(&mut h);
        epoch.hash(&mut h);
        let v = h.finish() as u32 as i32;
        // -1 is the "send me a challenge" sentinel and 0 is avoided for clarity.
        match v {
            -1 | 0 => 0x5352_4344, // "SRCD"
            v => v,
        }
    }

    pub fn issue(&self, addr: &SocketAddr, now: Instant) -> i32 {
        self.compute(addr, self.epoch(now))
    }

    pub fn verify(&self, addr: &SocketAddr, challenge: i32, now: Instant) -> bool {
        let e = self.epoch(now);
        challenge == self.compute(addr, e) || (e > 0 && challenge == self.compute(addr, e - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn issue_verify_and_expiry() {
        let c = ChallengeIssuer::with_secret([1, 2, 3, 4], Duration::from_secs(30));
        let t0 = c.origin;
        let a = addr("127.0.0.1:5000");
        let ch = c.issue(&a, t0);
        assert_ne!(ch, -1);
        assert!(c.verify(&a, ch, t0));
        assert!(
            c.verify(&a, ch, t0 + Duration::from_secs(45)),
            "previous epoch ok"
        );
        assert!(!c.verify(&a, ch, t0 + Duration::from_secs(61)), "expired");
        assert!(!c.verify(&addr("127.0.0.2:5000"), ch, t0), "bound to ip");
        assert!(!c.verify(&a, ch.wrapping_add(1), t0));
    }

    #[test]
    fn different_secrets_differ() {
        let a = addr("10.0.0.1:1");
        let c1 = ChallengeIssuer::with_secret([1, 0, 0, 0], Duration::from_secs(30));
        let c2 = ChallengeIssuer::with_secret([2, 0, 0, 0], Duration::from_secs(30));
        assert_ne!(c1.compute(&a, 0), c2.compute(&a, 0));
    }
}

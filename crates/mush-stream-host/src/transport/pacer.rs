//! Send-side token-bucket pacing.
//!
//! NVENC produces an entire frame's worth of packets in a single burst —
//! at 60fps a 4 Mb keyframe lands as ~3 200 packets in microseconds. Without
//! shaping, the OS sendmmsg flood arrives at the receiver in a tight burst
//! that often outruns its NIC ring buffer or path queue, adding queueing
//! latency or causing tail drops.
//!
//! [`TokenBucket`] smooths the send rate to a target bytes/sec. Capacity
//! lets the pacer absorb short encoder bursts without suspending the send
//! loop on every frame.

use std::time::{Duration, Instant};

/// A simple token bucket sized in bytes.
///
/// Refills continuously at `refill_rate_bps` bytes per second, capped at
/// `capacity_bytes`. [`Self::take`] awaits if the bucket lacks the
/// requested tokens.
#[derive(Debug)]
pub struct TokenBucket {
    capacity_bytes: f64,
    refill_rate_bps: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity_bytes: u64, refill_rate_bps: u64) -> Self {
        Self {
            capacity_bytes: capacity_bytes as f64,
            refill_rate_bps: refill_rate_bps as f64,
            // Start full so the first burst doesn't immediately sleep.
            tokens: capacity_bytes as f64,
            last_refill: Instant::now(),
        }
    }

    /// Take `bytes` tokens, awaiting if necessary. Returns when the bucket
    /// has been debited.
    pub async fn take(&mut self, bytes: u64) {
        let bytes = bytes as f64;
        self.refill();
        if self.tokens < bytes {
            // Sleep just long enough to accumulate the deficit.
            let deficit = bytes - self.tokens;
            let wait_secs = deficit / self.refill_rate_bps;
            let dur = Duration::try_from_secs_f64(wait_secs).unwrap_or_default();
            tokio::time::sleep(dur).await;
            self.refill();
        }
        // Even after sleep+refill, floating-point edges can leave tokens
        // marginally below `bytes`; clamp to never go below 0.
        self.tokens = (self.tokens - bytes).max(0.0);
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate_bps).min(self.capacity_bytes);
        self.last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn drains_immediately_when_under_capacity() {
        // 1 MB/s, 100 KB capacity. A 50 KB burst should not sleep.
        let mut bucket = TokenBucket::new(100_000, 1_000_000);
        let start = tokio::time::Instant::now();
        bucket.take(50_000).await;
        assert!(start.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn sleeps_proportionally_when_drained() {
        // 1 MB/s. Drain capacity, then take 250 KB more — should sleep
        // ~0.25s under the paused tokio clock.
        let mut bucket = TokenBucket::new(100_000, 1_000_000);
        bucket.take(100_000).await; // drain
        let start = tokio::time::Instant::now();
        bucket.take(250_000).await;
        let elapsed = start.elapsed();
        // Under start_paused, sleep advances the clock deterministically.
        assert!(
            elapsed >= Duration::from_millis(245),
            "expected at least 245ms, got {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(260),
            "expected at most 260ms, got {elapsed:?}"
        );
    }
}

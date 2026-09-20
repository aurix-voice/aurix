use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Upper bound on distinct rate-limit keys kept in memory.
pub const MAX_BUCKETS: usize = 500_000;

#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<DashMap<String, TokenBucket>>,
    default_rate: u32,
    default_burst: u32,
    check_counter: Arc<AtomicU64>,
}

struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: u32, burst: u32) -> Self {
        Self::with_f64(rate as f64, burst as f64)
    }

    fn with_f64(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            max_tokens: burst,
            refill_rate: rate,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self, count: f64) -> bool {
        self.try_consume_or_wait(count).is_ok()
    }

    /// Consumes `count` tokens, or reports how long until they will be available.
    fn try_consume_or_wait(&mut self, count: f64) -> std::result::Result<(), Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= count {
            self.tokens -= count;
            Ok(())
        } else if self.refill_rate > 0.0 {
            Err(Duration::from_secs_f64(
                (count - self.tokens) / self.refill_rate,
            ))
        } else {
            Err(Duration::MAX)
        }
    }
}

impl RateLimiter {
    pub fn new(rate: u32, burst: u32) -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
            default_rate: rate,
            default_burst: burst,
            check_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Must be called once inside a tokio runtime to start background cleanup.
    pub fn start_cleanup_task(&self) {
        let buckets = self.buckets.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                buckets.retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(300));
            }
        });
    }

    pub fn check(&self, key: &str) -> bool {
        self.check_with_cost(key, 1.0)
    }

    pub fn check_with_cost(&self, key: &str, cost: f64) -> bool {
        // Inline probabilistic cleanup if start_cleanup_task was never called
        let count = self.check_counter.fetch_add(1, Ordering::Relaxed);
        if count.is_multiple_of(10_000) {
            self.buckets
                .retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(300));
        }

        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(key) {
            // Under a key-flood, evict stale buckets before admitting a new key; if that does
            // not help, fail closed for the new key rather than growing without bound.
            self.buckets
                .retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(60));
            if self.buckets.len() >= MAX_BUCKETS {
                return false;
            }
        }
        let mut entry = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(self.default_rate, self.default_burst));
        entry.value_mut().try_consume(cost)
    }

    /// Token-bucket check with per-key limits. If a bucket already exists for `key` with
    /// different parameters it is re-parameterised in place (tokens are clamped to the new burst).
    pub fn check_custom(&self, key: &str, rate: u32, burst: u32) -> bool {
        self.try_acquire(key, rate as f64, burst as f64, 1.0)
            .is_ok()
    }

    /// Takes `cost` tokens from `key`'s bucket of `rate` tokens/s and capacity `burst`, or
    /// returns how long the caller should wait. Re-parameterises an existing bucket in place
    /// (tokens are clamped to the new burst). Bounded like [`Self::check_with_cost`].
    pub fn try_acquire(
        &self,
        key: &str,
        rate: f64,
        burst: f64,
        cost: f64,
    ) -> std::result::Result<(), Duration> {
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(key) {
            self.buckets
                .retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(60));
            if self.buckets.len() >= MAX_BUCKETS {
                return Err(Duration::from_secs(1));
            }
        }
        let mut entry = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::with_f64(rate, burst));
        let bucket = entry.value_mut();
        if bucket.refill_rate != rate || bucket.max_tokens != burst {
            bucket.refill_rate = rate;
            bucket.max_tokens = burst;
            bucket.tokens = bucket.tokens.min(bucket.max_tokens);
        }
        bucket.try_consume_or_wait(cost)
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_enforces_burst_and_custom_reparam() {
        let rl = RateLimiter::new(1, 2);
        assert!(rl.check("k"));
        assert!(rl.check("k"));
        assert!(!rl.check("k"));
        // custom limits on a different key
        assert!(rl.check_custom("c", 10, 1));
        assert!(!rl.check_custom("c", 10, 1));
        // re-parameterising to a larger burst does not grant tokens retroactively
        assert!(!rl.check_custom("c", 10, 5));
    }

    #[test]
    fn try_acquire_reports_the_wait() {
        let rl = RateLimiter::new(1, 1);
        assert!(rl.try_acquire("w", 2.0, 1.0, 1.0).is_ok());
        let wait = rl.try_acquire("w", 2.0, 1.0, 1.0).unwrap_err();
        assert!(wait > Duration::from_millis(400) && wait <= Duration::from_millis(500));
        // a zero-rate bucket never refills
        assert!(rl.try_acquire("z", 0.0, 1.0, 1.0).is_ok());
        assert_eq!(
            rl.try_acquire("z", 0.0, 1.0, 1.0).unwrap_err(),
            Duration::MAX
        );
    }
}

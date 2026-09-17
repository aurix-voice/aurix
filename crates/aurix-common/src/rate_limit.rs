use dashmap::DashMap;
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
        Self {
            tokens: burst as f64,
            max_tokens: burst as f64,
            refill_rate: rate as f64,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self, count: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= count {
            self.tokens -= count;
            true
        } else {
            false
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
                buckets.retain(|_, bucket| {
                    bucket.last_refill.elapsed() < Duration::from_secs(300)
                });
            }
        });
    }

    pub fn check(&self, key: &str) -> bool {
        self.check_with_cost(key, 1.0)
    }

    pub fn check_with_cost(&self, key: &str, cost: f64) -> bool {
        // Inline probabilistic cleanup if start_cleanup_task was never called
        let count = self.check_counter.fetch_add(1, Ordering::Relaxed);
        if count % 10_000 == 0 {
            self.buckets.retain(|_, bucket| {
                bucket.last_refill.elapsed() < Duration::from_secs(300)
            });
        }

        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(key) {
            // Under a key-flood, evict stale buckets before admitting a new key; if that does
            // not help, fail closed for the new key rather than growing without bound.
            self.buckets.retain(|_, bucket| bucket.last_refill.elapsed() < Duration::from_secs(60));
            if self.buckets.len() >= MAX_BUCKETS {
                return false;
            }
        }
        let mut entry = self.buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(self.default_rate, self.default_burst));
        entry.value_mut().try_consume(cost)
    }

    /// Token-bucket check with per-key limits. If a bucket already exists for `key` with
    /// different parameters it is re-parameterised in place (tokens are clamped to the new burst).
    pub fn check_custom(&self, key: &str, rate: u32, burst: u32) -> bool {
        let mut entry = self.buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(rate, burst));
        let bucket = entry.value_mut();
        if bucket.refill_rate != rate as f64 || bucket.max_tokens != burst as f64 {
            bucket.refill_rate = rate as f64;
            bucket.max_tokens = burst as f64;
            bucket.tokens = bucket.tokens.min(bucket.max_tokens);
        }
        bucket.try_consume(1.0)
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
}

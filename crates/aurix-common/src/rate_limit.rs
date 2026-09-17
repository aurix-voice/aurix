use dashmap::DashMap;
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

        let mut entry = self.buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(self.default_rate, self.default_burst));
        entry.value_mut().try_consume(cost)
    }

    pub fn check_custom(&self, key: &str, rate: u32, burst: u32) -> bool {
        let mut entry = self.buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(rate, burst));
        entry.value_mut().try_consume(1.0)
    }
}
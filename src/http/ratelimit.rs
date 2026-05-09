// src/http/ratelimit.rs
//
// In-memory per-IP token-bucket rate limiter.
//
// Each remote IP has a virtual bucket of `capacity` tokens that
// refills continuously at `per_minute / 60` tokens per second. Each
// allowed request consumes one token; if the bucket is empty when a
// request arrives, that request is rejected with 429.
//
// Why hand-rolled and not a crate?
//   - The whole implementation is ~30 lines.
//   - We don't need distributed limits, tracing integration, or the
//     other features that come with `tower-governor` etc.
//   - Hand-rolled = visible in the call site = easier to audit for
//     a custodial service.
//
// Caveats / known limits:
//   - The `HashMap` grows monotonically with distinct IPs ever seen.
//     A long-running publicly-exposed instance should add a periodic
//     pruner that drops idle entries. For LAN/single-tenant use the
//     map will stay small.
//   - Per-IP only — an attacker rotating IPs (Tor, residential proxy
//     pool) bypasses this. We could add per-login backoff for /auth
//     in a future slice.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Token-bucket rate limiter, keyed by remote IP.
pub struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    capacity: f64,
    refill_per_second: f64,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// `capacity` is the burst size (max tokens an IP can have at
    /// once). `per_minute` is the steady-state refill rate.
    ///
    /// e.g. `RateLimiter::new(5, 5)` = "5 requests in a burst, then
    /// 1 request every 12 seconds long-term".
    pub fn new(capacity: u32, per_minute: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity: capacity as f64,
            refill_per_second: per_minute as f64 / 60.0,
        }
    }

    /// Try to consume one token from `ip`'s bucket. Returns `true`
    /// on success, `false` when the bucket is empty.
    ///
    /// === Rust note: `Mutex::lock().unwrap()` ===
    ///
    /// `Mutex::lock` returns `Result<MutexGuard, PoisonError>`.
    /// "Poisoning" is Rust's safety mechanism: if a thread panicked
    /// while holding the lock, subsequent locks return an error so
    /// you can decide whether the data is still consistent. For us,
    /// the only thing inside the lock is `HashMap` integrity —
    /// nothing dangerous happens on panic — so unwrapping is fine.
    pub fn try_acquire(&self, ip: IpAddr) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();

        // Insert a full bucket on first sight of this IP.
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.capacity,
            last_refill: now,
        });

        // Refill since last touch, capped at capacity.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_second).min(self.capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

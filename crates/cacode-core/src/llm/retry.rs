//! Exponential backoff with jitter and a per-provider circuit breaker.

use super::LlmError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base: Duration,
    pub max_delay: Duration,
    /// Cap on honoured `Retry-After` values.
    pub max_retry_after: Duration,
    /// How long a provider stays "open" (skipped) after exhausting retries.
    pub cooldown: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
            max_retry_after: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
        }
    }
}

fn jitter_factor() -> f64 {
    // xorshift seeded from the clock: good enough for jitter, no rand crate.
    let mut x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64 ^ d.as_secs()).unwrap_or(42) | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    0.75 + (x % 1000) as f64 / 2000.0 // 0.75 .. 1.25
}

impl RetryPolicy {
    /// Delay before attempt `attempt + 1` (attempt is 1-based, already failed).
    pub fn delay(&self, attempt: usize, err: &LlmError) -> Duration {
        if let LlmError::RateLimited { retry_after: Some(ra), .. } = err {
            return (*ra).min(self.max_retry_after);
        }
        let exp = self.base.as_millis() as f64 * 2f64.powi(attempt.saturating_sub(1) as i32);
        Duration::from_millis((exp * jitter_factor()) as u64).min(self.max_delay)
    }
}

/// Shared breaker state keyed by provider label.
#[derive(Debug, Clone, Default)]
pub struct CircuitBreaker {
    open_until: Arc<Mutex<HashMap<String, Instant>>>,
}

impl CircuitBreaker {
    pub fn is_open(&self, key: &str) -> bool {
        let mut m = match self.open_until.lock() {
            Ok(m) => m,
            Err(_) => return false,
        };
        match m.get(key) {
            Some(t) if Instant::now() < *t => true,
            Some(_) => {
                m.remove(key);
                false
            }
            None => false,
        }
    }
    pub fn trip(&self, key: &str, cooldown: Duration) {
        if let Ok(mut m) = self.open_until.lock() {
            m.insert(key.to_string(), Instant::now() + cooldown);
        }
    }
    pub fn reset(&self, key: &str) {
        if let Ok(mut m) = self.open_until.lock() {
            m.remove(key);
        }
    }
}

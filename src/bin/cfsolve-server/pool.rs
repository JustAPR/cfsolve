use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Maximum age of a token in the pool before it is considered expired (240s safety margin within Cloudflare's 300s TTL).
pub const DEFAULT_POOL_TTL: Duration = Duration::from_secs(240);
/// Maximum pre-minted tokens to hold per unique key.
pub const DEFAULT_MAX_PER_KEY: usize = 2;

/// Key identifying a unique Turnstile challenge context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub sitekey: String,
    pub action: Option<String>,
    pub cdata: Option<String>,
    pub url: String,
    pub profile: Option<String>,
    pub proxy: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PooledToken {
    pub token: String,
    pub created_at: Instant,
}

#[derive(Default)]
struct InnerPool {
    tokens: HashMap<PoolKey, VecDeque<PooledToken>>,
    in_flight: HashMap<PoolKey, usize>,
}

/// Thread-safe concurrent pre-minting pool.
#[derive(Clone)]
pub struct TokenPool {
    inner: Arc<RwLock<InnerPool>>,
    max_per_key: usize,
    ttl: Duration,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl Default for TokenPool {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PER_KEY, DEFAULT_POOL_TTL)
    }
}

impl TokenPool {
    pub fn new(max_per_key: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(InnerPool::default())),
            max_per_key,
            ttl,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Try to pop a fresh unexpired token from the pool for `key`.
    pub async fn pop(&self, key: &PoolKey) -> Option<String> {
        let mut guard = self.inner.write().await;
        let ttl = self.ttl;
        if let Some(deque) = guard.tokens.get_mut(key) {
            while let Some(item) = deque.pop_front() {
                if item.created_at.elapsed() < ttl {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Some(item.token);
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Calculates how many tokens should be minted to reach `max_per_key` capacity.
    pub async fn needed_count(&self, key: &PoolKey) -> usize {
        let guard = self.inner.read().await;
        let ttl = self.ttl;
        let cached = guard
            .tokens
            .get(key)
            .map(|q| q.iter().filter(|t| t.created_at.elapsed() < ttl).count())
            .unwrap_or(0);
        let in_flight = guard.in_flight.get(key).copied().unwrap_or(0);
        let total = cached + in_flight;
        if total < self.max_per_key {
            self.max_per_key - total
        } else {
            0
        }
    }

    /// Atomically records that background workers are starting to mint for `key`.
    pub async fn register_in_flight(&self, key: &PoolKey, count: usize) {
        let mut guard = self.inner.write().await;
        *guard.in_flight.entry(key.clone()).or_insert(0) += count;
    }

    /// Decrements in-flight counter and pushes minted token into deque if provided.
    pub async fn finish_in_flight(&self, key: &PoolKey, token: Option<String>) {
        let mut guard = self.inner.write().await;
        if let Some(count) = guard.in_flight.get_mut(key) {
            if *count > 0 {
                *count -= 1;
            }
        }
        if let Some(tok) = token {
            let deque = guard.tokens.entry(key.clone()).or_default();
            if deque.len() < self.max_per_key {
                deque.push_back(PooledToken {
                    token: tok,
                    created_at: Instant::now(),
                });
            }
        }
    }

    /// Periodically purge expired tokens across all keys.
    pub async fn prune_expired(&self) {
        let mut guard = self.inner.write().await;
        let ttl = self.ttl;
        guard.tokens.retain(|_, deque| {
            deque.retain(|t| t.created_at.elapsed() < ttl);
            !deque.is_empty()
        });
        guard.in_flight.retain(|_, count| *count > 0);
    }

    /// Get total active unexpired token count across all keys.
    pub async fn total_count(&self) -> usize {
        let guard = self.inner.read().await;
        let ttl = self.ttl;
        guard
            .tokens
            .values()
            .map(|q| q.iter().filter(|t| t.created_at.elapsed() < ttl).count())
            .sum()
    }

    /// Returns (hits, misses).
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_pool_pop_push_expiration() {
        let pool = TokenPool::new(2, Duration::from_millis(50));
        let key = PoolKey {
            sitekey: "0x4AAAAAA".to_string(),
            action: None,
            cdata: None,
            url: "https://example.com/".to_string(),
            profile: None,
            proxy: None,
        };

        assert_eq!(pool.pop(&key).await, None);
        assert_eq!(pool.needed_count(&key).await, 2);

        pool.register_in_flight(&key, 1).await;
        assert_eq!(pool.needed_count(&key).await, 1);

        pool.finish_in_flight(&key, Some("token_123".to_string()))
            .await;
        assert_eq!(pool.needed_count(&key).await, 1);

        assert_eq!(pool.pop(&key).await, Some("token_123".to_string()));
        assert_eq!(pool.pop(&key).await, None);

        // Test expiration
        pool.finish_in_flight(&key, Some("token_fast_expire".to_string()))
            .await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(pool.pop(&key).await, None);
    }
}

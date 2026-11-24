use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, watch};

use anyhow::{Result, anyhow};
use cfsolve::{Browser, Options, PageSource, Turnstile, Widget};
use url::Url;

use crate::models::{BucketStartRequest, BucketStatusResponse};

pub const DEFAULT_BUCKET_CAPACITY: usize = 5;
pub const MAX_BUCKET_CAPACITY: usize = 20;
pub const DEFAULT_IDLE_TIMEOUT_MINS: u64 = 30;
pub const TOKEN_MAX_AGE: Duration = Duration::from_secs(240);

#[derive(Clone, Debug)]
pub struct BucketToken {
    pub token: String,
    pub created_at: Instant,
}

pub struct BucketSession {
    pub session_id: String,
    pub url: String,
    pub sitekey: String,
    pub action: Option<String>,
    pub cdata: Option<String>,
    pub proxy: Option<String>,
    pub profile: Option<String>,
    pub capacity: usize,
    pub idle_timeout: Duration,
    pub tokens: VecDeque<BucketToken>,
    pub in_flight: usize,
    pub total_minted: u64,
    pub total_consumed: u64,
    pub created_at: Instant,
    pub last_accessed_at: Instant,
    pub shutdown_tx: Option<watch::Sender<bool>>,
    pub is_closed: bool,
}

impl BucketSession {
    pub fn prune_expired(&mut self) {
        let ttl = TOKEN_MAX_AGE;
        self.tokens.retain(|t| t.created_at.elapsed() < ttl);
    }

    pub fn pop(&mut self) -> Option<String> {
        self.last_accessed_at = Instant::now();
        self.prune_expired();
        if let Some(item) = self.tokens.pop_front() {
            self.total_consumed += 1;
            Some(item.token)
        } else {
            None
        }
    }

    pub fn status(&self) -> BucketStatusResponse {
        let ttl = TOKEN_MAX_AGE;
        let valid_tokens = self
            .tokens
            .iter()
            .filter(|t| t.created_at.elapsed() < ttl)
            .count();
        let idle_elapsed = self.last_accessed_at.elapsed();
        let idle_remaining = self
            .idle_timeout
            .saturating_sub(idle_elapsed)
            .as_secs();

        BucketStatusResponse {
            session_id: self.session_id.clone(),
            url: self.url.clone(),
            sitekey: self.sitekey.clone(),
            status: if self.is_closed { "closed" } else { "active" },
            capacity: self.capacity,
            ready_tokens: valid_tokens,
            in_flight_minters: self.in_flight,
            total_minted: self.total_minted,
            total_consumed: self.total_consumed,
            idle_seconds_remaining: idle_remaining,
            uptime_seconds: self.created_at.elapsed().as_secs(),
        }
    }
}

/// Thread-safe manager for all active leased bucket sessions.
#[derive(Clone, Default)]
pub struct BucketManager {
    sessions: Arc<RwLock<HashMap<String, Arc<RwLock<BucketSession>>>>>,
}

impl BucketManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates or updates a leased token bucket session and starts its autonomous refill daemon.
    pub async fn start_session(
        &self,
        req: BucketStartRequest,
        browser_loader: impl Fn(Option<&str>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Arc<Browser>>> + Send>>
            + Send
            + Sync
            + 'static
            + Clone,
        semaphore: Arc<tokio::sync::Semaphore>,
    ) -> Result<BucketStatusResponse> {
        let session_id = req
            .session_id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                let rand_id: u32 = rand_u32();
                format!("bucket-{:08x}", rand_id)
            });

        let capacity = req
            .capacity
            .unwrap_or(DEFAULT_BUCKET_CAPACITY)
            .clamp(1, MAX_BUCKET_CAPACITY);
        let idle_timeout = Duration::from_secs(
            req.idle_timeout_minutes
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_MINS)
                .max(1)
                * 60,
        );
        let normalized_proxy = req.proxy.as_deref().map(cfsolve::normalize_proxy);

        // Check if session already exists
        {
            let mut map = self.sessions.write().await;
            if let Some(existing_arc) = map.get(&session_id) {
                let mut session = existing_arc.write().await;
                if !session.is_closed {
                    session.capacity = capacity;
                    session.idle_timeout = idle_timeout;
                    session.last_accessed_at = Instant::now();
                    return Ok(session.status());
                }
            }

            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let session = Arc::new(RwLock::new(BucketSession {
                session_id: session_id.clone(),
                url: req.url.clone(),
                sitekey: req.sitekey.clone(),
                action: req.action.clone(),
                cdata: req.cdata.clone(),
                proxy: normalized_proxy.clone(),
                profile: req.profile.clone(),
                capacity,
                idle_timeout,
                tokens: VecDeque::with_capacity(capacity),
                in_flight: 0,
                total_minted: 0,
                total_consumed: 0,
                created_at: Instant::now(),
                last_accessed_at: Instant::now(),
                shutdown_tx: Some(shutdown_tx),
                is_closed: false,
            }));

            map.insert(session_id.clone(), session.clone());

            // Spawn autonomous refill daemon for this bucket
            let session_for_daemon = session.clone();
            let session_id_daemon = session_id.clone();
            let sessions_map_weak = Arc::downgrade(&self.sessions);

            tokio::spawn(async move {
                run_bucket_refill_daemon(
                    session_for_daemon,
                    shutdown_rx,
                    session_id_daemon,
                    sessions_map_weak,
                    browser_loader,
                    semaphore,
                )
                .await;
            });

            let guard = session.read().await;
            Ok(guard.status())
        }
    }

    /// Pops a fresh token from a bucket session in 0ms (or short wait if momentarily empty).
    pub async fn pop_token(
        &self,
        session_id: &str,
        wait_timeout: Duration,
    ) -> Result<Option<(String, usize, usize, u64)>> {
        let session_arc = {
            let map = self.sessions.read().await;
            map.get(session_id).cloned()
        };

        let Some(session_arc) = session_arc else {
            return Err(anyhow!("Bucket session '{}' not found or expired", session_id));
        };

        let start = Instant::now();
        loop {
            {
                let mut session = session_arc.write().await;
                if session.is_closed {
                    return Err(anyhow!("Bucket session '{}' is closed", session_id));
                }
                if let Some(token) = session.pop() {
                    let remaining = session.tokens.len();
                    let capacity = session.capacity;
                    let consumed = session.total_consumed;
                    return Ok(Some((token, remaining, capacity, consumed)));
                }
            }

            if start.elapsed() >= wait_timeout {
                return Ok(None);
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Retrieves status of a specific session bucket.
    pub async fn get_status(&self, session_id: &str) -> Option<BucketStatusResponse> {
        let map = self.sessions.read().await;
        let session = map.get(session_id)?;
        let guard = session.read().await;
        Some(guard.status())
    }

    /// Lists status for all active bucket sessions.
    pub async fn list_sessions(&self) -> Vec<BucketStatusResponse> {
        let map = self.sessions.read().await;
        let mut list = Vec::with_capacity(map.len());
        for session in map.values() {
            let guard = session.read().await;
            list.push(guard.status());
        }
        list
    }

    /// Closes a bucket session immediately, halts background minters, and deletes from memory.
    pub async fn close_session(&self, session_id: &str) -> bool {
        let mut map = self.sessions.write().await;
        if let Some(session_arc) = map.remove(session_id) {
            let mut session = session_arc.write().await;
            session.is_closed = true;
            if let Some(tx) = session.shutdown_tx.take() {
                let _ = tx.send(true);
            }
            session.tokens.clear();
            true
        } else {
            false
        }
    }

    /// Periodic housekeeper: cleans expired tokens and destroys sessions exceeding idle TTL.
    pub async fn prune_idle_and_expired(&self) {
        let mut map = self.sessions.write().await;
        let mut to_remove = Vec::new();

        for (id, session_arc) in map.iter() {
            let mut session = session_arc.write().await;
            session.prune_expired();

            if session.is_closed || session.last_accessed_at.elapsed() >= session.idle_timeout {
                session.is_closed = true;
                if let Some(tx) = session.shutdown_tx.take() {
                    let _ = tx.send(true);
                }
                session.tokens.clear();
                to_remove.push(id.clone());
            }
        }

        for id in to_remove {
            tracing::info!("Bucket session '{}' idle lease expired. Deleted from memory.", id);
            map.remove(&id);
        }
    }
}

/// Autonomous background loop that keeps a session bucket filled to capacity.
async fn run_bucket_refill_daemon(
    session_arc: Arc<RwLock<BucketSession>>,
    mut shutdown_rx: watch::Receiver<bool>,
    session_id: String,
    sessions_map_weak: std::sync::Weak<RwLock<HashMap<String, Arc<RwLock<BucketSession>>>>>,
    browser_loader: impl Fn(Option<&str>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Arc<Browser>>> + Send>>
        + Send
        + Sync
        + 'static
        + Clone,
    semaphore: Arc<tokio::sync::Semaphore>,
) {
    tracing::info!("Starting autonomous refill daemon for bucket '{}'", session_id);

    loop {
        // Check for shutdown signal
        if *shutdown_rx.borrow() {
            break;
        }

        // Check if session has timed out due to inactivity
        let (should_close, needs_mint, config_opt) = {
            let mut session = session_arc.write().await;
            if session.is_closed {
                (true, false, None)
            } else if session.last_accessed_at.elapsed() >= session.idle_timeout {
                session.is_closed = true;
                (true, false, None)
            } else {
                session.prune_expired();
                let current_tokens = session.tokens.len();
                let in_flight = session.in_flight;
                let needed = current_tokens + in_flight < session.capacity;

                // Max 1 concurrent background minter per bucket to prevent CPU thrashing
                if needed && in_flight < 1 {
                    session.in_flight += 1;
                    (
                        false,
                        true,
                        Some((
                            session.url.clone(),
                            session.sitekey.clone(),
                            session.action.clone(),
                            session.cdata.clone(),
                            session.proxy.clone(),
                            session.profile.clone(),
                        )),
                    )
                } else {
                    (false, false, None)
                }
            }
        };

        if should_close {
            tracing::info!("Refill daemon exiting for bucket '{}' (closed or idle)", session_id);
            if let Some(map_arc) = sessions_map_weak.upgrade() {
                let mut map = map_arc.write().await;
                map.remove(&session_id);
            }
            break;
        }

        if needs_mint {
            if let Some((url, sitekey, action, cdata, proxy, profile)) = config_opt {
                let session_arc_inner = session_arc.clone();
                let loader = browser_loader.clone();
                let sem = semaphore.clone();
                let sid = session_id.clone();

                tokio::spawn(async move {
                    let solve_res = async {
                        let _permit = sem
                            .acquire()
                            .await
                            .map_err(|_| anyhow!("Semaphore closed"))?;
                        let browser = loader(profile.as_deref()).await?;
                        let parsed_url = Url::parse(&url)?;
                        let widget = Some(Widget {
                            sitekey: sitekey.clone(),
                            action,
                            cdata,
                        });
                        let turnstile = Turnstile {
                            url: parsed_url,
                            page: PageSource::Blank,
                            widget,
                        };
                        let options = Options {
                            proxy,
                            trusted_root: None,
                            sink: None,
                        };
                        let token = browser
                            .solve_turnstile(turnstile, &options)
                            .await
                            .map_err(crate::handlers::format_cf_error)?;
                        Ok::<String, anyhow::Error>(token)
                    }
                    .await;

                    let mut session = session_arc_inner.write().await;
                    if session.in_flight > 0 {
                        session.in_flight -= 1;
                    }
                    match solve_res {
                        Ok(token) => {
                            if !session.is_closed && session.tokens.len() < session.capacity {
                                session.total_minted += 1;
                                session.tokens.push_back(BucketToken {
                                    token,
                                    created_at: Instant::now(),
                                });
                                tracing::info!(
                                    "Bucket '{}' refilled. Ready tokens: {}/{}",
                                    sid,
                                    session.tokens.len(),
                                    session.capacity
                                );
                            }
                        }
                        Err(err) => {
                            tracing::warn!("Bucket '{}' minting failure: {err:#}", sid);
                            drop(session);
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                });
            }
        }

        tokio::select! {
            _ = shutdown_rx.changed() => break,
            _ = tokio::time::sleep(Duration::from_millis(600)) => {}
        }
    }
}

fn rand_u32() -> u32 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    u32::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bucket_session_pop_and_prune() {
        let mut session = BucketSession {
            session_id: "test-session".to_string(),
            url: "https://example.com/".to_string(),
            sitekey: "0x4AAAA".to_string(),
            action: None,
            cdata: None,
            proxy: None,
            profile: None,
            capacity: 3,
            idle_timeout: Duration::from_secs(60),
            tokens: VecDeque::new(),
            in_flight: 0,
            total_minted: 0,
            total_consumed: 0,
            created_at: Instant::now(),
            last_accessed_at: Instant::now(),
            shutdown_tx: None,
            is_closed: false,
        };

        assert_eq!(session.pop(), None);
        session.tokens.push_back(BucketToken {
            token: "tok_1".to_string(),
            created_at: Instant::now(),
        });
        assert_eq!(session.pop(), Some("tok_1".to_string()));
        assert_eq!(session.total_consumed, 1);
        assert_eq!(session.pop(), None);
    }
}

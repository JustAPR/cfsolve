use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use url::Url;

use cfsolve::{Clearance, FetchedPage, Options, Outcome, PageSource, Turnstile, Widget};

use crate::models::{
    BucketStartRequest, BucketStartResponse, BucketStatusResponse, BucketTokenResponse,
    ClearRequest, ClearResponse, CookieData, HealthResponse, SolveRequest, SolveResponse,
    TurnstileRequest, TurnstileResponse,
};
use crate::pool::PoolKey;
use crate::state::{AppState, RequestTracker};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Dashboard and Playground HTML UI handler.
pub async fn dashboard_handler() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// Root handler: returns interactive dashboard for browsers, or plaintext for curl/CLI.
pub async fn index_handler(headers: HeaderMap) -> Response {
    let accepts_html = headers
        .get(header::ACCEPT)
        .and_then(|val| val.to_str().ok())
        .map(|val| val.contains("text/html"))
        .unwrap_or(false);

    if accepts_html {
        Html(DASHBOARD_HTML).into_response()
    } else {
        "cfsolve high-performance async API server\n\n\
        Web Dashboard & Interactive Playground:\n\
        - Open in browser: http://127.0.0.1:3000/ (or /dashboard, /playground)\n\n\
        API Endpoints:\n\
        - POST /turnstile  : Solves a Turnstile widget (returns token)\n\
        - POST /clear      : Solves a Cloudflare challenge page (returns cf_clearance & cookies)\n\
        - POST /solve      : Auto-detects and solves Turnstile or challenge page\n\
        - GET  /health     : Health check and server metrics\n\
        - GET  /stats      : Alias for /health\n"
            .into_response()
    }
}

/// Health and status handler.
pub async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let loaded = state.list_profiles().await;
    let (hits, misses) = state.token_pool.stats();
    let pooled = state.token_pool.total_count().await;
    let active_buckets = state.bucket_manager.list_sessions().await.len();
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        default_profile: state.default_profile_name.clone(),
        loaded_profiles: loaded,
        active_requests: state.active_requests.load(Ordering::Relaxed),
        total_requests: state.total_requests.load(Ordering::Relaxed),
        failed_requests: state.failed_requests.load(Ordering::Relaxed),
        pooled_tokens: pooled,
        pool_hits: hits,
        pool_misses: misses,
        active_buckets,
    })
}

/// Solves a Turnstile widget.
pub async fn turnstile_handler(
    State(state): State<AppState>,
    Json(payload): Json<TurnstileRequest>,
) -> (StatusCode, Json<TurnstileResponse>) {
    let _tracker = RequestTracker::new(state.active_requests.clone());
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let start_time = Instant::now();

    // If request specifies a leased bucket session, pop directly from it in 0ms
    if let Some(ref session_id) = payload.session_id {
        if let Ok(Some((token, _rem, _cap, _cons))) = state
            .bucket_manager
            .pop_token(session_id, Duration::from_millis(100))
            .await
        {
            return (
                StatusCode::OK,
                Json(TurnstileResponse {
                    success: true,
                    token: Some(token),
                    error: None,
                    elapsed_ms: start_time.elapsed().as_millis(),
                    cached: Some(true),
                }),
            );
        }
    }

    let use_pool = payload.pre_mint.unwrap_or(true);
    let normalized_proxy = payload.proxy.as_deref().map(cfsolve::normalize_proxy);

    let pool_key = payload.sitekey.as_ref().map(|sitekey| PoolKey {
        sitekey: sitekey.clone(),
        action: payload.action.clone(),
        cdata: payload.cdata.clone(),
        url: payload.url.clone(),
        profile: payload.profile.clone(),
        proxy: normalized_proxy.clone(),
    });

    if use_pool {
        if let Some(ref key) = pool_key {
            if let Some(token) = state.token_pool.pop(key).await {
                // Background replenish 1 token to keep the pool warm
                state.trigger_replenish(key.clone(), 1);

                return (
                    StatusCode::OK,
                    Json(TurnstileResponse {
                        success: true,
                        token: Some(token),
                        error: None,
                        elapsed_ms: start_time.elapsed().as_millis(),
                        cached: Some(true),
                    }),
                );
            }
        }
    }

    let timeout_duration =
        Duration::from_secs(payload.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let solve_future = async {
        let _permit = state
            .solve_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("Server semaphore closed"))?;
        let browser = state.get_browser(payload.profile.as_deref()).await?;
        let parsed_url = Url::parse(&payload.url)?;

        let widget = payload.sitekey.as_ref().map(|key| Widget {
            sitekey: key.clone(),
            action: payload.action.clone(),
            cdata: payload.cdata.clone(),
        });

        let page_source = match payload.page_source.as_deref() {
            Some("blank") => PageSource::Blank,
            Some("navigate") => PageSource::Navigate,
            Some("html") => PageSource::Html {
                html: payload.html.unwrap_or_default(),
                referrer_policy: payload.referrer_policy,
            },
            _ => {
                if let Some(html) = payload.html {
                    PageSource::Html {
                        html,
                        referrer_policy: payload.referrer_policy,
                    }
                } else if widget.is_some() {
                    PageSource::Blank
                } else {
                    PageSource::Navigate
                }
            }
        };

        let turnstile = Turnstile {
            url: parsed_url,
            page: page_source,
            widget,
        };

        let options = Options {
            proxy: payload.proxy,
            trusted_root: None,
            sink: None,
        };

        let token = browser
            .solve_turnstile(turnstile, &options)
            .await
            .map_err(format_cf_error)?;

        Ok::<String, anyhow::Error>(token)
    };

    match tokio::time::timeout(timeout_duration, solve_future).await {
        Ok(Ok(token)) => {
            if use_pool {
                if let Some(ref key) = pool_key {
                    // Queue was cold or missed: pre-mint 2 reserve tokens into pool in background
                    state.trigger_replenish(key.clone(), 2);
                }
            }
            (
                StatusCode::OK,
                Json(TurnstileResponse {
                    success: true,
                    token: Some(token),
                    error: None,
                    elapsed_ms: start_time.elapsed().as_millis(),
                    cached: Some(false),
                }),
            )
        }
        Ok(Err(err)) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            tracing::error!("Turnstile solve error: {err:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(TurnstileResponse {
                    success: false,
                    token: None,
                    error: Some(format!("{err:#}")),
                    elapsed_ms: start_time.elapsed().as_millis(),
                    cached: None,
                }),
            )
        }
        Err(_) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(TurnstileResponse {
                    success: false,
                    token: None,
                    error: Some(format!(
                        "Timed out after {} seconds",
                        timeout_duration.as_secs()
                    )),
                    elapsed_ms: start_time.elapsed().as_millis(),
                    cached: None,
                }),
            )
        }
    }
}

/// Solves a Cloudflare challenge page (cf_clearance cookie).
pub async fn clear_handler(
    State(state): State<AppState>,
    Json(payload): Json<ClearRequest>,
) -> (StatusCode, Json<ClearResponse>) {
    let _tracker = RequestTracker::new(state.active_requests.clone());
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let start_time = Instant::now();

    let timeout_duration =
        Duration::from_secs(payload.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let clear_future = async {
        let _permit = state
            .solve_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("Server semaphore closed"))?;
        let browser = state.get_browser(payload.profile.as_deref()).await?;
        let options = Options {
            proxy: payload.proxy,
            trusted_root: None,
            sink: None,
        };

        let clearance = if let Some(fp) = payload.fetched_page {
            let status =
                cfsolve::StatusCode::from_u16(fp.status).unwrap_or(cfsolve::StatusCode::FORBIDDEN);
            let fetched = FetchedPage {
                url: Url::parse(&fp.url)?,
                status,
                html: fp.html,
                referrer_policy: fp.referrer_policy,
                set_cookies: fp.set_cookies,
            };
            browser
                .clear_fetched(fetched, &options)
                .await
                .map_err(format_cf_error)?
        } else {
            let url = Url::parse(&payload.url)?;
            browser
                .clear(&url, &options)
                .await
                .map_err(format_cf_error)?
        };

        Ok::<Clearance, anyhow::Error>(clearance)
    };

    match tokio::time::timeout(timeout_duration, clear_future).await {
        Ok(Ok(clearance)) => {
            let (cookies_data, cookie_header) = extract_cookie_info(&clearance.cookies);
            let page_url = clearance.page.url.to_string();
            let page_status = clearance.page.status.as_u16();

            (
                StatusCode::OK,
                Json(ClearResponse {
                    success: true,
                    clearance: Some(clearance.clearance),
                    cookies: Some(cookies_data),
                    cookie_header: Some(cookie_header),
                    page_url: Some(page_url),
                    page_status: Some(page_status),
                    error: None,
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
        Ok(Err(err)) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            tracing::error!("Challenge clear error: {err:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ClearResponse {
                    success: false,
                    clearance: None,
                    cookies: None,
                    cookie_header: None,
                    page_url: None,
                    page_status: None,
                    error: Some(format!("{err:#}")),
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
        Err(_) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(ClearResponse {
                    success: false,
                    clearance: None,
                    cookies: None,
                    cookie_header: None,
                    page_url: None,
                    page_status: None,
                    error: Some(format!(
                        "Timed out after {} seconds",
                        timeout_duration.as_secs()
                    )),
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
    }
}

/// Unified solve handler (auto-detects Turnstile vs Challenge page).
pub async fn solve_handler(
    State(state): State<AppState>,
    Json(payload): Json<SolveRequest>,
) -> (StatusCode, Json<SolveResponse>) {
    let _tracker = RequestTracker::new(state.active_requests.clone());
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let start_time = Instant::now();

    let timeout_duration =
        Duration::from_secs(payload.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS));

    let solve_future = async {
        let _permit = state
            .solve_semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("Server semaphore closed"))?;
        let browser = state.get_browser(payload.profile.as_deref()).await?;
        let options = Options {
            proxy: payload.proxy,
            trusted_root: None,
            sink: None,
        };
        let url = Url::parse(&payload.url)?;
        let outcome = browser
            .solve(&url, &options)
            .await
            .map_err(format_cf_error)?;

        Ok::<Outcome, anyhow::Error>(outcome)
    };

    match tokio::time::timeout(timeout_duration, solve_future).await {
        Ok(Ok(Outcome::Token(token))) => (
            StatusCode::OK,
            Json(SolveResponse {
                success: true,
                solve_type: Some("token".to_string()),
                token: Some(token),
                clearance: None,
                cookies: None,
                cookie_header: None,
                page_url: Some(payload.url),
                error: None,
                elapsed_ms: start_time.elapsed().as_millis(),
            }),
        ),
        Ok(Ok(Outcome::Cleared(clearance))) => {
            let (cookies_data, cookie_header) = extract_cookie_info(&clearance.cookies);
            let page_url = clearance.page.url.to_string();

            (
                StatusCode::OK,
                Json(SolveResponse {
                    success: true,
                    solve_type: Some("clearance".to_string()),
                    token: None,
                    clearance: Some(clearance.clearance),
                    cookies: Some(cookies_data),
                    cookie_header: Some(cookie_header),
                    page_url: Some(page_url),
                    error: None,
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
        Ok(Err(err)) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            tracing::error!("Solve error: {err:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SolveResponse {
                    success: false,
                    solve_type: None,
                    token: None,
                    clearance: None,
                    cookies: None,
                    cookie_header: None,
                    page_url: None,
                    error: Some(format!("{err:#}")),
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
        Err(_) => {
            state.failed_requests.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(SolveResponse {
                    success: false,
                    solve_type: None,
                    token: None,
                    clearance: None,
                    cookies: None,
                    cookie_header: None,
                    page_url: None,
                    error: Some(format!(
                        "Timed out after {} seconds",
                        timeout_duration.as_secs()
                    )),
                    elapsed_ms: start_time.elapsed().as_millis(),
                }),
            )
        }
    }
}

fn extract_cookie_info(cookies: &[cfsolve::Cookie<'static>]) -> (Vec<CookieData>, String) {
    let mut data = Vec::with_capacity(cookies.len());
    let mut header_parts = Vec::with_capacity(cookies.len());

    for c in cookies {
        data.push(CookieData {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain: c.domain().map(|d| d.to_string()),
            path: c.path().map(|p| p.to_string()),
            http_only: c.http_only(),
            secure: c.secure(),
            raw: c.to_string(),
        });
        header_parts.push(format!("{}={}", c.name(), c.value()));
    }

    let cookie_header = header_parts.join("; ");
    (data, cookie_header)
}

pub fn format_cf_error(err: cfsolve::Error) -> anyhow::Error {
    match err {
        cfsolve::Error::Network(e)
        | cfsolve::Error::Config(e)
        | cfsolve::Error::Rejected { error: e, .. }
        | cfsolve::Error::Sink(e)
        | cfsolve::Error::Unsupported(e) => {
            let mut chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
            chain.dedup();
            anyhow::anyhow!("{}", chain.join(": "))
        }
        other => anyhow::anyhow!("{other}"),
    }
}

/// Creates or updates a leased token bucket session.
pub async fn bucket_start_handler(
    State(state): State<AppState>,
    Json(payload): Json<BucketStartRequest>,
) -> (StatusCode, Json<BucketStartResponse>) {
    let idle_mins = payload
        .idle_timeout_minutes
        .unwrap_or(crate::bucket::DEFAULT_IDLE_TIMEOUT_MINS);
    match state.start_bucket(payload).await {
        Ok(status) => (
            StatusCode::OK,
            Json(BucketStartResponse {
                success: true,
                session_id: status.session_id,
                status: status.status,
                capacity: status.capacity,
                ready_tokens: status.ready_tokens,
                idle_timeout_minutes: idle_mins,
                message: format!(
                    "Bucket session active. Target capacity: {}, auto-expires if idle for {} mins.",
                    status.capacity, idle_mins
                ),
            }),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(BucketStartResponse {
                success: false,
                session_id: String::new(),
                status: "error",
                capacity: 0,
                ready_tokens: 0,
                idle_timeout_minutes: 0,
                message: format!("Failed to initialize bucket: {err:#}"),
            }),
        ),
    }
}

/// Pops a token from a leased bucket session with 0ms response time.
pub async fn bucket_token_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> (StatusCode, Json<BucketTokenResponse>) {
    let start_time = Instant::now();
    // Allow up to 15s wait if bucket is refilling
    match state
        .bucket_manager
        .pop_token(&session_id, Duration::from_secs(15))
        .await
    {
        Ok(Some((token, remaining, capacity, total_consumed))) => (
            StatusCode::OK,
            Json(BucketTokenResponse {
                success: true,
                session_id,
                token: Some(token),
                elapsed_ms: start_time.elapsed().as_millis(),
                remaining,
                capacity,
                total_consumed,
                error: None,
            }),
        ),
        Ok(None) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(BucketTokenResponse {
                success: false,
                session_id,
                token: None,
                elapsed_ms: start_time.elapsed().as_millis(),
                remaining: 0,
                capacity: 0,
                total_consumed: 0,
                error: Some("Bucket empty: timeout waiting for background minter".to_string()),
            }),
        ),
        Err(err) => (
            StatusCode::NOT_FOUND,
            Json(BucketTokenResponse {
                success: false,
                session_id,
                token: None,
                elapsed_ms: start_time.elapsed().as_millis(),
                remaining: 0,
                capacity: 0,
                total_consumed: 0,
                error: Some(format!("{err:#}")),
            }),
        ),
    }
}

/// Retrieves the live status of a bucket session.
pub async fn bucket_status_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(status) = state.bucket_manager.get_status(&session_id).await {
        (
            StatusCode::OK,
            Json(serde_json::to_value(status).unwrap_or_default()),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "error": format!("Bucket session '{}' not found or expired", session_id)
            })),
        )
    }
}

/// Lists all active bucket sessions.
pub async fn bucket_list_handler(
    State(state): State<AppState>,
) -> (StatusCode, Json<Vec<BucketStatusResponse>>) {
    let list = state.bucket_manager.list_sessions().await;
    (StatusCode::OK, Json(list))
}

/// Closes a bucket session immediately and purges its memory.
pub async fn bucket_close_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let closed = state.bucket_manager.close_session(&session_id).await;
    if closed {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "session_id": session_id,
                "message": "Bucket session halted and deleted from memory"
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "success": false,
                "session_id": session_id,
                "error": "Bucket session not found"
            })),
        )
    }
}

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use url::Url;

use cfsolve::{Clearance, FetchedPage, Options, Outcome, PageSource, Turnstile, Widget};

use crate::models::{
    ClearRequest, ClearResponse, CookieData, HealthResponse, SolveRequest, SolveResponse,
    TurnstileRequest, TurnstileResponse,
};
use crate::state::{AppState, RequestTracker};

const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Root info endpoint with usage guide.
pub async fn index_handler() -> &'static str {
    "cfsolve high-performance async API server\n\n\
    Endpoints:\n\
    - POST /turnstile  : Solves a Turnstile widget (returns token)\n\
    - POST /clear      : Solves a Cloudflare challenge page (returns cf_clearance & cookies)\n\
    - POST /solve      : Auto-detects and solves Turnstile or challenge page\n\
    - GET  /health     : Health check and server metrics\n\
    - GET  /stats      : Alias for /health\n"
}

/// Health and status handler.
pub async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let loaded = state.list_profiles().await;
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        default_profile: state.default_profile_name.clone(),
        loaded_profiles: loaded,
        active_requests: state.active_requests.load(Ordering::Relaxed),
        total_requests: state.total_requests.load(Ordering::Relaxed),
        failed_requests: state.failed_requests.load(Ordering::Relaxed),
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

        let widget = payload.sitekey.map(|key| Widget {
            sitekey: key,
            action: payload.action,
            cdata: payload.cdata,
        });

        let page_source = match payload.page_source.as_deref() {
            Some("blank") => PageSource::Blank,
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
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok::<String, anyhow::Error>(token)
    };

    match tokio::time::timeout(timeout_duration, solve_future).await {
        Ok(Ok(token)) => (
            StatusCode::OK,
            Json(TurnstileResponse {
                success: true,
                token: Some(token),
                error: None,
                elapsed_ms: start_time.elapsed().as_millis(),
            }),
        ),
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
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            let url = Url::parse(&payload.url)?;
            browser
                .clear(&url, &options)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
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
            .map_err(|e| anyhow::anyhow!("{e}"))?;

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

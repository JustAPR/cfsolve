use serde::{Deserialize, Serialize};

/// Request payload for solving Turnstile widgets.
#[derive(Debug, Deserialize)]
pub struct TurnstileRequest {
    /// URL of the page containing the Turnstile widget.
    pub url: String,
    /// Turnstile sitekey (e.g. "0x4AAAAAA..."). Optional if the page HTML has a container and page_source is navigate.
    pub sitekey: Option<String>,
    /// Widget action parameter.
    pub action: Option<String>,
    /// Widget cData customer parameter.
    pub cdata: Option<String>,
    /// Where the page comes from: "navigate" (default), "blank", or "html".
    #[serde(default)]
    pub page_source: Option<String>,
    /// Raw page HTML when page_source is "html".
    pub html: Option<String>,
    /// Optional Referrer-Policy header (e.g. "strict-origin-when-cross-origin").
    pub referrer_policy: Option<String>,
    /// Optional HTTP, HTTPS, or SOCKS5 proxy (e.g. "socks5h://user:pass@host:1080").
    pub proxy: Option<String>,
    /// Browser profile name or path (defaults to "safari-26-ios").
    pub profile: Option<String>,
    /// Maximum time in seconds to wait for solving (default 60s).
    pub timeout_seconds: Option<u64>,
    /// Whether to use the pre-minted token pool (default true). Set to false to force on-demand synchronous solve.
    #[serde(default)]
    pub pre_mint: Option<bool>,
    /// Optional leased bucket session identifier to pull from or associate with.
    pub session_id: Option<String>,
}

/// Response payload for Turnstile solving.
#[derive(Debug, Serialize)]
pub struct TurnstileResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
}

/// Optional input representing an already-fetched challenge page (status 403).
#[derive(Debug, Deserialize)]
pub struct FetchedPageInput {
    pub url: String,
    pub status: u16,
    pub html: String,
    pub referrer_policy: Option<String>,
    #[serde(default)]
    pub set_cookies: Vec<String>,
}

/// Request payload for clearing Cloudflare challenge pages (Managed Challenge / Interstitial).
#[derive(Debug, Deserialize)]
pub struct ClearRequest {
    /// Target protected URL.
    pub url: String,
    /// Optional proxy URL.
    pub proxy: Option<String>,
    /// Browser profile name or path.
    pub profile: Option<String>,
    /// Maximum time in seconds to wait for clearing (default 60s).
    pub timeout_seconds: Option<u64>,
    /// Pre-fetched challenge page details if the client already made the request.
    pub fetched_page: Option<FetchedPageInput>,
}

/// Serialized cookie information.
#[derive(Debug, Clone, Serialize)]
pub struct CookieData {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub http_only: bool,
    pub secure: bool,
    pub raw: String,
}

/// Response payload for clearing a challenge page.
#[derive(Debug, Serialize)]
pub struct ClearResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clearance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookies: Option<Vec<CookieData>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Request payload for unified solve endpoint (auto-detects Turnstile vs Challenge page).
#[derive(Debug, Deserialize)]
pub struct SolveRequest {
    pub url: String,
    pub proxy: Option<String>,
    pub profile: Option<String>,
    pub timeout_seconds: Option<u64>,
}

/// Response payload for unified solve endpoint.
#[derive(Debug, Serialize)]
pub struct SolveResponse {
    pub success: bool,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub solve_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clearance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookies: Option<Vec<CookieData>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

/// Health and status response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub default_profile: String,
    pub loaded_profiles: Vec<String>,
    pub active_requests: u64,
    pub total_requests: u64,
    pub failed_requests: u64,
    pub pooled_tokens: usize,
    pub pool_hits: u64,
    pub pool_misses: u64,
    pub active_buckets: usize,
}

/// Request payload to create or update an autonomous leased token bucket session.
#[derive(Debug, Deserialize)]
pub struct BucketStartRequest {
    /// Unique session identifier (e.g. "scraper-worker-1", or auto-generated if omitted).
    pub session_id: Option<String>,
    /// Target page URL.
    pub url: String,
    /// Turnstile sitekey.
    pub sitekey: String,
    /// Optional action.
    pub action: Option<String>,
    /// Optional cdata.
    pub cdata: Option<String>,
    /// Optional proxy (e.g. "socks5://...", "(Socks5)host:port:user:pass", or "http://...").
    pub proxy: Option<String>,
    /// Browser profile name (defaults to "safari-26-ios").
    pub profile: Option<String>,
    /// Target bucket capacity (default 5, max 20).
    pub capacity: Option<usize>,
    /// Inactivity idle timeout in minutes before bucket auto-deletes (default 30 minutes).
    pub idle_timeout_minutes: Option<u64>,
}

/// Response payload when creating or updating a bucket session.
#[derive(Debug, Serialize)]
pub struct BucketStartResponse {
    pub success: bool,
    pub session_id: String,
    pub status: &'static str,
    pub capacity: usize,
    pub ready_tokens: usize,
    pub idle_timeout_minutes: u64,
    pub message: String,
}

/// Response when popping a token from a bucket session.
#[derive(Debug, Serialize)]
pub struct BucketTokenResponse {
    pub success: bool,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub elapsed_ms: u128,
    pub remaining: usize,
    pub capacity: usize,
    pub total_consumed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Detailed status of a bucket session.
#[derive(Debug, Serialize, Clone)]
pub struct BucketStatusResponse {
    pub session_id: String,
    pub url: String,
    pub sitekey: String,
    pub status: &'static str,
    pub capacity: usize,
    pub ready_tokens: usize,
    pub in_flight_minters: usize,
    pub total_minted: u64,
    pub total_consumed: u64,
    pub idle_seconds_remaining: u64,
    pub uptime_seconds: u64,
}

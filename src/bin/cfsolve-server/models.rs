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
}

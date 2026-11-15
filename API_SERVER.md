# cfsolve Async REST API Server

`cfsolve-server` is a standalone, asynchronous HTTP REST API server built with **Axum** and **Tokio**. It exposes endpoints to solve Cloudflare Turnstile widgets and Managed Challenge interstitial pages natively, concurrently, and without running a headless browser.

---

## Key Highlights

- **Native Async & Multi-Threaded:** Powered by Tokio's multi-threaded runtime (`rt-multi-thread`).
- **High Concurrency & Low Memory:** Pre-loads and shares `Arc<Browser>` fingerprint profiles in read-only memory. Solves run concurrently without mutex lock contention.
- **Independent Sessions:** Each solve request automatically manages its own cookies, HTTP/2 multiplexing, and TLS transport.
- **Per-Request Proxy Support:** Send individual requests through residential or datacenter proxies (`http://`, `https://`, `socks5://`, `socks5h://`).
- **Live Metrics & Stats:** Real-time tracking of active requests, total solves, and failures.

---

## Starting the Server

### Development Build
```bash
cargo run --bin cfsolve-server
```

### Production Release Build
```bash
cargo run --release --bin cfsolve-server
# Or execute binary directly:
./target/release/cfsolve-server.exe
```

### Configuration Options
You can configure the server via CLI flags or environment variables:

| Flag | Env Variable | Default | Description |
|------|--------------|---------|-------------|
| `--host <IP>` | `HOST` | `0.0.0.0` | IP interface to bind to |
| `--port <PORT>` | `PORT` | `3000` | Port to listen on |
| `--profile-dir <DIR>` | `PROFILE_DIR` | `profiles` | Directory containing browser profiles (`.json`) |
| `--default-profile <NAME>` | `DEFAULT_PROFILE` | `safari-26-ios` | Default browser profile name |

Example with custom port and profile:
```bash
cargo run --bin cfsolve-server -- --host 127.0.0.1 --port 8080 --default-profile safari-26-ios
```

---

## API Endpoints

### 1. `GET /health` / `GET /stats`
Returns server operational status, loaded browser profiles, and real-time request metrics.

#### Response
```json
{
  "status": "ok",
  "version": "0.1.0",
  "default_profile": "safari-26-ios",
  "loaded_profiles": [
    "safari-26-ios"
  ],
  "active_requests": 0,
  "total_requests": 14,
  "failed_requests": 1
}
```

---

### 2. `POST /turnstile`
Solves a Cloudflare Turnstile widget on a target web page.

#### Request Schema
```json
{
  "url": "https://example.com/login",
  "sitekey": "0x4AAAAAAAJ8Q0gY-fS99999",
  "action": "login",
  "cdata": "custom-session-data",
  "page_source": "navigate",
  "html": null,
  "referrer_policy": null,
  "proxy": "socks5h://user:password@proxy.example.com:1080",
  "profile": "safari-26-ios",
  "timeout_seconds": 60
}
```

#### Fields:
- `url` (*required*): URL of the page containing the Turnstile widget.
- `sitekey` (*optional*): The Turnstile sitekey. If omitted, solves the first `.cf-turnstile` widget on the navigated page.
- `action` (*optional*): Widget action parameter.
- `cdata` (*optional*): Widget customer data parameter.
- `page_source` (*optional*):
  - `"navigate"` (default): Navigates to `url` to fetch the HTML.
  - `"blank"`: Renders the widget on a blank page at `url` (requires `sitekey`).
  - `"html"`: Uses the supplied HTML string instead of fetching.
- `html` (*optional*): Raw HTML if `page_source` is `"html"` (useful for pages behind a login).
- `referrer_policy` (*optional*): Referrer-Policy header string.
- `proxy` (*optional*): Upstream proxy URL (`http://`, `https://`, `socks5://`, or `socks5h://`).
- `profile` (*optional*): Profile name or path (defaults to server's default profile).
- `timeout_seconds` (*optional*): Request timeout in seconds (default 60).

#### Success Response (`HTTP 200`)
```json
{
  "success": true,
  "token": "0.4c97926b4845014b2d3077...",
  "elapsed_ms": 1320
}
```

#### Error Response (`HTTP 400` / `500` / `504`)
```json
{
  "success": false,
  "error": "Timeout or solve error description",
  "elapsed_ms": 60001
}
```

---

### 3. `POST /clear`
Solves a Cloudflare Challenge page (Managed Challenge / WAF interstitial) and obtains `cf_clearance`.

#### Request Schema
```json
{
  "url": "https://example.com/protected",
  "proxy": "socks5h://user:password@proxy.example.com:1080",
  "profile": "safari-26-ios",
  "timeout_seconds": 60,
  "fetched_page": null
}
```

#### Pre-Fetched Page Support (Optional):
If your application already received the initial 403 response, pass it in `fetched_page`:
```json
{
  "url": "https://example.com/protected",
  "fetched_page": {
    "url": "https://example.com/protected",
    "status": 403,
    "html": "<!DOCTYPE html><html>...</html>",
    "referrer_policy": null,
    "set_cookies": []
  }
}
```

#### Success Response (`HTTP 200`)
```json
{
  "success": true,
  "clearance": "gJb3c6...cf_clearance_cookie_value",
  "cookies": [
    {
      "name": "cf_clearance",
      "value": "gJb3c6...",
      "domain": ".example.com",
      "path": "/",
      "http_only": true,
      "secure": true,
      "raw": "cf_clearance=gJb3c6...; Path=/; Domain=.example.com; HttpOnly; Secure"
    }
  ],
  "cookie_header": "cf_clearance=gJb3c6...",
  "page_url": "https://example.com/protected",
  "page_status": 200,
  "elapsed_ms": 2450
}
```

---

### 4. `POST /solve`
Unified solver endpoint. Navigates to the given URL and automatically solves whichever challenge is presented:
- If a Turnstile widget is found, it returns `type: "token"`.
- If a Managed Challenge is found, it clears it and returns `type: "clearance"`.

#### Request Schema
```json
{
  "url": "https://example.com/page",
  "proxy": "socks5h://user:password@host:port",
  "profile": "safari-26-ios",
  "timeout_seconds": 60
}
```

---

## Client Usage Examples

### cURL
```bash
# Turnstile widget solve
curl -X POST http://127.0.0.1:3000/turnstile \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://example.com/login",
    "sitekey": "0x4AAAAAA...",
    "page_source": "blank"
  }'

# Managed Challenge solve
curl -X POST http://127.0.0.1:3000/clear \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://example.com/protected"
  }'
```

### Python Async Client (`httpx` + `asyncio`)
See [scripts/client_example.py](file:///C:/Users/praja/desktop/Random_Shit/HiggsChecker/cfsolve/scripts/client_example.py) for an end-to-end example that concurrently dispatches multiple requests using `asyncio.gather()`.

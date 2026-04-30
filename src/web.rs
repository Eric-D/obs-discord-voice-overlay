//! HTTP + SSE surface served to OBS, plus the `/setup` configuration wizard.
//!
//! Routes:
//! - `GET /` — resolve `?theme=<name>` (default fallback) -> serve theme HTML.
//! - `GET /themes/<name>/style.css` and `/themes/<name>/app.js` — theme assets.
//! - `GET /events` — SSE stream of overlay deltas, with auto keep-alive so
//!   browsers can auto-reconnect.
//! - `GET /setup` — HTML wizard to (re)configure the encrypted on-disk config.
//! - `POST /setup` — accepts the form submission, encrypts + writes atomically.
//!
//! When the binary launches without a usable persistent config (missing,
//! decrypt failure, machine-id unavailable), the `setup_mode` flag flips on
//! and a middleware short-circuits every non-`/setup` route to HTTP 503.
//! IPC + OAuth do not start in that mode.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Form, Router,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::{Config, Secrets};
use crate::state::{OverlayStore, StateDelta};
use crate::themes;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<OverlayStore>,
    pub setup_mode: Arc<AtomicBool>,
    /// Snapshot of the loaded config used to pre-fill the reconfigure form.
    /// `None` while in setup mode (no usable config on disk yet). Held in a
    /// Mutex so a successful POST /setup can update it without restarting.
    pub current_config: Arc<Mutex<Option<Config>>>,
    pub config_path: Arc<PathBuf>,
    /// Serializes the validate -> save -> update-snapshot critical section
    /// of POST /setup so two concurrent submissions can not race on the
    /// `.tmp` file or produce a torn `current_config` snapshot.
    pub save_lock: Arc<Mutex<()>>,
    /// Bind port, used to validate the `Host` and `Origin` headers on
    /// POST /setup as a lightweight CSRF defense for localhost-only
    /// deployments.
    pub bind_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct ThemeQuery {
    #[serde(default)]
    pub theme: Option<String>,
}

/// Build the axum router, including the `/setup` routes and the setup-mode
/// middleware that 503s every other route while config is missing.
pub fn router(state: AppState) -> Router {
    // 64 KiB is plenty for a setup-form submission and small enough that a
    // hostile localhost process can't produce allocator pressure by spamming
    // huge bodies. We apply this globally; the cost on the SSE / GET routes
    // is just an extra body-stream wrapper.
    let body_limit = RequestBodyLimitLayer::new(64 * 1024);

    let setup_routes = Router::new()
        .route("/setup", get(setup_get))
        .route("/setup", post(setup_post))
        .with_state(state.clone());

    let app_routes = Router::new()
        .route("/", get(root))
        .route("/themes/:name/style.css", get(theme_css))
        .route("/themes/:name/app.js", get(theme_js))
        .route("/events", get(sse))
        .with_state(state.clone());

    Router::new()
        .merge(setup_routes)
        .merge(app_routes)
        .layer(body_limit)
        .layer(middleware::from_fn_with_state(state, setup_mode_guard))
}

/// Middleware: when `setup_mode == true`, every non-`/setup` route returns 503.
async fn setup_mode_guard(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if state.setup_mode.load(Ordering::Acquire) {
        let path = request.uri().path();
        if path != "/setup" {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Configuration required \u{2014} visit /setup",
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn root(Query(q): Query<ThemeQuery>) -> impl IntoResponse {
    let t = themes::resolve(q.theme.as_deref());
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        t.html,
    )
}

async fn theme_css(Path(name): Path<String>) -> axum::response::Response {
    // Asset routes must NOT silently fall back to `default` — that mismatches
    // with another theme's HTML and is hard to debug. Return 404 instead.
    match themes::find_exact(&name) {
        Some(t) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            t.css,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "theme not found").into_response(),
    }
}

async fn theme_js(Path(name): Path<String>) -> axum::response::Response {
    match themes::find_exact(&name) {
        Some(t) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
            t.js,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "theme not found").into_response(),
    }
}

/// SSE stream. Sends a full `state` event on connect, then incremental deltas.
///
/// Ordering matters: we subscribe to deltas **before** capturing the snapshot.
/// Otherwise a delta produced between snapshot capture and subscription would
/// be lost and the browser would never observe it.
async fn sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let deltas_rx = state.store.subscribe_deltas();
    let initial = state.store.snapshot();
    let store = state.store.clone();

    let init_stream =
        tokio_stream::once(Ok::<Event, Infallible>(delta_to_event(&StateDelta::State(initial))));

    let deltas_stream = tokio_stream::wrappers::BroadcastStream::new(deltas_rx).map(
        move |res| -> Result<Event, Infallible> {
            match res {
                Ok(delta) => Ok(delta_to_event(&delta)),
                // The browser dropped behind the broadcast buffer. Send a
                // fresh `state` snapshot so it can re-sync rather than
                // silently dropping events.
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!("sse: broadcast lagged by {n}; emitting full snapshot");
                    let snap = store.snapshot();
                    Ok(delta_to_event(&StateDelta::State(snap)))
                }
            }
        },
    );

    let stream = init_stream.chain(deltas_stream);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn delta_to_event(d: &StateDelta) -> Event {
    let (event_name, payload) = match d {
        StateDelta::State(s) => ("state", serde_json::to_string(s).unwrap_or_default()),
        StateDelta::ParticipantJoin(p) => (
            "participant_join",
            serde_json::to_string(p).unwrap_or_default(),
        ),
        StateDelta::ParticipantLeave { user_id } => (
            "participant_leave",
            serde_json::to_string(&serde_json::json!({ "user_id": user_id }))
                .unwrap_or_default(),
        ),
        StateDelta::SpeakingStart { user_id } => (
            "speaking_start",
            serde_json::to_string(&serde_json::json!({ "user_id": user_id }))
                .unwrap_or_default(),
        ),
        StateDelta::SpeakingStop { user_id } => (
            "speaking_stop",
            serde_json::to_string(&serde_json::json!({ "user_id": user_id }))
                .unwrap_or_default(),
        ),
        StateDelta::VoiceStateUpdate(p) => (
            "voice_state_update",
            serde_json::to_string(p).unwrap_or_default(),
        ),
        StateDelta::Connection { connected } => (
            "connection",
            serde_json::to_string(&serde_json::json!({ "connected": connected }))
                .unwrap_or_default(),
        ),
    };
    Event::default().event(event_name).data(payload)
}

const SETUP_CSS: &str = r#"
:root { color-scheme: dark; }
body {
  background: #1e1f22;
  color: #dbdee1;
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  margin: 0;
  padding: 32px 16px;
  display: flex;
  justify-content: center;
}
.card {
  background: #2b2d31;
  border: 1px solid #1f2023;
  border-radius: 8px;
  padding: 24px 28px;
  max-width: 560px;
  width: 100%;
  box-shadow: 0 8px 24px rgba(0,0,0,0.35);
}
h1 { font-size: 22px; margin: 0 0 4px 0; color: #fff; }
p.lead { color: #b5bac1; margin: 0 0 16px 0; }
.notice {
  background: #313338;
  border-left: 3px solid #faa61a;
  padding: 10px 12px;
  border-radius: 4px;
  font-size: 13px;
  color: #dbdee1;
  margin: 12px 0 18px 0;
}
.error {
  background: #4a1f23;
  border-left: 3px solid #ed4245;
  padding: 10px 12px;
  border-radius: 4px;
  font-size: 13px;
  color: #fbd6d8;
  margin: 0 0 18px 0;
}
label { display: block; font-size: 12px; text-transform: uppercase; letter-spacing: .04em; color: #b5bac1; margin: 14px 0 6px; }
input[type=text], input[type=password], input[type=number] {
  width: 100%;
  box-sizing: border-box;
  background: #1e1f22;
  border: 1px solid #1a1b1e;
  color: #f2f3f5;
  font-size: 14px;
  padding: 9px 10px;
  border-radius: 4px;
}
input:focus { outline: 2px solid #5865f2; outline-offset: 0; }
button {
  margin-top: 18px;
  background: #5865f2;
  color: #fff;
  border: 0;
  padding: 10px 18px;
  border-radius: 4px;
  font-size: 14px;
  font-weight: 500;
  cursor: pointer;
}
button:hover { background: #4752c4; }
.help { font-size: 12px; color: #949ba4; margin-top: 4px; }
.success { color: #3ba55c; font-weight: 500; }
"#;

#[derive(Debug, Deserialize)]
struct SetupForm {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    overlay_port: u16,
}

fn render_setup_page(
    prefill_redirect_uri: &str,
    prefill_port: u16,
    error: Option<&str>,
    success: bool,
) -> String {
    let error_block = match error {
        Some(msg) => format!(
            r#"<div class="error">{}</div>"#,
            html_escape(msg)
        ),
        None => String::new(),
    };
    let header_blurb = if success {
        r#"<p class="success">Saved &mdash; please restart the app to apply the new configuration.</p>"#.to_string()
    } else {
        r#"<p class="lead">Configure your Discord application credentials. They are encrypted on disk with a key bound to this machine.</p>"#.to_string()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <title>OBS Discord Voice Overlay - Setup</title>
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>{css}</style>
</head>
<body>
  <div class="card">
    <h1>OBS Discord Voice Overlay</h1>
    {header_blurb}
    <div class="notice">
      <strong>Encrypted at rest.</strong> Values you submit here are encrypted with ChaCha20-Poly1305 under a key derived from this computer's machine ID. Copying the resulting <code>config.json</code> to a different machine will not decrypt - you will need to run setup again there.
    </div>
    {error_block}
    <form method="POST" action="/setup" autocomplete="off">
      <label for="client_id">Application ID</label>
      <input id="client_id" name="client_id" type="text" required placeholder="e.g. 1234567890123456789" />
      <div class="help">From <code>https://discord.com/developers/applications</code> -> your app -> General Information.</div>

      <label for="client_secret">Client Secret</label>
      <input id="client_secret" name="client_secret" type="password" required placeholder="paste your OAuth2 client secret" />
      <div class="help">From OAuth2 -> Reset Secret. Treat like a password; never re-shared.</div>

      <label for="redirect_uri">Redirect URI</label>
      <input id="redirect_uri" name="redirect_uri" type="text" required value="{redirect_uri}" />
      <div class="help">Must match exactly one URL listed under OAuth2 -> Redirects in the dev portal.</div>

      <label for="overlay_port">Overlay Port</label>
      <input id="overlay_port" name="overlay_port" type="number" min="1024" max="65535" required value="{port}" />
      <div class="help">Local HTTP port (default 7373). Change requires restart.</div>

      <button type="submit">Save</button>
    </form>
  </div>
</body>
</html>"#,
        css = SETUP_CSS,
        header_blurb = header_blurb,
        error_block = error_block,
        redirect_uri = html_escape(prefill_redirect_uri),
        port = prefill_port,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn setup_get(State(state): State<AppState>) -> Response {
    // Pre-fill non-secret fields from the current config when reconfiguring.
    let (redirect_uri, port) = {
        let guard = state.current_config.lock().await;
        match guard.as_ref() {
            Some(cfg) => (cfg.secrets.redirect_uri.clone(), cfg.overlay_port),
            None => ("http://localhost:7373/callback".to_string(), 7373u16),
        }
    };
    let body = render_setup_page(&redirect_uri, port, None, false);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn setup_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    // Origin/Host CSRF guard: a malicious browser tab on the user's machine
    // could otherwise POST to 127.0.0.1:{port}/setup and overwrite credentials.
    // Require Host to match {localhost|127.0.0.1}:{port} and, if Origin is
    // present, require it to match the same.
    if let Err(msg) = check_origin(&headers, state.bind_port) {
        tracing::warn!("setup: rejecting POST with bad origin/host: {msg}");
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Invalid origin",
        )
            .into_response();
    }

    // Validation: non-empty secrets, parseable URI, port in [1024, 65535].
    if form.client_id.trim().is_empty() {
        return setup_validation_error(&state, "Application ID is required.").await;
    }
    if form.client_secret.trim().is_empty() {
        return setup_validation_error(&state, "Client Secret is required.").await;
    }
    // Reject control characters in any free-text field. \r and \n can break
    // out of HTML attributes / log lines on the re-render path; \0 confuses
    // C-string-style downstream consumers.
    if has_control_chars(&form.client_id)
        || has_control_chars(&form.client_secret)
        || has_control_chars(&form.redirect_uri)
    {
        return setup_validation_error(
            &state,
            "Form fields must not contain control characters (CR, LF, NUL).",
        )
        .await;
    }
    let parsed_redirect = match reqwest::Url::parse(form.redirect_uri.trim()) {
        Ok(u) => u,
        Err(_) => {
            return setup_validation_error(&state, "Redirect URI is not a valid URL.").await;
        }
    };
    // Reject `file://`, `javascript:`, `data:`, etc. We only ever round-trip
    // an HTTP(S) redirect through the OAuth flow.
    let scheme = parsed_redirect.scheme();
    if scheme != "http" && scheme != "https" {
        return setup_validation_error(
            &state,
            "Redirect URI must use http or https scheme.",
        )
        .await;
    }
    if !(1024..=65535).contains(&form.overlay_port) {
        return setup_validation_error(&state, "Overlay Port must be between 1024 and 65535.")
            .await;
    }

    // Serialize the entire validate -> save -> snapshot-update window so two
    // concurrent submitters can not race on the `.tmp` file or produce torn
    // `current_config` reads.
    let _save_guard = state.save_lock.lock().await;

    // Preserve any existing token_storage_dir + log_level from the current
    // config; otherwise start with sensible defaults. Also snapshot the
    // previous client_secret so we can detect rotation and invalidate the
    // cached OAuth token below.
    let (token_storage_dir, log_level, prev_secret_opt) = {
        let guard = state.current_config.lock().await;
        match guard.as_ref() {
            Some(cfg) => (
                cfg.token_storage_dir.clone(),
                cfg.log_level.clone(),
                Some(cfg.secrets.client_secret.clone()),
            ),
            None => (None, "info".to_string(), None),
        }
    };

    let new_cfg = Config {
        overlay_port: form.overlay_port,
        token_storage_dir,
        log_level,
        secrets: Secrets {
            client_id: form.client_id.trim().to_string(),
            client_secret: form.client_secret,
            redirect_uri: form.redirect_uri.trim().to_string(),
        },
    };

    // If the client_secret changed (or this is a fresh install with no prior
    // config), drop the cached OAuth token. A token issued under the previous
    // secret will not authenticate against the new one.
    let secret_rotated = match &prev_secret_opt {
        Some(prev) => prev != &new_cfg.secrets.client_secret,
        None => true,
    };
    if secret_rotated {
        let token_path = new_cfg.resolved_token_storage_dir().join("token.json");
        // Best-effort; ENOENT is fine.
        let _ = tokio::fs::remove_file(&token_path).await;
        tracing::info!(
            "setup: client_secret rotation detected; cleared cached token at {}",
            token_path.display()
        );
    }

    if let Err(e) = new_cfg.save(&state.config_path).await {
        tracing::error!("setup: failed to save config: {e}");
        return setup_validation_error(
            &state,
            &format!("Failed to save configuration: {e}"),
        )
        .await;
    }

    // Update in-memory snapshot so a follow-up GET /setup pre-fills correctly.
    {
        let mut guard = state.current_config.lock().await;
        *guard = Some(new_cfg.clone());
    }

    tracing::info!(
        "setup: config saved to {}",
        state.config_path.display()
    );

    let body = render_setup_page(
        &new_cfg.secrets.redirect_uri,
        new_cfg.overlay_port,
        None,
        true,
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Returns `true` if `s` contains any of: CR, LF, or NUL. These can break
/// HTML attribute escaping in the form re-render and log-line integrity.
fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| c == '\r' || c == '\n' || c == '\0')
}

/// Lightweight CSRF defense: require the `Host` header (and `Origin`, when
/// present) to match `{localhost|127.0.0.1}:{port}`. Returns Err with a
/// reason string when the headers don't pass.
fn check_origin(headers: &HeaderMap, port: u16) -> Result<(), String> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "missing Host header".to_string())?;
    if !is_local_host(host, port) {
        return Err(format!("unexpected Host: {host}"));
    }
    if let Some(origin_val) = headers.get(header::ORIGIN) {
        let origin = origin_val
            .to_str()
            .map_err(|_| "Origin header is not valid UTF-8".to_string())?;
        let parsed = reqwest::Url::parse(origin)
            .map_err(|e| format!("Origin not parseable: {e}"))?;
        let origin_host = parsed
            .host_str()
            .ok_or_else(|| "Origin has no host".to_string())?;
        let origin_port = parsed.port().unwrap_or_else(|| match parsed.scheme() {
            "https" => 443,
            _ => 80,
        });
        if origin_port != port || (origin_host != "localhost" && origin_host != "127.0.0.1") {
            return Err(format!("unexpected Origin: {origin}"));
        }
    }
    Ok(())
}

fn is_local_host(host: &str, port: u16) -> bool {
    let expected_localhost = format!("localhost:{port}");
    let expected_loopback = format!("127.0.0.1:{port}");
    host == expected_localhost || host == expected_loopback
}

async fn setup_validation_error(state: &AppState, msg: &str) -> Response {
    let (redirect_uri, port) = {
        let guard = state.current_config.lock().await;
        match guard.as_ref() {
            Some(cfg) => (cfg.secrets.redirect_uri.clone(), cfg.overlay_port),
            None => ("http://localhost:7373/callback".to_string(), 7373u16),
        }
    };
    let body = render_setup_page(&redirect_uri, port, Some(msg), false);
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Bind and serve until `cancel` fires, then drain gracefully.
pub async fn serve(
    addr: SocketAddr,
    state: AppState,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("http: listening on http://{}", addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            cancel.cancelled().await;
            tracing::info!("http: cancellation received; draining");
        })
        .await
}

/// Re-export so callers can build `AppState` without a separate import.
pub fn make_state(
    store: Arc<OverlayStore>,
    setup_mode: bool,
    current_config: Option<Config>,
    config_path: PathBuf,
    bind_port: u16,
) -> AppState {
    AppState {
        store,
        setup_mode: Arc::new(AtomicBool::new(setup_mode)),
        current_config: Arc::new(Mutex::new(current_config)),
        config_path: Arc::new(config_path),
        save_lock: Arc::new(Mutex::new(())),
        bind_port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn redirect_uri_scheme_accepts_http_and_https() {
        for ok in &["http://localhost:7373/cb", "https://example.com/cb"] {
            let parsed = reqwest::Url::parse(ok).expect("parse");
            let s = parsed.scheme();
            assert!(s == "http" || s == "https", "{ok} unexpectedly rejected");
        }
    }

    #[test]
    fn redirect_uri_scheme_rejects_dangerous_schemes() {
        for bad in &[
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "ftp://example.com/",
        ] {
            // Some of these may fail to parse; if they do parse, the scheme
            // must NOT be http(s).
            if let Ok(parsed) = reqwest::Url::parse(bad) {
                let s = parsed.scheme();
                assert!(
                    s != "http" && s != "https",
                    "{bad} should be rejected by scheme check"
                );
            }
        }
    }

    #[test]
    fn has_control_chars_detects_cr_lf_nul() {
        assert!(has_control_chars("foo\rbar"));
        assert!(has_control_chars("foo\nbar"));
        assert!(has_control_chars("foo\0bar"));
        assert!(!has_control_chars("foo bar"));
        assert!(!has_control_chars("plain-ascii-12345"));
        // Tabs are NOT in the rejection set; they don't break attribute
        // escaping or log-line integrity.
        assert!(!has_control_chars("foo\tbar"));
    }

    #[test]
    fn check_origin_accepts_localhost_host_no_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("localhost:7373"));
        check_origin(&h, 7373).expect("must accept");
    }

    #[test]
    fn check_origin_accepts_loopback_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7373"));
        check_origin(&h, 7373).expect("must accept");
    }

    #[test]
    fn check_origin_rejects_missing_host() {
        let h = HeaderMap::new();
        check_origin(&h, 7373).expect_err("must reject");
    }

    #[test]
    fn check_origin_rejects_wrong_port() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("localhost:9999"));
        check_origin(&h, 7373).expect_err("must reject");
    }

    #[test]
    fn check_origin_rejects_offsite_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("localhost:7373"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example.com"),
        );
        check_origin(&h, 7373).expect_err("must reject offsite Origin");
    }

    #[test]
    fn check_origin_accepts_matching_origin() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("localhost:7373"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:7373"),
        );
        check_origin(&h, 7373).expect("must accept");
    }
}

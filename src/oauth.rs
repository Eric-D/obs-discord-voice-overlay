//! OAuth flow over Discord RPC IPC.
//!
//! Steps:
//! 1. AUTHORIZE via IPC -> Discord shows consent prompt -> returns one-shot `code`.
//! 2. POST to `https://discord.com/api/oauth2/token` (`grant_type=authorization_code`)
//!    with `client_id` + `client_secret` + `code` + `redirect_uri`.
//! 3. AUTHENTICATE via IPC with the returned `access_token`.
//!
//! Tokens are cached to `{TOKEN_STORAGE_DIR}/token.json`, mode 0600 on Unix.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ipc::{IpcClient, IpcError};

const TOKEN_FILENAME: &str = "token.json";
const TOKEN_TMP_FILENAME: &str = "token.json.tmp";
const DISCORD_TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
/// User has 120s to click Authorize; otherwise we surface a clear timeout
/// rather than blocking the connection driver indefinitely.
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// HTTP timeout shared by `exchange_code` and `refresh_token` so a hung
/// Discord endpoint doesn't wedge the OAuth flow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Process-wide reqwest::Client — constructed once with a 15s timeout and
/// shared between OAuth calls so we don't pay the cost of building a new
/// client (and TLS state) per request.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("build reqwest client")
    })
}

/// Runtime OAuth configuration. Sourced from the persistent on-disk
/// [`crate::config::Config`] (sensitive trio + token storage dir) plus the
/// scopes the binary needs.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub token_storage_dir: PathBuf,
    /// Scopes to request via AUTHORIZE.
    pub scopes: Vec<String>,
}

impl OAuthConfig {
    /// Build from the decrypted persistent config + the scopes we want.
    pub fn from_config(cfg: &crate::config::Config, scopes: Vec<String>) -> Self {
        Self {
            client_id: cfg.secrets.client_id.clone(),
            client_secret: cfg.secrets.client_secret.clone(),
            redirect_uri: cfg.secrets.redirect_uri.clone(),
            token_storage_dir: cfg.resolved_token_storage_dir(),
            scopes,
        }
    }
}

/// Persisted token bundle. `expires_at` is unix-seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub scope: String,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("discord refused authorize: {0}")]
    AuthorizeRefused(String),
    #[error("token response missing field: {0}")]
    MalformedTokenResponse(&'static str),
    /// User did not consent within [`AUTHORIZE_TIMEOUT`].
    #[error("AUTHORIZE timed out waiting for user consent")]
    AuthorizeTimeout,
    /// User clicked Cancel on the consent prompt — surfaced as a hard fail so
    /// the binary can exit non-zero rather than loop.
    #[error("user denied authorization")]
    UserDenied,
}

fn token_path(dir: &Path) -> PathBuf {
    dir.join(TOKEN_FILENAME)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load a previously persisted token, if present. A corrupt token file is
/// treated as absent: the caller falls back to the AUTHORIZE flow rather than
/// blocking startup over a stale half-written file.
pub async fn load_token(dir: &Path) -> Result<Option<StoredToken>, OAuthError> {
    let path = token_path(dir);
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<StoredToken>(&bytes) {
            Ok(parsed) => Ok(Some(parsed)),
            Err(e) => {
                tracing::warn!("token file corrupt, falling back to AUTHORIZE: {e}");
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete the cached token file if it exists. Used when a stored token is
/// known-bad (rejected by AUTHENTICATE + refresh both failed) so the next run
/// starts clean.
pub async fn delete_token(dir: &Path) -> Result<(), OAuthError> {
    let path = token_path(dir);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Persist a token bundle to disk atomically and with mode 0600 from the
/// moment it exists (closing the TOCTOU window that `tokio::fs::write` leaves
/// open between create + chmod).
///
/// Strategy: write to `token.json.tmp` with `O_CREAT|O_EXCL|mode=0o600`,
/// `write_all`, `sync_data`, then `rename` to `token.json`. If a stale temp
/// file is left behind from a previous crash we clear it first.
pub async fn save_token(dir: &Path, token: &StoredToken) -> Result<(), OAuthError> {
    tokio::fs::create_dir_all(dir).await?;
    let final_path = token_path(dir);
    let tmp_path = dir.join(TOKEN_TMP_FILENAME);
    let bytes = serde_json::to_vec_pretty(token)?;

    // Drop any stale temp from a crashed previous run so create_new succeeds.
    match tokio::fs::remove_file(&tmp_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let tmp_for_blocking = tmp_path.clone();
    let bytes_for_blocking = bytes.clone();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp_for_blocking)?;
        f.write_all(&bytes_for_blocking)?;
        f.sync_data()?;
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)??;

    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(())
}

/// Run the AUTHORIZE -> token exchange flow, returning a fresh [`StoredToken`].
/// The user must click "Authorize" in the Discord client; we cap the wait at
/// [`AUTHORIZE_TIMEOUT`] so a forgotten consent prompt doesn't hang us forever.
pub async fn run_authorize_flow(
    ipc: &IpcClient,
    cfg: &OAuthConfig,
) -> Result<StoredToken, OAuthError> {
    tracing::info!("oauth: requesting AUTHORIZE — please consent in Discord");
    let resp = match tokio::time::timeout(
        AUTHORIZE_TIMEOUT,
        ipc.command(
            "AUTHORIZE",
            json!({
                "client_id": cfg.client_id,
                "scopes": cfg.scopes,
            }),
        ),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(IpcError::Remote(msg))) => {
            // Discord returns Remote(...) on user cancel — surface it as a
            // distinct denial so callers can exit non-zero per the I/O matrix.
            tracing::error!("oauth: AUTHORIZE refused by Discord/user: {msg}");
            return Err(OAuthError::UserDenied);
        }
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(OAuthError::AuthorizeTimeout),
    };

    let code = resp
        .get("data")
        .and_then(|d| d.get("code"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| OAuthError::AuthorizeRefused("no code in AUTHORIZE response".into()))?
        .to_string();

    exchange_code(cfg, &code).await
}

async fn post_token_form(form: &[(&str, &str)]) -> Result<Value, OAuthError> {
    let resp = http_client().post(DISCORD_TOKEN_URL).form(form).send().await?;
    let status = resp.status();
    if !status.is_success() {
        // Capture the response body so Discord OAuth error codes
        // (`invalid_grant`, `invalid_client`, etc.) make it into logs.
        let body = resp.text().await.unwrap_or_else(|e| format!("<body read failed: {e}>"));
        return Err(OAuthError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }
    let v: Value = resp.json().await?;
    Ok(v)
}

/// Exchange a one-shot `code` for an access/refresh token bundle.
pub async fn exchange_code(cfg: &OAuthConfig, code: &str) -> Result<StoredToken, OAuthError> {
    let resp = post_token_form(&[
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", cfg.client_secret.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", cfg.redirect_uri.as_str()),
    ])
    .await?;
    // No previous refresh_token on first exchange.
    parse_token_response(&resp, None)
}

/// Use a refresh token to mint a new access/refresh pair. If Discord omits
/// `refresh_token` from the response (as some flows do), the previous token
/// is carried over so subsequent refreshes keep working.
pub async fn refresh_token(
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> Result<StoredToken, OAuthError> {
    let resp = post_token_form(&[
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", cfg.client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ])
    .await?;
    parse_token_response(&resp, Some(refresh_token))
}

fn parse_token_response(
    resp: &Value,
    previous_refresh: Option<&str>,
) -> Result<StoredToken, OAuthError> {
    let access_token = resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or(OAuthError::MalformedTokenResponse("access_token"))?
        .to_string();
    let refresh = match resp.get("refresh_token").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => previous_refresh
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_default(),
    };
    let expires_in = resp
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .ok_or(OAuthError::MalformedTokenResponse("expires_in"))?;
    let scope = resp
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(StoredToken {
        access_token,
        refresh_token: refresh,
        // saturating_add: if Discord ever returns a wild expires_in, prefer
        // saturation to wrapping back to 0 (which would mark the token as
        // immediately expired).
        expires_at: now_secs().saturating_add(expires_in),
        scope,
    })
}

/// Send AUTHENTICATE on the IPC connection, returning the response.
pub async fn authenticate(ipc: &IpcClient, access_token: &str) -> Result<Value, OAuthError> {
    Ok(ipc
        .command("AUTHENTICATE", json!({ "access_token": access_token }))
        .await?)
}

/// Returns true if the token has fewer than 60 seconds of life left.
pub fn is_expired(token: &StoredToken) -> bool {
    token.expires_at <= now_secs() + 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_response_carries_over_refresh_when_missing() {
        let resp = json!({
            "access_token": "new-access",
            "expires_in": 3600,
            "scope": "rpc",
            // refresh_token intentionally absent
        });
        let parsed = parse_token_response(&resp, Some("old-refresh")).unwrap();
        assert_eq!(parsed.access_token, "new-access");
        assert_eq!(parsed.refresh_token, "old-refresh");
    }

    #[test]
    fn parse_token_response_uses_new_refresh_when_present() {
        let resp = json!({
            "access_token": "a",
            "refresh_token": "fresh",
            "expires_in": 60,
        });
        let parsed = parse_token_response(&resp, Some("stale")).unwrap();
        assert_eq!(parsed.refresh_token, "fresh");
    }

    #[test]
    fn parse_token_response_missing_expires_in_errors() {
        let resp = json!({ "access_token": "a", "refresh_token": "r" });
        let err = parse_token_response(&resp, None).unwrap_err();
        assert!(matches!(
            err,
            OAuthError::MalformedTokenResponse("expires_in")
        ));
    }

    #[test]
    fn parse_token_response_saturating_add_does_not_overflow() {
        // expires_in == u64::MAX must saturate to u64::MAX, not wrap to 0.
        let resp = json!({
            "access_token": "a",
            "refresh_token": "r",
            "expires_in": u64::MAX,
        });
        let parsed = parse_token_response(&resp, None).unwrap();
        assert_eq!(parsed.expires_at, u64::MAX);
    }

    #[test]
    fn parse_token_response_missing_access_token_errors() {
        let resp = json!({ "refresh_token": "r", "expires_in": 60 });
        assert!(matches!(
            parse_token_response(&resp, None),
            Err(OAuthError::MalformedTokenResponse("access_token"))
        ));
    }
}

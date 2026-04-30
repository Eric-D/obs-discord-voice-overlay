//! Entrypoint: load persistent config, build a multi-thread tokio runtime,
//! spawn the existing IPC + HTTP + driver lifecycle on it, and run the system
//! tray on the main thread. Quit / Ctrl-C cancel a shared
//! [`tokio_util::sync::CancellationToken`] so all tasks drain cleanly.
//!
//! On Windows release builds the console window is hidden via
//! `windows_subsystem = "windows"`; debug builds keep the console for dev
//! ergonomics.
//!
//! When the on-disk config is missing, decrypts incorrectly, or the host's
//! machine UID is unreadable, the binary enters **setup mode**: only `/setup`
//! is served (everything else 503s), IPC + OAuth do NOT start, the tray icon
//! goes amber, and the default browser auto-opens to `/setup` on first launch.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod autostart;
mod config;
mod events;
mod icons;
mod ipc;
mod oauth;
mod state;
mod themes;
mod tray;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ConfigError};
use crate::events::{
    get_channel, get_selected_voice_channel, subscribe_channel_select, subscribe_voice_channel,
    unsubscribe_voice_channel, ChannelSelect, DiscordUser, Speaking, VoiceFlags, VoiceState,
};
use crate::ipc::{spawn_ipc_loop, IpcClient, IpcError, IpcEvent, IpcStatus};
use crate::oauth::{
    authenticate, delete_token, is_expired, load_token, refresh_token, run_authorize_flow,
    save_token, OAuthConfig, OAuthError, StoredToken,
};
use crate::state::{participant_from, OverlayStore, Participant};

/// Default overlay port used while in setup mode (no config to read it from).
const DEFAULT_OVERLAY_PORT: u16 = 7373;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Build the runtime explicitly so we can keep the main thread free for
    // the platform UI event loop (see `tray::run_tray`).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let config_path = match config::default_path() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("config: cannot determine config path: {e}");
            return Err(format!("cannot determine config path: {e}").into());
        }
    };
    let load_result = rt.block_on(config::load(&config_path));

    // Treat any failure that means "we can no longer trust the cached token"
    // as a token-invalidation trigger. Decrypt failure is the obvious one; a
    // ParseJson or UnsupportedVersion result means the file on disk is
    // unintelligible to us, and the cached token may bind to a different
    // (now-unknown) client_secret. Better to force a fresh AUTHORIZE than to
    // risk silently authenticating against the wrong app.
    let (initial_config, setup_mode, token_invalidate) = match &load_result {
        Ok(Some(cfg)) => (Some(cfg.clone()), false, false),
        Ok(None) => {
            tracing::warn!(
                "config: no persistent config at {} - entering setup mode",
                config_path.display()
            );
            (None, true, false)
        }
        Err(ConfigError::Decrypt) => {
            tracing::warn!(
                "config: decrypt failed at {} - entering setup mode (cached token will be cleared)",
                config_path.display()
            );
            (None, true, true)
        }
        Err(ConfigError::ParseJson(msg)) => {
            tracing::warn!(
                "config: JSON parse failed at {} ({msg}) - entering setup mode (cached token will be cleared)",
                config_path.display()
            );
            (None, true, true)
        }
        Err(ConfigError::UnsupportedVersion(v)) => {
            tracing::warn!(
                "config: unsupported version {v} at {} - entering setup mode (cached token will be cleared)",
                config_path.display()
            );
            (None, true, true)
        }
        Err(e) => {
            tracing::warn!(
                "config: failed to load {} ({e}) - entering setup mode",
                config_path.display()
            );
            (None, true, false)
        }
    };

    // On any "config now untrusted" failure: invalidate the cached OAuth
    // token so the next successful setup doesn't try to authenticate with a
    // token issued for a (potentially) different client_secret.
    if token_invalidate {
        let token_dir = config::default_token_storage_dir();
        let token_path = token_dir.join("token.json");
        let _ = rt.block_on(tokio::fs::remove_file(&token_path));
    }

    let port = initial_config
        .as_ref()
        .map(|c| c.overlay_port)
        .unwrap_or(DEFAULT_OVERLAY_PORT);

    let cancel = CancellationToken::new();
    let store = Arc::new(state::OverlayStore::new());
    if setup_mode {
        store.set_needs_setup(true);
    }
    let tray_rx = store.subscribe_tray_state();

    // Spawn the existing async lifecycle on the runtime. It honors the
    // shared cancellation token for graceful shutdown.
    let async_cancel = cancel.clone();
    let async_store = store.clone();
    let async_config_path = config_path.clone();
    let async_initial_config = initial_config.clone();
    // Set when the async runtime exits with an error (e.g. HTTP failed to
    // bind). The main thread reads this after the tray loop returns to
    // decide whether to exit non-zero.
    let runtime_failed = Arc::new(AtomicBool::new(false));
    let runtime_failed_for_async = runtime_failed.clone();
    let _join = rt.spawn(async move {
        if let Err(e) = run(
            async_initial_config,
            async_config_path,
            setup_mode,
            async_store,
            async_cancel.clone(),
        )
        .await
        {
            tracing::error!("async runtime exited with error: {e}");
            runtime_failed_for_async.store(true, Ordering::Release);
            async_cancel.cancel();
        }
    });

    // Forward Ctrl-C (debug ergonomics) into the same cancel token so the
    // shutdown path is identical to the tray "Quit" item.
    let signal_cancel = cancel.clone();
    rt.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutdown: ctrl-c received");
            signal_cancel.cancel();
        }
    });

    // Auto-open the browser on first transition into setup mode (only once
    // ever, tracked via a marker file).
    if setup_mode {
        // Marker invariant: if the config file is missing entirely (e.g. user
        // deleted a corrupt config to force re-setup), drop the marker too so
        // we re-open the browser on this launch.
        let config_exists = std::path::Path::new(&config_path).exists();
        let marker = config::setup_prompt_marker_path();
        if !config_exists {
            let _ = std::fs::remove_file(&marker);
        }
        let already_prompted = std::path::Path::new(&marker).exists();
        if !already_prompted {
            let url = format!("http://localhost:{port}/setup");
            tracing::info!("setup: auto-opening browser to {url}");
            let url_for_thread = url.clone();
            std::thread::spawn(move || {
                // Tiny grace so the HTTP listener has a chance to bind.
                std::thread::sleep(Duration::from_millis(500));
                if let Err(e) = open::that(&url_for_thread) {
                    tracing::warn!(
                        "setup: failed to auto-open browser to {url_for_thread}: {e}"
                    );
                }
            });
            // Best-effort marker write; never block startup on it.
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, b"1");
        } else {
            tracing::info!(
                "setup: marker {} exists; skipping browser auto-open",
                marker.display()
            );
        }
    } else {
        // Clean up the marker once we have a working config so that a future
        // re-entry into setup mode (e.g. after machine migration) re-prompts.
        let marker = config::setup_prompt_marker_path();
        let _ = std::fs::remove_file(&marker);
    }

    // Run the tray UI on the main thread. Returns when the user clicks Quit.
    let rt_handle = rt.handle().clone();
    if let Err(e) = tray::run_tray(tray_rx, port, cancel.clone(), rt_handle) {
        tracing::error!("tray exited with error: {e}");
        cancel.cancel();
    }

    // Graceful drain budget; matches the spec's "force exit 0 after 5s".
    rt.shutdown_timeout(Duration::from_secs(5));
    if runtime_failed.load(Ordering::Acquire) {
        // Non-zero exit so launchers / autostart can detect a hard failure
        // (e.g. port conflict in setup mode).
        std::process::exit(1);
    }
    Ok(())
}

/// The previous `#[tokio::main]` body, lifted into a function the runtime
/// can spawn. Honors `cancel` for graceful shutdown of HTTP + driver.
///
/// In setup mode (`setup_mode == true`), only the HTTP server is started —
/// the IPC + OAuth driver task is skipped entirely until the user fills out
/// `/setup` and restarts.
async fn run(
    initial_config: Option<Config>,
    config_path: PathBuf,
    setup_mode: bool,
    store: Arc<OverlayStore>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = initial_config
        .as_ref()
        .map(|c| c.overlay_port)
        .unwrap_or(DEFAULT_OVERLAY_PORT);

    let app_state = web::make_state(
        store.clone(),
        setup_mode,
        initial_config.clone(),
        config_path.clone(),
        port,
    );

    // HTTP server with cancellation-aware graceful shutdown.
    let http_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let http_cancel = cancel.clone();
    let http_handle: JoinHandle<std::io::Result<()>> =
        tokio::spawn(async move { web::serve(http_addr, app_state, http_cancel).await });

    // Driver task: only spawned when we have a usable config. In setup mode
    // we just sit on HTTP until the user submits /setup and restarts.
    let mut driver_handle: Option<JoinHandle<()>> = None;
    if !setup_mode {
        let cfg = initial_config
            .clone()
            .expect("setup_mode==false implies a loaded config");
        let oauth_cfg = OAuthConfig::from_config(
            &cfg,
            vec!["rpc".to_string(), "rpc.voice.read".to_string()],
        );

        // IPC reconnect loop.
        let ipc_status_rx = spawn_ipc_loop(cfg.secrets.client_id.clone());

        let store_for_driver = store.clone();
        let driver_cancel = cancel.clone();
        driver_handle = Some(tokio::spawn(async move {
            driver_loop(ipc_status_rx, store_for_driver, oauth_cfg, driver_cancel).await
        }));
    } else {
        tracing::info!("run: setup mode active; IPC + OAuth driver not started");
    }

    let outcome: Result<(), Box<dyn std::error::Error + Send + Sync>> = tokio::select! {
        _ = cancel.cancelled() => {
            tracing::info!("shutdown: cancellation received");
            Ok(())
        }
        result = http_handle => {
            // Distinguish "HTTP died after running" from "HTTP never bound".
            // In setup mode the latter is especially bad: the user sees an
            // amber tray and an unreachable URL with no obvious cause.
            let mode = if setup_mode { "setup mode" } else { "normal mode" };
            match &result {
                Ok(Ok(())) => {
                    tracing::error!(
                        "HTTP server ({mode}) stopped unexpectedly with Ok at {http_addr}"
                    );
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        "HTTP server ({mode}) failed to bind/serve at {http_addr}: {e}"
                    );
                    if setup_mode {
                        // Stay on NeedsSetup so the tray still surfaces the
                        // "config required" state; without HTTP the user can't
                        // actually reach /setup, but at least the tooltip and
                        // the explicit log line tell them why.
                        store.set_needs_setup(true);
                    }
                }
                Err(join_err) => {
                    tracing::error!(
                        "HTTP server ({mode}) task panicked at {http_addr}: {join_err}"
                    );
                }
            }
            cancel.cancel();
            Err(format!("http server stopped at {http_addr}").into())
        }
    };
    // Drain the driver — it observes the cancel token directly. Abort if
    // it doesn't exit promptly so we don't hang shutdown.
    if let Some(mut h) = driver_handle {
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut h).await;
        if !h.is_finished() {
            h.abort();
        }
    }
    outcome
}

/// Reacts to IPC connect/disconnect lifecycle. On every fresh connection, runs
/// the auth handshake, subscribes to events, and forwards them into [`OverlayStore`].
///
/// Uses `watch::Receiver` instead of `broadcast::Receiver` so we never miss a
/// `Connected` -> `Disconnected` transition due to lag.
async fn driver_loop(
    mut status_rx: watch::Receiver<IpcStatus>,
    store: Arc<OverlayStore>,
    cfg: OAuthConfig,
    cancel: CancellationToken,
) {
    // Track the current event-handler task so we can cancel it before
    // spawning a fresh one on reconnect.
    let mut current_event_task: Option<JoinHandle<()>> = None;

    // Apply the initial state once; then watch transitions.
    loop {
        let status = status_rx.borrow_and_update().clone();
        match status {
            IpcStatus::Connected(client) => {
                store.set_connected(true);
                if let Some(prev) = current_event_task.take() {
                    prev.abort();
                }
                match on_connected(&client, &store, &cfg).await {
                    Ok(handle) => current_event_task = Some(handle),
                    Err(e) => {
                        tracing::error!("driver: connection setup failed: {e}");
                    }
                }
            }
            IpcStatus::Disconnected => {
                store.set_connected(false);
                if let Some(prev) = current_event_task.take() {
                    prev.abort();
                }
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("driver: cancellation received; exiting");
                if let Some(prev) = current_event_task.take() {
                    prev.abort();
                }
                return;
            }
            res = status_rx.changed() => {
                if res.is_err() {
                    tracing::error!("driver: ipc status channel closed; exiting");
                    return;
                }
            }
        }
    }
}

/// Sets up auth + subscriptions on a freshly connected IPC client and spawns
/// the event handler. Returns the spawned task's [`JoinHandle`] so the driver
/// can abort it on the next reconnect.
async fn on_connected(
    client: &IpcClient,
    store: &Arc<OverlayStore>,
    cfg: &OAuthConfig,
) -> Result<JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
    // 1) Authenticate.
    let token = ensure_authenticated(client, cfg).await?;
    let oauth_state = Arc::new(OAuthState::new(cfg.clone(), token.clone()));
    tracing::info!(
        "ipc: authenticated; scope={:?} expires_at={}",
        token.scope,
        token.expires_at
    );

    // 2) Subscribe to events FIRST so we don't miss the first VOICE_STATE_*
    //    that may fire as a side-effect of subscribe_channel_select / load.
    subscribe_channel_select(client).await?;

    // 3) Determine current channel + load participants.
    let current = get_selected_voice_channel(client).await.ok();
    let initial_channel: Option<String> = current
        .as_ref()
        .and_then(|v| v.get("data"))
        .and_then(|d| d.get("id"))
        .and_then(|i| i.as_str())
        .map(|s| s.to_string());
    if let Some(channel_id) = initial_channel.as_deref() {
        load_channel(client, store, channel_id).await?;
    } else {
        store.set_channel(None, vec![]);
    }

    // 4) Spawn the event handler — runs until the IPC connection drops or
    //    the next reconnect aborts it.
    let client_clone = client.clone();
    let store_clone = store.clone();
    let oauth_state_clone = oauth_state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = handle_events(client_clone, store_clone, oauth_state_clone).await {
            tracing::warn!("driver: event handler exit: {e}");
        }
    });
    Ok(handle)
}

/// Mutable OAuth state shared between the connection setup path and the
/// event-handler retry helper. The token is wrapped in a [`Mutex`] because
/// multiple commands may concurrently try to refresh after a 401-equivalent.
struct OAuthState {
    cfg: OAuthConfig,
    token: Mutex<StoredToken>,
}

impl OAuthState {
    fn new(cfg: OAuthConfig, token: StoredToken) -> Self {
        Self {
            cfg,
            token: Mutex::new(token),
        }
    }
}

/// Loads a token from disk if possible, refreshes if expired, and falls back
/// to AUTHORIZE if neither works. Always finishes with an AUTHENTICATE call.
///
/// On AUTHENTICATE failure with a non-expired stored token we now try
/// `refresh_token` first before falling through to AUTHORIZE — and if we do
/// fall through, the bad cached file is deleted so the next run starts clean.
async fn ensure_authenticated(
    client: &IpcClient,
    cfg: &OAuthConfig,
) -> Result<StoredToken, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(stored) = load_token(&cfg.token_storage_dir).await? {
        let token = if is_expired(&stored) {
            tracing::info!("oauth: cached token expired; refreshing");
            match refresh_token(cfg, &stored.refresh_token).await {
                Ok(t) => {
                    save_token(&cfg.token_storage_dir, &t).await?;
                    t
                }
                Err(e) => {
                    tracing::warn!("oauth: refresh failed ({e}); falling back to AUTHORIZE");
                    return authorize_and_save(client, cfg).await;
                }
            }
        } else {
            stored.clone()
        };
        match authenticate(client, &token.access_token).await {
            Ok(_) => return Ok(token),
            Err(e) => {
                tracing::warn!(
                    "oauth: AUTHENTICATE rejected token ({e}); attempting refresh before re-AUTHORIZE"
                );
                if !token.refresh_token.is_empty() {
                    match refresh_token(cfg, &token.refresh_token).await {
                        Ok(refreshed) => {
                            if save_token(&cfg.token_storage_dir, &refreshed).await.is_err() {
                                tracing::warn!("oauth: failed to persist refreshed token");
                            }
                            match authenticate(client, &refreshed.access_token).await {
                                Ok(_) => return Ok(refreshed),
                                Err(e2) => {
                                    tracing::warn!(
                                        "oauth: AUTHENTICATE rejected refreshed token ({e2}); re-running AUTHORIZE"
                                    );
                                }
                            }
                        }
                        Err(re) => {
                            tracing::warn!(
                                "oauth: refresh after AUTHENTICATE failure failed ({re}); re-running AUTHORIZE"
                            );
                        }
                    }
                }
                // Bad cached token: delete it so next run starts clean.
                if let Err(de) = delete_token(&cfg.token_storage_dir).await {
                    tracing::warn!("oauth: failed to delete bad cached token: {de}");
                }
            }
        }
    }
    authorize_and_save(client, cfg).await
}

/// Runs AUTHORIZE, persists, and AUTHENTICATEs. User denial is fatal — we
/// surface a clear error and exit non-zero rather than looping forever.
async fn authorize_and_save(
    client: &IpcClient,
    cfg: &OAuthConfig,
) -> Result<StoredToken, Box<dyn std::error::Error + Send + Sync>> {
    let fresh = match run_authorize_flow(client, cfg).await {
        Ok(t) => t,
        Err(OAuthError::UserDenied) => {
            tracing::error!("user denied authorization, exiting");
            std::process::exit(1);
        }
        Err(OAuthError::AuthorizeTimeout) => {
            tracing::error!("AUTHORIZE timed out — user did not consent in 120s, exiting");
            std::process::exit(1);
        }
        Err(e) => return Err(Box::new(e)),
    };
    save_token(&cfg.token_storage_dir, &fresh).await?;
    authenticate(client, &fresh.access_token).await?;
    Ok(fresh)
}

async fn load_channel(
    client: &IpcClient,
    store: &Arc<OverlayStore>,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = get_channel(client, channel_id).await?;
    let voice_states = resp
        .get("data")
        .and_then(|d| d.get("voice_states"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut participants: Vec<Participant> = Vec::with_capacity(voice_states.len());
    for vs in voice_states {
        if let Some(p) = participant_from_value(&vs) {
            participants.push(p);
        }
    }
    store.set_channel(Some(channel_id.to_string()), participants);
    subscribe_voice_channel(client, channel_id).await?;
    Ok(())
}

fn participant_from_value(v: &Value) -> Option<Participant> {
    let user_val = match v.get("user") {
        Some(u) => u.clone(),
        None => {
            tracing::warn!("participant_from_value: missing `user` field; payload={v}");
            return None;
        }
    };
    let user: DiscordUser = match serde_json::from_value(user_val) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("participant_from_value: failed to decode user ({e}); payload={v}");
            return None;
        }
    };
    let flags: VoiceFlags = match v.get("voice_state").cloned() {
        Some(vs) => match serde_json::from_value(vs) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "participant_from_value: failed to decode voice_state ({e}); payload={v}"
                );
                VoiceFlags::default()
            }
        },
        None => VoiceFlags::default(),
    };
    let nick = v.get("nick").and_then(|n| n.as_str());
    Some(participant_from(&user, nick, &flags, false))
}

/// Detect whether an [`IpcError::Remote`] message indicates the access token
/// is no longer valid. Discord RPC returns these codes/messages on AUTHENTICATE
/// + per-command failures when the token has expired or been revoked.
fn is_token_invalid_error(err: &IpcError) -> bool {
    if let IpcError::Remote(msg) = err {
        let m = msg.to_ascii_lowercase();
        // 4002 / 4009 / generic "invalid token" / "oauth" wording.
        m.contains("invalid token")
            || m.contains("oauth2 token")
            || m.contains("oauth token")
            || m.contains("4002")
            || m.contains("4009")
    } else {
        false
    }
}

/// Send a command, transparently refreshing the OAuth token + re-AUTHENTICATEing
/// once if the first attempt fails with a token-invalid error. This implements
/// AC6 (mid-session 401 refresh+retry).
async fn auth_command(
    cmd: &str,
    args: Value,
    oauth: &Arc<OAuthState>,
    ipc: &IpcClient,
) -> Result<Value, IpcError> {
    match ipc.command(cmd, args.clone()).await {
        Ok(v) => Ok(v),
        Err(e) if is_token_invalid_error(&e) => {
            tracing::warn!("auth_command: {cmd} hit token-invalid ({e}); refreshing + retrying");
            // Single-flight the refresh under the token mutex.
            let mut guard = oauth.token.lock().await;
            let refresh = guard.refresh_token.clone();
            if refresh.is_empty() {
                tracing::error!("auth_command: cannot refresh — no refresh_token stored");
                return Err(e);
            }
            let refreshed = match refresh_token(&oauth.cfg, &refresh).await {
                Ok(t) => t,
                Err(re) => {
                    tracing::error!("auth_command: refresh_token failed: {re}");
                    return Err(e);
                }
            };
            if let Err(se) = save_token(&oauth.cfg.token_storage_dir, &refreshed).await {
                tracing::warn!("auth_command: failed to persist refreshed token: {se}");
            }
            *guard = refreshed.clone();
            drop(guard);
            if let Err(ae) = authenticate(ipc, &refreshed.access_token).await {
                tracing::error!("auth_command: re-AUTHENTICATE failed after refresh: {ae}");
                // Fall back to the original error; reconnect loop will recover.
                return Err(e);
            }
            ipc.command(cmd, args).await
        }
        Err(e) => Err(e),
    }
}

async fn handle_events(
    client: IpcClient,
    store: Arc<OverlayStore>,
    oauth: Arc<OAuthState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut rx = client.subscribe();
    let mut current_channel: Option<String> = store.snapshot().channel_id;
    loop {
        let event: IpcEvent = match rx.recv().await {
            Ok(e) => e,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("driver: ipc events lagged by {n}");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };
        match event.evt.as_str() {
            "VOICE_CHANNEL_SELECT" => {
                let cs: ChannelSelect = match serde_json::from_value(event.data.clone()) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "driver: malformed VOICE_CHANNEL_SELECT ({e}); payload={}",
                            event.data
                        );
                        continue;
                    }
                };
                // Same-channel select: skip the unsubscribe/resubscribe/reset
                // cycle so the overlay doesn't blink when Discord re-emits the
                // current channel.
                if cs.channel_id == current_channel {
                    tracing::debug!("driver: VOICE_CHANNEL_SELECT for current channel; no-op");
                    continue;
                }
                if let Some(prev) = current_channel.take() {
                    let _ = unsubscribe_voice_channel(&client, &prev).await;
                }
                if let Some(channel_id) = cs.channel_id.clone() {
                    if let Err(e) = load_channel_with_auth(&client, &oauth, &store, &channel_id).await {
                        tracing::warn!("driver: load_channel failed: {e}");
                    }
                    current_channel = Some(channel_id);
                } else {
                    store.set_channel(None, vec![]);
                    current_channel = None;
                }
            }
            "VOICE_STATE_CREATE" | "VOICE_STATE_UPDATE" => {
                match serde_json::from_value::<VoiceState>(event.data.clone()) {
                    Ok(vs) => {
                        let p = participant_from(
                            &vs.user,
                            vs.nick.as_deref(),
                            &vs.voice_state,
                            false,
                        );
                        store.upsert_participant(p);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "driver: malformed {} ({e}); payload={}",
                            event.evt,
                            event.data
                        );
                    }
                }
            }
            "VOICE_STATE_DELETE" => match serde_json::from_value::<VoiceState>(event.data.clone()) {
                Ok(vs) => store.remove_participant(&vs.user.id),
                Err(e) => {
                    tracing::warn!(
                        "driver: malformed VOICE_STATE_DELETE ({e}); payload={}",
                        event.data
                    );
                }
            },
            "SPEAKING_START" => match serde_json::from_value::<Speaking>(event.data.clone()) {
                Ok(s) => {
                    store.set_speaking(&s.user_id, true);
                }
                Err(e) => {
                    tracing::warn!(
                        "driver: malformed SPEAKING_START ({e}); payload={}",
                        event.data
                    );
                }
            },
            "SPEAKING_STOP" => match serde_json::from_value::<Speaking>(event.data.clone()) {
                Ok(s) => {
                    store.set_speaking(&s.user_id, false);
                }
                Err(e) => {
                    tracing::warn!(
                        "driver: malformed SPEAKING_STOP ({e}); payload={}",
                        event.data
                    );
                }
            },
            other => {
                tracing::debug!("driver: ignoring event {other}");
            }
        }
    }
}

/// Variant of [`load_channel`] that goes through [`auth_command`] for
/// GET_CHANNEL so a mid-session 401 triggers a transparent refresh + retry.
async fn load_channel_with_auth(
    client: &IpcClient,
    oauth: &Arc<OAuthState>,
    store: &Arc<OverlayStore>,
    channel_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = auth_command(
        "GET_CHANNEL",
        serde_json::json!({ "channel_id": channel_id }),
        oauth,
        client,
    )
    .await?;
    let voice_states = resp
        .get("data")
        .and_then(|d| d.get("voice_states"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut participants: Vec<Participant> = Vec::with_capacity(voice_states.len());
    for vs in voice_states {
        if let Some(p) = participant_from_value(&vs) {
            participants.push(p);
        }
    }
    store.set_channel(Some(channel_id.to_string()), participants);
    subscribe_voice_channel(client, channel_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_token_invalid_error_matches_known_strings() {
        let cases = [
            "Invalid Token",
            "OAuth2 token expired",
            "code 4002 invalid token",
            "RPC code 4009",
        ];
        for c in cases {
            assert!(
                is_token_invalid_error(&IpcError::Remote(c.into())),
                "expected token-invalid match for {c:?}"
            );
        }
    }

    #[test]
    fn is_token_invalid_error_ignores_unrelated() {
        assert!(!is_token_invalid_error(&IpcError::Remote(
            "guild not found".into()
        )));
        assert!(!is_token_invalid_error(&IpcError::Disconnected));
    }
}

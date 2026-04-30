# Deferred Work

Goals carved off from primary specs, queued for follow-up implementation cycles.

## Discord Voice Overlay — Desktop Application Layer

**Deferred from**: `spec-discord-voice-overlay.md` (split decision, 2026-04-30)

**Goal**: Turn the CLI overlay service into a polished Windows-first desktop application — system tray icon, autostart on boot, right-click menu for preview / quit / autostart toggle, console hidden on Windows release builds.

**Scope**:
- System tray icon (cross-platform; Windows priority, Linux best-effort) using `tray-icon` crate or equivalent.
- Right-click tray menu:
  - "Open overlay preview" → opens `http://localhost:{OVERLAY_PORT}/` in default browser.
  - "Auto-start on boot" → toggle, persisted across launches.
  - "Quit" → graceful shutdown of HTTP server + IPC client.
- Autostart toggle (cross-platform via `auto-launch` crate; registry on Windows, `.desktop` file on Linux).
- Hide console on Windows release builds: `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`.
- Tray icon surfaces voice connection status (connected/disconnected indicator).

**Dependencies on the parent spec**: Requires the core overlay (RPC IPC, OAuth, axum/SSE, themes) shipped and working end-to-end. The tray app wraps the existing binary lifecycle.

**Why deferred**:
- Independently shippable layer — desktop polish on top of a working CLI service.
- Validates Discord RPC + SSE + overlay end-to-end before investing in tray UI.
- Tray event loop integration with tokio adds complexity better tackled with empirical feedback from the core spec.
- Keeps the parent spec in the optimal 900-1600 token range.

**Trigger to resume**: open as a new spec via `bmad-quick-dev` after the parent spec ships and is manually validated in OBS.

## Discord Voice Overlay — Hardening backlog (post-step-04 review)

**Deferred from**: `spec-discord-voice-overlay.md` (review-driven defers, 2026-04-30)

Items below were surfaced by the three reviewer agents in step-04 but classified `defer` (not caused by current change scope, or quality-of-life polish that can ship in a follow-up).

- **IPv6 listener**: also bind `::1` on `OVERLAY_PORT` so OBS resolving `localhost` to `::1` doesn't fail. Currently `127.0.0.1` only.
- **Avatar URL hardening**: validate server-side that emitted `avatar_url` starts with `https://cdn.discordapp.com/`; URL-encode `user_id` segments before formatting.
- **Pomelo legacy users**: `(user_id >> 22) % 6` only fits post-Pomelo accounts. For accounts with a non-`"0"` discriminator, use legacy `discriminator % 5`.
- **OAuth error body**: surface Discord's JSON error body in the error message rather than the bare HTTP status — currently makes invalid `client_id` / `redirect_uri` debugging painful.
- **README threat model**: add a note about local multi-user boxes (any logged-in user can `curl` the SSE stream); recommend single-user dev / streaming machine, or add a per-launch URL token.
- **Concurrent IPC write poisoning**: on `write_all` failure mid-frame, mark the writer poisoned / shutdown to avoid a half-corrupted byte stream surviving into the next command.
- **Initial-load no-channel unsubscribe**: if the user is not in voice when the bot first starts, the unsubscribe-on-channel-leave path is never exercised; trivial under-coverage.
- **`AddrInUse` actionable error**: when `OVERLAY_PORT` is occupied, surface a clear "port already in use" message with hint to set `OVERLAY_PORT`.
- **RTL/zero-width sanitization**: strip Unicode bidi-control characters from `display_name` before rendering to avoid layout-flip pranks.
- **`get_selected_voice_channel` error vs. null**: distinguish a transport error from genuine "user not in voice"; on transport error, surface a status-pill reason rather than empty state.
- **`spawn_ipc_loop` no-endpoint UI signal**: when Discord client is not running and IPC retries forever, send a typed `IpcStatus::DiscordOffline` so the overlay can show a pill ("Discord client not running") rather than a generic disconnected state.
- **Linux IPC probe parallelization**: 30 sequential `connect()` probes is a startup-cost smell; not actually slow (negligible at startup) but a parallel probe is cleaner.
- **5-failed-retries pill semantics**: spec I/O matrix says "After 5 failed retries → error pill stays". Current implementation shows the disconnected pill continuously, which functionally meets the user-visible outcome but doesn't track the 5-retry threshold explicitly. Tighten if a different UX is wanted later.

**Trigger to resume**: open a new `bmad-quick-dev` spec for "Discord Voice Overlay — Hardening" after manual validation of the core feature reveals which of these become user-visible in practice.

## Desktop Tray Application — Hardening backlog (post-step-04 review)

**Deferred from**: `spec-desktop-tray-app.md` (review-driven defers, 2026-04-30)

Items surfaced by reviewer agents in step-04 of the tray spec but classified `defer` (cosmetic, lower-priority polish, or rare edge cases that can ship in a follow-up).

- **High-DPI Windows icons**: render a 32×32 variant alongside 16×16 so the tray doesn't upscale-blur on 200% DPI screens.
- **Crash diagnostics under `windows_subsystem = "windows"`**: install a panic hook that writes a backtrace to `dirs::config_dir()/obs-discord-voice-overlay/crash.log` since stderr is `/dev/null` in release.
- **Balloon notifications on errors**: `tray-icon` supports OS-native notifications. Show one when autostart toggle fails or when "Open overlay preview" fails so the user gets feedback beyond a swallowed log.
- **Browser-fallback for "Open overlay preview"**: on `open::that` error, copy the URL to the clipboard and notify so corporate-locked-browser scenarios remain usable.
- **Path-with-quotes edge case for autostart**: handle binary paths containing `"` characters when registering with `auto-launch`.
- **Debounce "Open overlay preview" clicks**: rapid clicks currently spawn unbounded detached threads. Single-worker thread + cooldown.
- **Autostart path refresh on startup**: if `is_enabled()` is true and the registry/desktop entry points to an old path, re-enable to refresh to `current_exe()`. Handles the "user moved the binary" case.
- **Tray icon idle CPU**: switch from `ControlFlow::WaitUntil(now + 250ms)` polling to `EventLoopProxy::send_event` driven by `rx.changed().await` in a tokio task, so the event loop idles on `ControlFlow::Wait` until a real event arrives.
- **`MenuEvent::set_event_handler` global singleton cleanup**: clear the handler on tray exit and document the single-call invariant in `tray::run_tray`.

**Trigger to resume**: open a new `bmad-quick-dev` spec for "Desktop Tray — Hardening" after Windows-host validation surfaces which of these the user actually feels.

## Persistent Encrypted Config — Hardening backlog (post-step-04 review)

**Deferred from**: `spec-persistent-config.md` (review-driven defers, 2026-04-30)

Items surfaced by reviewer agents but classified `defer` (defense-in-depth or rare scenarios that can ship in a follow-up).

- **`zeroize` for key material**: wrap `Secrets`, derived `[u8; 32]` key, and intermediate plaintext buffers in `Zeroizing<>`. Adds a small `zeroize = "1"` dep. Protects against post-mortem core dumps and swap-page leaks.
- **EXDEV cross-filesystem rename fallback**: `tokio::fs::rename` fails with `EXDEV` if `.tmp` and final path are on different mounts. We currently keep them in the same parent dir, but if the user relocates `config_dir` to a mount with symlinks, fall back to copy + remove instead of erroring out.
- **`Secrets`-side AEAD AAD binding**: bind the plaintext non-secret fields (`version`, `overlay_port`, `token_storage_dir`, `log_level`) into the ChaCha20-Poly1305 associated data. Without it, an attacker with file-write access can swap port/log_level without invalidating the AEAD tag. Limited blast radius on a single-user machine, but a defense-in-depth miss.
- **Allocation guard on EncryptedBlob decode**: cap base64 input lengths (e.g. 4 KiB salt/nonce/ciphertext) before allocating, to avoid trivial allocator-pressure DoS via a crafted config file.
- **Tray icon refresh after `set_needs_setup`**: minor race window where a stale `Connected(true)` event is observed alongside `NeedsSetup`. State machine handles correctness, but tray icon may flicker briefly. Add a unit test pinning the behavior.
- **Setup wizard polish**: success page could show a "Restart now" button that gracefully calls the same cancellation token as the tray's Quit menu (since hot-reload is out of scope, this is a UX shortcut, not hot-reload).
- **HTML wizard separate file**: extract the inline HTML/CSS into `web/setup/index.html` etc. similar to themes for maintainability if the wizard grows.
- **Schema migration story**: current code rejects older or newer `version` fields. Add a migration path so a v1 → v2 transition can preserve secrets across an upgrade.
- **Backup before overwrite**: when `save` overwrites an existing file, first copy it to `config.json.bak` so accidental misconfiguration is recoverable.

**Trigger to resume**: new `bmad-quick-dev` spec "Persistent Config — Hardening" if any of these become user-visible during real-world use.

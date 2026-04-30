---
title: 'Discord Voice Channel Overlay for OBS'
type: 'feature'
created: '2026-04-30'
status: 'done'
context: []
baseline_commit: 'd212367849e4ffbb381eef111fbffe4e747259cd'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Streaming with Discord voice lacks a customizable overlay of the current call's participants — StreamKit is restrictive on style, and the user wants live speaking effects, custom animations, and follow-user-across-channels behavior without exposing a Discord bot in the call.

**Approach:** A Rust process on the streaming machine connects to the local Discord client over RPC IPC, OAuth-authorizes once with `rpc.voice.read` (validated working for the app owner), subscribes to voice + speaking events, and serves a custom HTML overlay over local HTTP+SSE that OBS loads as a Browser Source. Multiple built-in themes are selectable via URL query.

## Boundaries & Constraints

**Always:**
- Binary runs on the same machine as Discord client (RPC IPC is local-only).
- Cross-platform IPC: `cfg(unix)` uses Unix sockets at `$XDG_RUNTIME_DIR/discord-ipc-{0..9}` (incl. `snap.discord/` and `app/com.discordapp.Discord/` prefixes); `cfg(windows)` uses named pipes at `\\.\pipe\discord-ipc-{0..9}`.
- All config via env vars: `DISCORD_CLIENT_ID`, `DISCORD_CLIENT_SECRET`, `OVERLAY_PORT` (default `7373`), `TOKEN_STORAGE_DIR` (default OS-appropriate via `dirs`).
- `client_secret` and OAuth tokens stay in the Rust process — never sent to the browser, never logged.
- Bot follows the user's voice channel via `VOICE_CHANNEL_SELECT` — no manual channel ID config.
- Overlay auto-reconnects SSE with exponential backoff and re-syncs full state on reconnect.
- Themes embedded at compile time (`include_str!`); selection via `?theme=<name>` URL query; unknown name falls back to `default`. Each theme owns its `overlay.html`/`style.css`/`app.js` so themes may diverge in DOM, not just CSS.

**Ask First:**
- Names and aesthetic direction for the 2-3 built-in themes (proposal: `default` neutral, `minimal` no-frills, `neon` gaming/streaming).
- Empty state UX when user is not in voice (hidden vs. placeholder) — propose hidden.

**Never:**
- No Gateway bot, no `serenity`/`songbird`, no bot joining the voice channel.
- No `client_secret`/tokens in committed code, browser code, or logs.
- No analytics, telemetry, or non-Discord network calls.
- No multi-guild aggregation: overlay shows only the user's current channel.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|---|---|---|---|
| Cold start, no cached token | Valid env vars set | Handshake → AUTHORIZE → user consents → token exchange → AUTHENTICATE → subscribe → HTTP listening | User denies → clear error, exit non-zero |
| Cold start, valid cached token | Token file present, unexpired | Skip AUTHORIZE, AUTHENTICATE directly | Token revoked → fall back to AUTHORIZE flow |
| User joins/switches channel | `VOICE_CHANNEL_SELECT` with channel_id | Resubscribe `VOICE_STATE_*`, `SPEAKING_*` for new channel; SSE pushes full participant list | N/A |
| User leaves voice | `VOICE_CHANNEL_SELECT` channel_id null | SSE pushes empty participants payload | N/A |
| Participant joins/leaves | `VOICE_STATE_CREATE`/`DELETE` | SSE pushes incremental update | N/A |
| Participant speaks | `SPEAKING_START`/`STOP` | SSE pushes `{user_id, speaking}` event; CSS `.speaking` class toggles | Unknown user_id → ignore |
| Discord client closes | IPC pipe EOF | Reconnect loop polls IPC every 2s; overlay shows disconnected pill | After 5 failed retries → error pill stays |
| Access token expires | Discord 401 on auth'd command | Refresh via `refresh_token`, retry transparently | Refresh fails → AUTHORIZE on next start |

</frozen-after-approval>

## Code Map

- `Cargo.toml` -- deps: `tokio` (rt-multi-thread, macros, net, io-util, sync, signal), `axum` (sse), `serde`/`serde_json`, `reqwest` (rustls-tls, json), `uuid`, `dirs`, `tracing`, `tracing-subscriber`.
- `src/main.rs` -- entrypoint: env parse, tracing init, spawn IPC + HTTP tasks, await Ctrl-C.
- `src/ipc.rs` -- cross-platform IPC; frame read/write generic over `AsyncRead+AsyncWrite`; command/response dispatcher (oneshot replies + broadcast events); reconnect loop.
- `src/oauth.rs` -- AUTHORIZE → `POST /oauth2/token` exchange → AUTHENTICATE → refresh; token persisted to `{TOKEN_STORAGE_DIR}/token.json` (mode 0600 on Unix).
- `src/events.rs` -- typed structs (`VoiceState`, `Speaking`, `ChannelSelect`) + subscribe helpers wrapping IPC SUBSCRIBE.
- `src/state.rs` -- `OverlayState { channel_id, participants: HashMap<UserId, Participant>, speaking: HashSet<UserId> }`; `tokio::sync::watch` for snapshots + `broadcast` for deltas.
- `src/web.rs` -- axum router; `GET /` resolves theme from `?theme=<name>` query (fallback `default`) and serves the theme's `overlay.html`; `GET /themes/<name>/{style.css,app.js}` serves theme assets; `GET /events` SSE stream.
- `web/themes/{default,minimal,neon}/{overlay.html,style.css,app.js}` -- 3 built-in theme variants; each `overlay.html` references `/themes/<name>/style.css` + `/themes/<name>/app.js` for stable URLs.
- `src/themes.rs` -- compile-time theme registry (match table with `include_str!` per theme); URL-name → assets resolver with `default` fallback.
- `README.md` -- Discord app setup (Public Bot off, redirect URI), env vars, OBS Browser Source URL.

## Tasks & Acceptance

**Execution:**
- [x] `Cargo.toml` -- declare deps and binary target
- [x] `src/ipc.rs` -- generalize from `_probes/rpc-scope-probe`, add dispatcher + reconnect
- [x] `src/oauth.rs` -- token exchange, persistence with proper file mode, refresh flow
- [x] `src/events.rs` -- typed event payloads + subscribe helpers
- [x] `src/state.rs` -- state struct + watch/broadcast merge logic
- [x] `src/themes.rs` -- compile-time theme registry + URL-name resolver with default fallback
- [x] `src/web.rs` -- axum HTTP + SSE + theme-aware routing for HTML and per-theme assets
- [x] `src/main.rs` -- wire IPC, OAuth init, state, HTTP server
- [x] `web/themes/{default,minimal,neon}/{overlay.html,style.css,app.js}` -- 3 built-in theme variants
- [x] `README.md` -- setup walkthrough including OBS Browser Source URL
- [x] Unit tests in `src/ipc.rs` (frame parsing) and `src/state.rs` (merge edge cases)

**Acceptance Criteria:**
- Given valid env vars and Discord client running, when the bot starts cold, then the AUTHORIZE prompt appears in Discord, the token persists on consent, and the HTTP server listens on `OVERLAY_PORT`.
- Given the bot is running and the user is in a voice channel, when OBS loads `http://localhost:OVERLAY_PORT/`, then participants render with name, avatar, and mute/deaf indicators within 1s.
- Given the overlay is connected, when the user switches voice channels, then it re-renders with the new participants within 1s.
- Given a participant starts/stops speaking, when SPEAKING events arrive, then the `.speaking` CSS class is added/removed on their card within 200ms.
- Given the SSE connection drops, when the bot is still running, then the browser auto-reconnects and re-syncs full state without manual reload.
- Given the access token expires mid-session, when an authenticated command runs, then refresh+retry happens transparently — no visible interruption.
- Given Discord client closes and reopens mid-session, when it returns, then IPC reconnects within 5s and the overlay resumes updates.
- Given a Browser Source URL with `?theme=<name>` matching a built-in theme, when the overlay loads, then the matching theme's HTML/CSS/JS is served; given an unknown or missing theme name, when it loads, then the `default` theme is served and the response is HTTP 200 (not an error).

## Design Notes

- IPC frame: 8-byte header (`op: u32 LE` + `len: u32 LE`) + UTF-8 JSON. Validated by `_probes/rpc-scope-probe`.
- OAuth token exchange: `POST https://discord.com/api/oauth2/token` with `grant_type=authorization_code`, `client_id`, `client_secret`, `code`, `redirect_uri` matching the dev portal config.
- Avatar URL: `https://cdn.discordapp.com/avatars/{user_id}/{avatar_hash}.png?size=128`; default fallback `https://cdn.discordapp.com/embed/avatars/{(user_id >> 22) % 6}.png` (Discord 2023+ pomelo formula).
- SSE event types: `state` (full snapshot on connect/channel-change), `participant_join`, `participant_leave`, `speaking_start`, `speaking_stop`, `voice_state_update` (mute/deaf/self_video changes).
- Default theme layout: flex row of cards; card = 64px circular avatar + name; halo = animated `box-shadow` keyframe triggered by `.speaking`; mute/deaf icons absolute on avatar corner. `minimal`/`neon` may diverge in DOM/CSS — only the SSE wire format from `/events` is shared.

## Verification

**Commands:**
- `cargo build --release` -- expected: clean build on Linux x86_64 and Windows x86_64.
- `cargo clippy --all-targets -- -D warnings` -- expected: zero warnings.
- `cargo test` -- expected: IPC frame parsing and state merge tests pass.

**Manual checks:**
- Run `cargo run --release` with env vars set, click Authorize in Discord → token persists in `{TOKEN_STORAGE_DIR}/token.json`.
- Add OBS Browser Source `http://localhost:7373/`, join a voice channel → cards render with name, avatar, mute/deaf state.
- Speak in the channel → own card pulses halo within 200ms; same for other speakers.
- Switch voice channels → overlay re-renders within 1s with new participants.
- Restart Discord client mid-session → overlay pauses then resumes within 5s without browser reload.
- Load `http://localhost:7373/?theme=neon` and `?theme=minimal` → confirm distinct visual styles render. Load `?theme=does-not-exist` → confirm fallback to `default` (HTTP 200, default theme served).

## Suggested Review Order

**Entry & orchestration**

- main wires IPC subscription, HTTP server task, and driver loop.
  [`main.rs:67`](../../src/main.rs#L67)

- driver_loop watches IPC status changes and dispatches connect/disconnect.
  [`main.rs:130`](../../src/main.rs#L130)

- on_connected runs auth, subscribes events, cancels stale event handlers.
  [`main.rs:173`](../../src/main.rs#L173)

**Discord IPC protocol**

- IpcClient: async command dispatcher (oneshot replies, watch status).
  [`ipc.rs:192`](../../src/ipc.rs#L192)

- spawn_ipc_loop: reconnect with exponential backoff capped at 30s.
  [`ipc.rs:295`](../../src/ipc.rs#L295)

- read_frame: 16 MiB cap rejects oversized length declarations before allocating.
  [`ipc.rs:138`](../../src/ipc.rs#L138)

- run_session: malformed frame body logged then skipped, session survives.
  [`ipc.rs:338`](../../src/ipc.rs#L338)

**OAuth + token persistence**

- save_token: atomic .tmp write at 0o600 then rename, crash-safe and TOCTOU-tight.
  [`oauth.rs:136`](../../src/oauth.rs#L136)

- run_authorize_flow: 120s timeout wraps the IPC AUTHORIZE so user inaction can't hang.
  [`oauth.rs:176`](../../src/oauth.rs#L176)

- parse_token_response: carries over previous refresh_token when omitted by Discord.
  [`oauth.rs:261`](../../src/oauth.rs#L261)

- http_client: shared reqwest::Client (OnceLock) with 15s timeout, no per-call rebuild.
  [`oauth.rs:33`](../../src/oauth.rs#L33)

**Mid-session auth recovery (AC6 + I/O matrix row 8)**

- auth_command: catches token-invalid, refreshes + re-authenticates, retries once.
  [`main.rs:396`](../../src/main.rs#L396)

- ensure_authenticated: tries refresh before re-running AUTHORIZE on cached failure.
  [`main.rs:241`](../../src/main.rs#L241)

- is_token_invalid_error: heuristic detector for known Discord IPC error strings.
  [`main.rs:379`](../../src/main.rs#L379)

**State management**

- OverlayStore: every mutator takes the snapshot inside the same lock scope.
  [`state.rs:80`](../../src/state.rs#L80)

- StateDelta variants are the wire contract consumed by the SSE handler.
  [`state.rs:42`](../../src/state.rs#L42)

**HTTP / SSE delivery**

- sse: subscribe-before-snapshot order, Lagged auto-resync via fresh state event.
  [`web.rs:92`](../../src/web.rs#L92)

- theme_css/theme_js: 404 on unknown theme names (only `/` falls back to default).
  [`web.rs:61`](../../src/web.rs#L61)

- router: minimal axum router; single AppState shared across handlers.
  [`web.rs:43`](../../src/web.rs#L43)

**Theme registry**

- resolve: URL `?theme=<name>` lookup with `default` fallback for `/`.
  [`themes.rs:40`](../../src/themes.rs#L40)

- find_exact: no-fallback variant used by asset routes for proper 404s.
  [`themes.rs:56`](../../src/themes.rs#L56)

**Frontend overlay (exemplar)**

- safeParse wraps every JSON.parse; `<img>` onerror falls back to inline SVG.
  [`default/app.js`](../../web/themes/default/app.js)

**Setup**

- Cargo.toml: deps as listed in Code Map, no Gateway/songbird crates.
  [`Cargo.toml`](../../Cargo.toml)

- README: Discord app setup, env vars, OBS Browser Source URL.
  [`README.md`](../../README.md)

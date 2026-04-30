---
title: 'Desktop Tray Application Layer'
type: 'feature'
created: '2026-04-30'
status: 'done'
context:
  - '_bmad-output/implementation-artifacts/spec-discord-voice-overlay.md'
baseline_commit: '5ddbc57c78a4d62454566750290a0a1be623d2ec'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The overlay binary currently requires a visible console window and manual `cargo run` to launch. Streamers want a polished desktop app: launch once (or autostart at boot), forget about it, control via a tray icon. Today neither autostart nor a tray menu exists; the console window is also visible on the streaming setup which is ugly.

**Approach:** Wrap the existing IPC + HTTP + driver lifecycle with a system tray icon that shows voice-connection state, exposes a right-click menu (preview / autostart toggle / quit), persists autostart via OS-native mechanisms, and hides the console on Windows release builds. The IPC, OAuth, axum/SSE, and theme layers are untouched.

## Boundaries & Constraints

**Always:**
- Three tray-icon states derived from `OverlayStore`: `discord-offline`, `idle` (IPC up, no voice), `in-voice`. Update within 1s of state change.
- Main thread runs the UI event loop; tokio runtime built via `Builder::new_multi_thread()` on a worker. Menu actions reach async land through `mpsc`.
- `tokio_util::sync::CancellationToken` shared by HTTP, IPC driver, and event handler; "Quit" fires it for graceful shutdown.
- Autostart state queried from OS at every menu-open via `auto-launch` (no local persistence).
- Console hidden on Windows release only via `cfg_attr` `windows_subsystem`; debug keeps console for dev ergonomics.
- Icon assets embedded (`include_bytes!` or programmatic) — never loaded from disk or network.

**Ask First:**
- Visual icon design — 3 small (16×16 + 32×32) glyphs in distinct colors (proposal: same headset/speaker silhouette, color variants `#71808a` discord-offline, `#5865f2` idle Discord-blurple, `#57f287` in-voice green). Agent picks neutral defaults; surface for review.
- "Open overlay preview" URL — stays at `http://localhost:{OVERLAY_PORT}/` (default theme, no options) or carries a remembered theme. **Propose**: always default — zero state to manage, the user can bookmark a richer URL separately.
- Tooltip on hover — show current state as text (e.g. "Discord overlay — In voice"). **Propose**: yes, refreshed alongside icon.

**Never:**
- No GUI configuration window — env vars and URL options remain the only config surfaces.
- No menu options for theme/port/credentials — those stay in env/URL.
- No icon assets fetched from the network at runtime.
- No second binary — this augments the existing crate; `cargo run --release` produces one executable that owns the tray.

## I/O & Edge-Case Matrix

| Scenario | Input | Expected | Error |
|---|---|---|---|
| Release launch on Windows | Double-click `.exe` | No console; tray icon visible within 2s | Tray creation fails → log to OS log dir, exit 1 |
| Debug launch | `cargo run` | Console + tray both visible | Tray fails → log, continue without tray (degraded mode) |
| Right-click tray | OS event | Menu shows: Preview, Auto-start (with checkmark from OS state), separator, Quit | N/A |
| "Open overlay preview" clicked | Menu event | Default browser opens `http://localhost:{OVERLAY_PORT}/` within 3s | `open` crate fails → log, no further action |
| "Auto-start on boot" clicked | Menu event | OS entry created/removed via `auto-launch`; checkmark refreshed on next open | `auto-launch` errors → log, leave state unchanged |
| Reboot with autostart enabled | OS login | Binary starts, tray icon appears, no console | Same failure handling as cold launch |
| "Quit" clicked | Menu event | CancellationToken fired → HTTP server drains, IPC disconnects, process exits 0 | If a task hangs >5s, force exit 0 |
| Ctrl+C (debug) | OS signal | Same shutdown path as Quit | N/A |
| IPC connection state changes | Background watch | Tray icon image swaps within 1s; tooltip updated | Image swap fails → log, retain previous icon |
| Voice channel join/leave | Background event | Tray icon transitions between `idle` and `in-voice` | N/A |

</frozen-after-approval>

## Code Map

- `Cargo.toml` -- add `tray-icon = "0.19"`, `auto-launch = "0.5"`, `open = "5"`, `tokio-util = { version = "0.7", features = ["rt"] }`. `image` only if programmatic icon path chosen.
- `src/main.rs` -- restructured: sync `fn main()` builds multi-thread tokio runtime, spawns the existing async lifecycle as `async fn run(env, cancel)`, then constructs the tray and runs the UI event loop on the main thread. Adds `windows_subsystem` cfg_attr.
- `src/tray.rs` -- tray construction, menu, event loop dispatcher; subscribes to `watch::Receiver<TrayState>` for icon swaps; forwards menu events via `mpsc::UnboundedSender<Action>`.
- `src/icons.rs` -- 3 RGBA icon variants (programmatic preferred) returned per `TrayState`.
- `src/autostart.rs` -- `auto-launch` wrapper: `is_enabled()`, `enable()`, `disable()`; `current_exe()` resolves binary path.
- `src/state.rs` -- `TrayState { DiscordOffline, Idle, InVoice }` + `subscribe_tray_state` watch from `OverlayStore`.
- `README.md` -- "Tray application" section: menu, autostart, console-hidden behavior.

## Tasks & Acceptance

**Execution:**
- [x] `Cargo.toml` -- add `tray-icon`, `auto-launch`, `open`, `tokio-util` deps
- [x] `src/icons.rs` -- 3 RGBA icon variants (programmatic generation preferred to keep binary asset-free)
- [x] `src/autostart.rs` -- query/enable/disable wrapper around `auto-launch`
- [x] `src/state.rs` -- `TrayState` enum + `subscribe_tray_state` watch channel from `OverlayStore`
- [x] `src/tray.rs` -- tray icon, menu, event loop integration, icon swap on state changes, action dispatch
- [x] `src/main.rs` -- restructure to sync main + multi-thread runtime + main-thread event loop + CancellationToken wiring + `windows_subsystem` attribute
- [x] `README.md` -- "Tray application" section with menu items, autostart, console-hidden notes
- [x] Unit tests: `state::tray_state_mapping` (connected+channel → InVoice, etc.), `autostart::is_enabled_returns_a_value` (smoke), `icons::rgba_buffer_is_correct_size`

**Acceptance Criteria:**
- Given a release build on Windows and Discord client running, when the user double-clicks the `.exe`, then no console window appears, the tray icon shows within 2s, and `http://localhost:{OVERLAY_PORT}/` is reachable.
- Given the tray icon is present, when the user right-clicks it, then a menu appears with exactly: "Open overlay preview", "Auto-start on boot" (with a checkmark reflecting the current OS state), a separator, and "Quit".
- Given the menu is open, when "Open overlay preview" is clicked, then the OS default browser navigates to `http://localhost:{OVERLAY_PORT}/` within 3s.
- Given the menu is open, when "Auto-start on boot" is clicked, then the OS autostart entry is created (or removed if previously enabled), and reopening the menu shows the checkmark in the new state.
- Given autostart is enabled, when the user logs out and back in (or reboots), then the binary launches automatically with no console and the tray icon appears.
- Given the binary is running, when "Quit" is clicked, then within 5s the HTTP listener stops accepting new connections, the IPC client disconnects, and the process exits with code 0.
- Given the bot is running, when Discord IPC connects or disconnects, or when the user enters or leaves a voice channel, then the tray icon image and tooltip update within 1s to match the new `TrayState`.
- Given a debug build (`cargo run`), when launched, then both the console and the tray icon appear; Ctrl+C in the console triggers the same graceful shutdown path as the "Quit" menu item.

## Design Notes

- Tokio: `Builder::new_multi_thread().enable_all().build()`; `rt.spawn(run(env, cancel.clone()))`; build tray + run event loop on main; `rt.shutdown_timeout(Duration::from_secs(5))` on quit.
- TrayState: `(connected, channel_id)` → `(false, _) = DiscordOffline`, `(true, None) = Idle`, `(true, Some(_)) = InVoice`. Implement `From<&OverlayState>`.
- Icon swap: `TrayIcon::set_icon(Some(Icon::from_rgba(buf, w, h)?))` from the UI thread on `watch.has_changed()` poll.
- `auto-launch`: `AutoLaunchBuilder::new().set_app_name("obs-discord-voice-overlay").set_app_path(&current_exe())`.

## Verification

**Commands:**
- `cargo build --release` -- expected: clean build on Linux x86_64 and Windows x86_64.
- `cargo clippy --all-targets -- -D warnings` -- zero warnings.
- `cargo test` -- existing 33 tests + new unit tests pass.

**Manual checks (Windows host):**
- Double-click the release `.exe` → no console, tray icon present, `http://localhost:7373/` reachable.
- Right-click tray → menu items match spec; click "Open overlay preview" → browser opens.
- Toggle "Auto-start on boot" twice → confirm registry entry under `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` appears/disappears.
- Reboot with autostart enabled → tray icon appears at login.
- Click "Quit" → process exits cleanly (Task Manager shows no orphan).
- Join a Discord voice channel → tray icon transitions to `in-voice` variant within 1s; leave → back to `idle`; close Discord → `discord-offline`.

## Suggested Review Order

**Entry & orchestration**

- Sync main, multi-thread runtime built explicitly, tray on main thread, shutdown_timeout reachable thanks to `run_return`.
  [`main.rs:83`](../../src/main.rs#L83)

- The previous `#[tokio::main]` body lifted into `async fn run` so the tokio runtime can be spawned by the sync main.
  [`main.rs:145`](../../src/main.rs#L145)

**Tray UI integration (this spec's heart)**

- `run_tray` Windows/macOS: `tao` event loop using `run_return` so control returns to main, `cancel_fired` flag guarantees cancellation on every exit path including `LoopDestroyed`.
  [`tray.rs:121`](../../src/tray.rs#L121)

- `run_tray` Linux fallback: `handle.block_on(cancel.cancelled())` reusing the existing tokio runtime — no nested runtime, no condvar deadlock.
  [`tray.rs:328`](../../src/tray.rs#L328)

- `handle_tray_build_failure`: debug = log + continue (degraded mode, server still serves OBS); release = log + `exit(1)`.
  [`tray.rs:92`](../../src/tray.rs#L92)

- `UserEvent` enum drives the event loop's `with_user_event` pattern; tokio task forwards `watch::changed()` into the loop via `EventLoopProxy` (where wired) — see closure body.
  [`tray.rs:47`](../../src/tray.rs#L47)

**State derivation**

- `TrayState` mapping: `(connected, channel_id)` → `DiscordOffline` / `Idle` / `InVoice`. Implements `From<&OverlayState>`.
  [`state.rs:46`](../../src/state.rs#L46)

- `OverlayStore` exposes `subscribe_tray_state` watch; mutators emit on every `connected` or `channel_id` change.
  [`state.rs:110`](../../src/state.rs#L110)

**Autostart**

- `is_enabled() -> Result<bool, AutostartError>` so transient OS errors don't masquerade as "off"; tray reads with `unwrap_or(false)` and logs.
  [`autostart.rs:64`](../../src/autostart.rs#L64)

- `strip_extended_prefix` removes `\\?\` from `current_exe()` paths so Windows autostart entries always resolve correctly.
  [`autostart.rs:40`](../../src/autostart.rs#L40)

**Cancellation hook into existing layers**

- `web::serve` now takes a `CancellationToken` and uses `axum::serve(...).with_graceful_shutdown(...)` for clean drain on Quit.
  [`web.rs:160`](../../src/web.rs#L160)

**Icons**

- `rgba_for(TrayState)` programmatically draws an antialiased filled circle in the state color — no asset files, no `image` crate.
  [`icons.rs:38`](../../src/icons.rs#L38)

**Setup**

- `Cargo.toml` adds `tray-icon`, `tao` (gated to Windows/macOS to keep Linux dev VMs buildable), `auto-launch`, `open`, `tokio-util`. `image` deliberately not pulled in.
  [`Cargo.toml`](../../Cargo.toml)

- README "Tray application" section: 3 icon states, menu items, console-hidden behavior, Linux dev caveat.
  [`README.md`](../../README.md)

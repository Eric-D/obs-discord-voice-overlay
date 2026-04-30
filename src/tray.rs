//! System-tray UI: icon, right-click menu, and event-loop dispatcher.
//!
//! Architecture:
//! - The `tao` event loop owns the main thread. `tray-icon` requires its
//!   icon to be created on the same thread that pumps OS messages, so we
//!   build the [`TrayIcon`] inside the `Init` event handler.
//! - State changes from the async world arrive via a `tokio::sync::watch::Receiver<TrayState>`.
//!   We poll it on every event loop iteration plus a short repeat timer
//!   so the icon swaps within ~250ms of an upstream change.
//! - Menu clicks reach us as `MenuEvent`s on a tao user-event channel.
//!   "Quit" cancels the shared [`CancellationToken`] and exits the loop.
//! - We use `run_return` (not `run`) so the call returns control to `main`
//!   after `ControlFlow::Exit`, allowing `rt.shutdown_timeout` to actually
//!   drain the async runtime instead of being unreachable code.
//!
//! On Linux this whole module degrades to a no-op blocker on `cancel`
//! because `tray-icon` requires a GTK-based desktop environment that the
//! user's deployment target (Windows) does not need. See `Cargo.toml`.

#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use tao::event::{Event, StartCause};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tao::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tao::platform::run_return::EventLoopExtRunReturn;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use tray_icon::{Icon, TrayIconBuilder};

#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::autostart;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::icons;
use crate::state::TrayState;

/// User-events delivered to the tao event loop. Tray icon click events are
/// not currently consumed (right-click triggers the OS-native menu directly),
/// but we still wire the channel so future hover/click hooks are easy.
#[cfg(any(target_os = "windows", target_os = "macos"))]
enum UserEvent {
    Menu(MenuEvent),
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn icon_for(state: TrayState) -> Option<Icon> {
    let (rgba, w, h) = icons::rgba_for(state);
    match Icon::from_rgba(rgba, w, h) {
        Ok(icon) => Some(icon),
        Err(e) => {
            tracing::warn!("tray: failed to build icon for {state:?}: {e}");
            None
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn tooltip_for(state: TrayState) -> &'static str {
    match state {
        TrayState::DiscordOffline => "Discord overlay - Discord offline",
        TrayState::Idle => "Discord overlay - Idle",
        TrayState::InVoice => "Discord overlay - In voice",
        TrayState::NeedsSetup => "Discord overlay - Needs setup (visit /setup)",
    }
}

/// Read autostart state for the menu, treating Err as "off" but logging the
/// underlying failure so genuine OS errors don't silently masquerade as a
/// clean unchecked state.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn autostart_checked_for_menu() -> bool {
    match autostart::is_enabled() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("tray: autostart::is_enabled query failed: {e}");
            false
        }
    }
}

/// Handle a tray-build failure according to the I/O Matrix:
/// - debug builds: log + block on cancel so the binary keeps serving OBS
///   without a tray icon (degraded mode).
/// - release builds: log, cancel async tasks, then exit 1 so an operator
///   sees the failure rather than thinking the app shut down cleanly.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn handle_tray_build_failure(
    err: &dyn std::fmt::Display,
    cancel: &CancellationToken,
    handle: &Handle,
) {
    #[cfg(debug_assertions)]
    {
        tracing::error!(
            "tray: failed to build tray icon ({err}); continuing in degraded (no-tray) mode"
        );
        // Keep the async lifecycle alive — the binary still serves the
        // overlay over HTTP; just no tray UI. Block until shutdown.
        handle.block_on(cancel.cancelled());
    }
    #[cfg(not(debug_assertions))]
    {
        tracing::error!("tray: failed to build tray icon ({err}); exiting");
        cancel.cancel();
        // Give async tasks a brief chance to react before we hard-exit.
        let _ = handle;
        std::process::exit(1);
    }
}

/// Run the tray icon and OS event loop on the calling (main) thread.
///
/// Blocks until the user clicks Quit, at which point [`CancellationToken::cancel`]
/// is fired so the async tasks can drain.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn run_tray(
    mut rx: watch::Receiver<TrayState>,
    port: u16,
    cancel: CancellationToken,
    handle: Handle,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // Forward menu events into the tao loop so we can mutate UI state from
    // a single thread.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    // Build the menu. Items are kept around so their `id()`s can be matched
    // against incoming `MenuEvent`s.
    let menu = Menu::new();
    let preview_item = MenuItem::new("Open overlay preview", true, None);
    let autostart_item = CheckMenuItem::new(
        "Auto-start on boot",
        true,
        autostart_checked_for_menu(),
        None,
    );
    let configure_item = MenuItem::new("Configure...", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&preview_item)?;
    menu.append(&autostart_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&configure_item)?;
    menu.append(&quit_item)?;

    let preview_id = preview_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let configure_id = configure_item.id().clone();
    let quit_id = quit_item.id().clone();

    // The TrayIcon must be built inside the running event loop to avoid
    // platform-specific quirks (see tauri-apps/tray-icon#90). Hold a slot
    // here and create on `Init`.
    let mut tray_icon: Option<tray_icon::TrayIcon> = None;
    let mut current_state = *rx.borrow_and_update();
    // Tracks whether we already requested cancellation from the loop, so
    // the `LoopDestroyed` arm doesn't double-cancel (harmless, but noisy
    // in logs).
    let mut cancel_fired = false;
    // Tracks whether tray construction failed, so we know to enter the
    // degraded-mode / release-exit branch *after* the loop returns.
    let mut tray_build_failed: Option<String> = None;

    event_loop.run_return(|event, _, control_flow| {
        // Wait with a short timeout so we can poll the watch receiver even
        // when the OS hasn't woken us. ~250ms keeps icon-swap latency well
        // under the 1s budget from the spec.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250));

        match event {
            Event::NewEvents(StartCause::Init) => {
                let initial_icon = icon_for(current_state);
                let tooltip = tooltip_for(current_state);
                debug_assert!(
                    tooltip.len() < 64,
                    "tooltip too long: {tooltip}"
                );
                let mut builder = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip(tooltip);
                if let Some(icon) = initial_icon {
                    builder = builder.with_icon(icon);
                }
                match builder.build() {
                    Ok(t) => tray_icon = Some(t),
                    Err(e) => {
                        // Defer the actual debug/release branching until
                        // *after* the event loop returns — running it from
                        // inside the closure would either block the loop
                        // (debug) or skip the orderly return path (release).
                        tray_build_failed = Some(e.to_string());
                        if !cancel_fired {
                            cancel.cancel();
                            cancel_fired = true;
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. })
            | Event::NewEvents(StartCause::Poll) => {
                // Poll the watch receiver for a state change.
                match rx.has_changed() {
                    Ok(true) => {
                        let next = *rx.borrow_and_update();
                        if next != current_state {
                            current_state = next;
                            if let Some(t) = tray_icon.as_ref() {
                                if let Some(icon) = icon_for(current_state) {
                                    if let Err(e) = t.set_icon(Some(icon)) {
                                        tracing::warn!("tray: set_icon failed: {e}");
                                    }
                                }
                                let tooltip = tooltip_for(current_state);
                                debug_assert!(
                                    tooltip.len() < 64,
                                    "tooltip too long: {tooltip}"
                                );
                                if let Err(e) = t.set_tooltip(Some(tooltip)) {
                                    tracing::warn!("tray: set_tooltip failed: {e}");
                                }
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(_) => {
                        // Sender dropped — typically because the async
                        // runtime panicked. Don't leave a stale tray icon.
                        tracing::error!("tray: state channel closed; shutting down");
                        if !cancel_fired {
                            cancel.cancel();
                            cancel_fired = true;
                        }
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(ev)) => {
                if ev.id == preview_id {
                    let url = format!("http://localhost:{port}/");
                    // Spawn `open` so the UI thread doesn't block on the
                    // shell launch; errors are logged but otherwise ignored.
                    std::thread::spawn(move || {
                        if let Err(e) = open::that(&url) {
                            tracing::warn!("tray: open::that({url}) failed: {e}");
                        }
                    });
                } else if ev.id == autostart_id {
                    // Cache the pre-toggle state so we don't re-query the
                    // OS twice on the success path. The post-toggle state
                    // is, by definition, the inverse — we only re-query
                    // (and trust the OS) on the error path, which falls
                    // back to the cached value.
                    let currently_on = match autostart::is_enabled() {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(
                                "tray: autostart::is_enabled failed; leaving menu state unchanged: {e}"
                            );
                            return;
                        }
                    };
                    let result = if currently_on {
                        autostart::disable()
                    } else {
                        autostart::enable()
                    };
                    match result {
                        Ok(()) => {
                            // Successful toggle — flip the checkmark
                            // without re-querying the OS.
                            autostart_item.set_checked(!currently_on);
                        }
                        Err(e) => {
                            tracing::warn!("tray: autostart toggle failed: {e}");
                            // Leave the checkmark in its previous state.
                            autostart_item.set_checked(currently_on);
                        }
                    }
                } else if ev.id == configure_id {
                    let url = format!("http://localhost:{port}/setup");
                    std::thread::spawn(move || {
                        if let Err(e) = open::that(&url) {
                            tracing::warn!("tray: open::that({url}) failed: {e}");
                        }
                    });
                } else if ev.id == quit_id {
                    if !cancel_fired {
                        cancel.cancel();
                        cancel_fired = true;
                    }
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::LoopDestroyed => {
                // Catches OS-driven exits (session logoff, WM_QUIT, Cmd-Q
                // via Dock, etc.) that bypass our Quit menu item. Without
                // this, the async runtime would never see cancellation
                // and `rt.shutdown_timeout` would be a no-op.
                if !cancel_fired {
                    tracing::info!("tray: event loop destroyed; firing cancel");
                    cancel.cancel();
                    cancel_fired = true;
                }
            }
            _ => {}
        }
    });

    // Belt-and-suspenders: if anything caused the loop to return without
    // hitting our cancel paths, fire it now so main's shutdown_timeout has
    // something to drain.
    if !cancel_fired {
        cancel.cancel();
    }

    if let Some(err_msg) = tray_build_failed {
        handle_tray_build_failure(&err_msg, &cancel, &handle);
    }

    Ok(())
}

/// Linux fallback: log a warning and block until cancellation. The Linux
/// dev VM lacks the GTK/AppIndicator stack `tray-icon` requires, and the
/// user's release target is Windows. We still spin a quiet placeholder so
/// `fn main()` has a uniform return contract.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn run_tray(
    _rx: watch::Receiver<TrayState>,
    _port: u16,
    cancel: CancellationToken,
    handle: Handle,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::warn!(
        "tray: system tray UI is not built on this platform; running headless. \
         Press Ctrl-C to quit."
    );
    // Use the runtime handle the caller already owns instead of building a
    // nested current-thread runtime — that pattern panicked when called
    // from within a multi-thread runtime context, and its condvar-based
    // failure path could deadlock the main thread if the inner build
    // failed.
    handle.block_on(cancel.cancelled());
    Ok(())
}

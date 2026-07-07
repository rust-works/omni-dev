//! macOS menu-bar tray for the daemon (`cfg(all(target_os = "macos", feature =
//! "menu-bar"))`).
//!
//! The tray must own the **main thread** (macOS GUI event loops require it), so
//! [`run`] builds the tokio runtime, spawns the daemon server onto it, and
//! hands the main thread to the `tao` event loop. Each registered service
//! contributes a submenu built from its [`MenuSnapshot`]; clicks are routed
//! back to [`DaemonService::menu_action`](crate::daemon::service::DaemonService::menu_action)
//! (or, for the `copy-*` clipboard actions, fulfilled locally via the
//! clipboard). A "Quit" item cancels the shared shutdown token and drains the
//! daemon.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::Clipboard;
use serde_json::Value;
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::run_return::EventLoopExtRunReturn;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::TrayIconBuilder;

use crate::daemon::registry::ServiceRegistry;
use crate::daemon::server::{self, DaemonOptions};
use crate::daemon::service::{MenuItem as ServiceMenuItem, MenuSnapshot};
use crate::daemon::{build_default_registry, DaemonRunConfig};

/// How often the tray re-polls each service's `menu()` and rebuilds/refreshes
/// the tray. Bounds the per-service `menu()` work to ~1 Hz regardless of how
/// often the OS invokes the event-loop callback (which can be many times a
/// second) — otherwise those `menu()` calls churn a core.
const MENU_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Menu id of the daemon-level Quit item.
const QUIT_ID: &str = "omni-dev:quit";
/// Separator between a service name and its action id inside a tray menu id.
/// `\u{1}` (SOH) never occurs in a service name or action id.
const DELIM: char = '\u{1}';

/// Runs the daemon with a macOS menu-bar tray, blocking until "Quit".
///
/// Builds the runtime, starts the services, spawns the control-socket server,
/// then drives the tray event loop on the main thread.
pub fn run(cfg: DaemonRunConfig) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to start the tokio runtime")?;
    let registry = runtime
        .block_on(build_default_registry(
            cfg.bridge_config,
            cfg.bridge_token_file.as_deref(),
            cfg.bridge_token_path,
        ))
        .context("failed to start the daemon services")?;
    let registry = Arc::new(registry);

    let shutdown = CancellationToken::new();
    let server = runtime.spawn(server::run_with_shutdown(
        registry.clone(),
        DaemonOptions {
            socket_path: cfg.socket_path,
        },
        shutdown.clone(),
    ));

    let result = event_loop(runtime.handle().clone(), registry, shutdown.clone());

    // The tray exited ("Quit", or the daemon was stopped externally): stop the
    // daemon and drain it.
    shutdown.cancel();
    match runtime.block_on(server) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("daemon server error: {e}"),
        Err(e) => tracing::warn!("daemon server task failed: {e}"),
    }
    result
}

/// Cheap per-service menu snapshots, in registration order.
fn snapshots(registry: &ServiceRegistry) -> Vec<(&'static str, MenuSnapshot)> {
    registry
        .services()
        .iter()
        .map(|s| (s.name(), s.menu()))
        .collect()
}

/// A stable signature of the current menu, so the tray rebuilds only on change.
fn signature(snaps: &[(&'static str, MenuSnapshot)]) -> String {
    let mut sig = String::new();
    for (name, snap) in snaps {
        sig.push_str(name);
        sig.push('/');
        sig.push_str(&snap.title);
        sig.push('|');
        for item in &snap.items {
            match item {
                ServiceMenuItem::Label(text) => {
                    sig.push('L');
                    sig.push_str(text);
                }
                ServiceMenuItem::Separator => sig.push('S'),
                ServiceMenuItem::Action(action) => {
                    sig.push('A');
                    sig.push_str(&action.id);
                    sig.push('=');
                    sig.push_str(&action.label);
                    sig.push(if action.enabled { '+' } else { '-' });
                }
            }
            sig.push(';');
        }
    }
    sig
}

/// A handle to a built menu item, retained so its text/enabled can be updated
/// **in place** — which does not close an open menu, unlike rebuilding it.
enum ItemHandle {
    Label(MenuItem),
    Action { id: String, item: MenuItem },
    Separator,
}

/// The ordered item handles for one service's submenu.
struct SubmenuHandles {
    items: Vec<ItemHandle>,
}

/// Builds the full tray menu: one submenu per service, plus a Quit item. Action
/// ids are prefixed with `"<service>{DELIM}"` so clicks route back to the right
/// service. Returns the menu plus per-submenu item handles for in-place updates.
fn build_menu(snaps: &[(&'static str, MenuSnapshot)]) -> (Menu, Vec<SubmenuHandles>) {
    let menu = Menu::new();
    let mut handles = Vec::new();
    for (name, snap) in snaps {
        let submenu = Submenu::new(&snap.title, true);
        let mut items = Vec::new();
        for item in &snap.items {
            let appended = match item {
                ServiceMenuItem::Label(text) => {
                    let mi = MenuItem::new(text, false, None);
                    let r = submenu.append(&mi);
                    items.push(ItemHandle::Label(mi));
                    r
                }
                ServiceMenuItem::Separator => {
                    let r = submenu.append(&PredefinedMenuItem::separator());
                    items.push(ItemHandle::Separator);
                    r
                }
                ServiceMenuItem::Action(action) => {
                    let id = format!("{name}{DELIM}{}", action.id);
                    let mi = MenuItem::with_id(id.clone(), &action.label, action.enabled, None);
                    let r = submenu.append(&mi);
                    items.push(ItemHandle::Action { id, item: mi });
                    r
                }
            };
            if let Err(e) = appended {
                tracing::warn!("failed to build tray menu item: {e}");
            }
        }
        if let Err(e) = menu.append(&submenu) {
            tracing::warn!("failed to add tray submenu: {e}");
        }
        handles.push(SubmenuHandles { items });
    }
    if let Err(e) = menu.append(&PredefinedMenuItem::separator()) {
        tracing::warn!("failed to add tray separator: {e}");
    }
    if let Err(e) = menu.append(&MenuItem::with_id(
        QUIT_ID,
        "Quit omni-dev daemon",
        true,
        None,
    )) {
        tracing::warn!("failed to add quit item: {e}");
    }
    (menu, handles)
}

/// Refreshes item text/enabled **in place** when the menu structure is unchanged
/// (same items, kinds, and action ids), so live stats update without closing an
/// open menu. Returns `false` when the structure differs and the menu must be
/// rebuilt via `set_menu` (which does close it — only on session add/remove).
fn update_in_place(handles: &[SubmenuHandles], snaps: &[(&'static str, MenuSnapshot)]) -> bool {
    if handles.len() != snaps.len() {
        return false;
    }
    for (handle, (name, snap)) in handles.iter().zip(snaps) {
        if handle.items.len() != snap.items.len() {
            return false;
        }
        for (item, snap_item) in handle.items.iter().zip(&snap.items) {
            let same_kind = match (item, snap_item) {
                (ItemHandle::Label(_), ServiceMenuItem::Label(_))
                | (ItemHandle::Separator, ServiceMenuItem::Separator) => true,
                (ItemHandle::Action { id, .. }, ServiceMenuItem::Action(action)) => {
                    *id == format!("{name}{DELIM}{}", action.id)
                }
                _ => false,
            };
            if !same_kind {
                return false;
            }
        }
    }
    for (handle, (_, snap)) in handles.iter().zip(snaps) {
        for (item, snap_item) in handle.items.iter().zip(&snap.items) {
            match (item, snap_item) {
                (ItemHandle::Label(mi), ServiceMenuItem::Label(text)) => mi.set_text(text),
                (ItemHandle::Action { item, .. }, ServiceMenuItem::Action(action)) => {
                    item.set_text(&action.label);
                    item.set_enabled(action.enabled);
                }
                _ => {}
            }
        }
    }
    true
}

/// Builds the tray and pumps the macOS event loop on the main thread, refreshing
/// the menu ~1 Hz and dispatching clicks until "Quit" or until `shutdown` is
/// cancelled (a signal, or `daemon stop` over the socket).
fn event_loop(
    handle: Handle,
    registry: Arc<ServiceRegistry>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut event_loop: EventLoop<()> = EventLoop::new();

    let initial = snapshots(&registry);
    let mut last_sig = signature(&initial);
    let (initial_menu, mut handles) = build_menu(&initial);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(initial_menu))
        .with_title("od")
        .with_tooltip("omni-dev daemon")
        .build()
        .context("failed to create the menu-bar tray icon")?;

    let menu_rx = MenuEvent::receiver();
    let mut clipboard: Option<Clipboard> = None;
    // The initial menu was just built, so the first re-poll is one interval away.
    let mut last_poll = Instant::now();

    event_loop.run_return(move |_event, _target, control_flow| {
        // Wake at least once per interval to refresh the live menu state.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + MENU_POLL_INTERVAL);

        // Close the tray when the daemon is stopped by a signal or `daemon stop`.
        // This cheap check runs on every callback so shutdown is observed
        // promptly, even though the menu re-poll below is throttled.
        if shutdown.is_cancelled() {
            *control_flow = ControlFlow::Exit;
            return;
        }

        // Re-poll each service's `menu()` at most once per interval rather than on
        // every OS callback (which fires many times a second) — the callback is
        // invoked far more often than the WaitUntil timer, and recomputing the
        // snapshots every time needlessly burns CPU.
        if last_poll.elapsed() >= MENU_POLL_INTERVAL {
            last_poll = Instant::now();
            let snaps = snapshots(&registry);
            let sig = signature(&snaps);
            if sig != last_sig {
                // Update item text/enabled in place so an open menu isn't closed;
                // only a structural change (sessions added/removed) rebuilds it.
                if !update_in_place(&handles, &snaps) {
                    let (menu, new_handles) = build_menu(&snaps);
                    tray.set_menu(Some(Box::new(menu)));
                    handles = new_handles;
                }
                last_sig = sig;
            }
        }

        while let Ok(event) = menu_rx.try_recv() {
            if event.id.as_ref() == QUIT_ID {
                *control_flow = ControlFlow::Exit;
                return;
            }
            handle_action(&handle, &registry, &mut clipboard, event.id.as_ref());
        }
    });

    Ok(())
}

/// Routes a clicked menu id back to its service: the `copy-*` actions
/// (`copy-key`/`copy-snippet`/`copy-request`) each read a string field from a
/// service op and copy it to the clipboard; everything else goes to the
/// service's `menu_action`.
fn handle_action(
    handle: &Handle,
    registry: &ServiceRegistry,
    clipboard: &mut Option<Clipboard>,
    full_id: &str,
) {
    let Some((service, action)) = full_id.split_once(DELIM) else {
        tracing::debug!("ignoring tray menu id with no service prefix: {full_id}");
        return;
    };

    // Clipboard actions: fetch a string field from a service op and copy it.
    let copy = match action {
        "copy-key" => Some(("token", "token")),
        "copy-snippet" => Some(("snippet", "snippet")),
        "copy-request" => Some(("request-command", "command")),
        _ => None,
    };
    if let Some((op, field)) = copy {
        match handle.block_on(registry.dispatch(service, op, Value::Null)) {
            Ok(value) => {
                if let Some(text) = value.get(field).and_then(Value::as_str) {
                    copy_to_clipboard(clipboard, text);
                } else {
                    tracing::warn!("`{op}` op returned no `{field}`");
                }
            }
            Err(e) => tracing::warn!("copy from `{op}` op failed: {e}"),
        }
        return;
    }

    let Some(svc) = registry.get(service) else {
        tracing::warn!("tray menu action for unknown service: {service}");
        return;
    };
    if let Err(e) = handle.block_on(svc.menu_action(action)) {
        tracing::warn!("tray menu action {full_id} failed: {e}");
    }
}

/// Copies `text` to the system clipboard, lazily creating the handle.
fn copy_to_clipboard(clipboard: &mut Option<Clipboard>, text: &str) {
    if clipboard.is_none() {
        match Clipboard::new() {
            Ok(handle) => *clipboard = Some(handle),
            Err(e) => {
                tracing::warn!("clipboard unavailable: {e}");
                return;
            }
        }
    }
    if let Some(handle) = clipboard.as_mut() {
        if let Err(e) = handle.set_text(text.to_string()) {
            tracing::warn!("failed to copy to clipboard: {e}");
        }
    }
}

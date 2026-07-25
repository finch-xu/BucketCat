//! System-tray presence (M7).
//!
//! Closing the main window hides it here instead of quitting, so a transfer in
//! flight is not killed by a stray click on the close button. The tray icon is
//! then the only way back to the window -- and on macOS, where hiding also
//! drops the Dock icon, the only way to quit at all.
//!
//! ## Why the menu is built twice
//!
//! The menu's labels have to be localized, but the chosen locale lives solely
//! in the webview's `localStorage` (`bucketcat.locale`, see
//! `src/i18n/index.ts`); Rust has no way to read it. Building the tray lazily,
//! once the frontend could tell us, was the alternative -- but during a silent
//! autostart the tray icon is the *only* sign the app came up, so it has to
//! exist from `setup`. So the icon is built immediately with English
//! fallbacks and the frontend replaces them through
//! [`crate::commands::set_tray_labels`] as soon as i18n resolves, a window of
//! a few hundred milliseconds.

use tauri::menu::{Menu, MenuEvent, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Runtime};

/// Id the tray is registered under, so [`set_labels`] can find it again.
const TRAY_ID: &str = "main";

/// Menu item ids. Matched in the menu event handler; never shown to the user.
const ITEM_SHOW: &str = "tray-show";
const ITEM_QUIT: &str = "tray-quit";

/// Labels the tray comes up with before the frontend has reported the active
/// locale. See this module's doc comment.
const FALLBACK_SHOW: &str = "Show BucketCat";
const FALLBACK_QUIT: &str = "Quit";

fn build_menu<R: Runtime>(app: &AppHandle<R>, show: &str, quit: &str) -> tauri::Result<Menu<R>> {
    let show_item = MenuItem::with_id(app, ITEM_SHOW, show, true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, ITEM_QUIT, quit, true, None::<&str>)?;
    Menu::with_items(app, &[&show_item, &quit_item])
}

fn on_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        ITEM_SHOW => show_main_window(app),
        // Goes through `RunEvent::ExitRequested`, not the window's
        // `CloseRequested`, so the close-to-tray interception in `lib.rs`
        // does not swallow it. This item is not decoration: on macOS a
        // dock-hidden app has no menu bar either, so Cmd+Q is unavailable
        // while hidden and this is the only way out.
        ITEM_QUIT => app.exit(0),
        _ => {}
    }
}

/// Creates the tray icon. Call once, from `setup`.
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = build_menu(app, FALLBACK_SHOW, FALLBACK_QUIT)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("BucketCat")
        // The app icon is full-color artwork. Left as a non-template image so
        // macOS renders it as-is; as a template it would be flattened to a
        // black silhouette and the bucket-and-cats would be unreadable at
        // menu-bar size.
        .icon_as_template(false)
        .on_menu_event(on_menu_event);

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    // Platform conventions differ and neither is a matter of taste: on macOS a
    // menu-bar extra opens its menu on a plain left click, while on Windows a
    // notification-area icon restores the window on left click and reserves
    // the menu for right click.
    #[cfg(not(target_os = "macos"))]
    {
        use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
        builder = builder
            .show_menu_on_left_click(false)
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show_main_window(tray.app_handle());
                }
            });
    }

    builder.build(app)?;
    Ok(())
}

/// Swaps in localized menu labels. See this module's doc comment for why the
/// menu is not simply built with them in the first place.
pub fn set_labels<R: Runtime>(app: &AppHandle<R>, show: &str, quit: &str) -> tauri::Result<()> {
    let menu = build_menu(app, show, quit)?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))?;
    }
    Ok(())
}

/// Brings the main window back and, on macOS, the Dock icon with it.
///
/// Dock first, then the window: restoring the activation policy is what makes
/// the process eligible to own a foreground window again, so a `set_focus`
/// issued before it can be ignored and leave the window visible but behind
/// whatever the user was looking at.
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(target_os = "macos")]
    if let Err(err) = app.set_dock_visibility(true) {
        tracing::warn!("restoring dock icon failed: {err}");
    }
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if let Err(err) = window.show() {
        tracing::warn!("showing main window failed: {err}");
    }
    if let Err(err) = window.set_focus() {
        tracing::warn!("focusing main window failed: {err}");
    }
}

/// Hides the main window to the tray and, on macOS, drops the Dock icon.
///
/// Window first, then the Dock: hiding the icon while the window is still on
/// screen leaves the app briefly in the contradictory state of being a
/// background accessory that owns a visible foreground window.
pub fn hide_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(err) = window.hide() {
            tracing::warn!("hiding main window failed: {err}");
        }
    }
    #[cfg(target_os = "macos")]
    if let Err(err) = app.set_dock_visibility(false) {
        tracing::warn!("hiding dock icon failed: {err}");
    }
}

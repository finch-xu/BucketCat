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
//!
//! ## The status line and the menu-bar title
//!
//! The first menu item is a live line -- `"3 transferring · 42% · 1.2 MB/s"`,
//! or, idle, `"No active transfers"` -- kept current by [`spawn_status_ticker`]
//! polling [`crate::transfer::TransferEngine::summary`] once a second, cheap
//! enough for that cadence since it is one lock acquisition over the task
//! table rather than a clone of it. On macOS the same numbers also go into
//! the tray icon's own title, next to the icon in the menu bar, so the rate
//! is visible without opening the menu at all; `TrayIcon::set_title` is
//! documented `Unsupported` on Windows, hence the `#[cfg(target_os =
//! "macos")]` around that half.
//!
//! [`TrayState`] holds what lives between ticks. It is a plain
//! `std::sync::Mutex`, not `tokio::sync::Mutex`: every critical section here
//! is a handful of field reads and writes with no `.await` inside it, so
//! there is nothing an async mutex would buy and a std one is both cheaper
//! and safe to reach from the synchronous menu-event handler. `last_rendered`
//! exists purely to skip the main-thread round trip `set_text` costs (see the
//! API facts below) when nothing actually changed -- an idle tray, the common
//! steady state, would otherwise wake the event loop once a second forever.
//!
//! `MenuItem::set_text` and `TrayIcon::set_title` both proxy to the main
//! thread internally and are `unsafe impl Send + Sync`, so the ticker -- a
//! background Tokio task -- can call them directly on the handles it holds.
//! A call from off the main thread blocks until the event loop gets to it,
//! and returns `Err` if the loop is gone (the app is exiting), which is why
//! every call site here logs and swallows rather than unwrapping.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Id the tray is registered under, so [`set_labels`] and [`update_status`]
/// can find it again.
const TRAY_ID: &str = "main";

/// Menu item ids. Matched in the menu event handler; never shown to the user.
const ITEM_STATUS: &str = "tray-status";
const ITEM_SHOW: &str = "tray-show";
const ITEM_CHECK_UPDATE: &str = "tray-check-update";
const ITEM_SETTINGS: &str = "tray-settings";
const ITEM_QUIT: &str = "tray-quit";

/// Emitted so the frontend can bring the corresponding pane to the front: the
/// status line's click opens the transfers list, "Settings…" and "Check for
/// Updates…" both open Settings (the latter also starting a check). Defined
/// here because both ends of an IPC event have to agree on the name; nothing
/// in this crate listens for them.
pub const EVENT_OPEN_SETTINGS: &str = "app://open-settings";
pub const EVENT_OPEN_TRANSFERS: &str = "app://open-transfers";

/// Localized copy for every tray menu item and the status line, sent from the
/// frontend once i18n has resolved (see this module's doc comment) and again
/// on every language switch. Field names are `snake_case` on purpose: serde's
/// default (no `rename_all`) is what the frontend's payload already uses.
#[derive(Clone, serde::Deserialize)]
pub struct TrayTexts {
    pub show: String,
    pub quit: String,
    pub settings: String,
    pub check_update: String,
    /// Status line shown while [`StatusNumbers`] is `None` -- nothing active.
    pub status_idle: String,
    /// Status line template for when something is transferring, with the
    /// literal placeholders `{count}`, `{pct}`, `{speed}`, filled in by
    /// [`render_status`] with a plain string replace. Single braces are
    /// deliberate, not a typo -- i18next only interpolates `{{double}}`, so
    /// this string reaches Rust unexpanded.
    pub status_active: String,
}

impl TrayTexts {
    /// Labels the tray comes up with before the frontend has reported the
    /// active locale. See this module's doc comment.
    fn english_fallback() -> Self {
        Self {
            show: "Show BucketCat".to_string(),
            quit: "Quit".to_string(),
            settings: "Settings…".to_string(),
            check_update: "Check for Updates…".to_string(),
            status_idle: "No active transfers".to_string(),
            status_active: "{count} transferring · {pct}% · {speed}".to_string(),
        }
    }
}

/// The numbers behind an active status line -- everything [`render_status`]
/// needs besides the template itself. `None`, in [`TrayState::last`] and
/// [`update_status`]'s parameter, means idle, not "zero of something".
#[derive(Clone, Copy)]
struct StatusNumbers {
    count: usize,
    pct: u8,
    speed_bps: u64,
}

/// Tauri-managed state backing the tray's live parts. See this module's doc
/// comment for why the lock is a plain `std::sync::Mutex` and what
/// `last_rendered` is for.
pub struct TrayState<R: Runtime> {
    texts: Mutex<TrayTexts>,
    /// The status item's handle, so [`update_status`] can retext it without
    /// rebuilding the whole menu. Replaced, not mutated in place, whenever
    /// [`set_labels`] rebuilds the menu for a locale switch: a stale handle
    /// left over from before that swap still points at a real item, just one
    /// no longer attached to the tray, so a `set_text` racing in against it
    /// from the ticker is a harmless no-op rather than an error.
    status_item: Mutex<Option<MenuItem<R>>>,
    /// The numbers [`update_status`] last received. Re-read by [`set_labels`]
    /// so a locale switch can re-render the status line in the new language
    /// immediately, without waiting for the ticker's next tick.
    last: Mutex<Option<StatusNumbers>>,
    /// The status text last actually pushed to the menu item, so
    /// [`update_status`] can skip the main-thread round trip when nothing
    /// changed -- see this module's doc comment.
    last_rendered: Mutex<String>,
}

/// Builds the menu: the live status line, then the fixed actions. The status
/// item is `enabled: true` -- it needs to be clickable to open the transfers
/// list -- `MenuItem` has no separate "informational, not a button" mode
/// short of disabling it outright.
fn build_menu<R: Runtime>(
    app: &AppHandle<R>,
    texts: &TrayTexts,
    status_text: &str,
) -> tauri::Result<(Menu<R>, MenuItem<R>)> {
    let status_item = MenuItem::with_id(app, ITEM_STATUS, status_text, true, None::<&str>)?;
    let show_item = MenuItem::with_id(app, ITEM_SHOW, &texts.show, true, None::<&str>)?;
    let check_update_item = MenuItem::with_id(
        app,
        ITEM_CHECK_UPDATE,
        &texts.check_update,
        true,
        None::<&str>,
    )?;
    let settings_item = MenuItem::with_id(app, ITEM_SETTINGS, &texts.settings, true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, ITEM_QUIT, &texts.quit, true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &show_item,
            &check_update_item,
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;
    Ok((menu, status_item))
}

/// Emits `event` and swallows a failure with a warning. The only way this can
/// fail is the webview being gone (shutting down), and there is nothing a
/// menu-click handler could do about that.
fn emit_or_warn<R: Runtime, S: serde::Serialize + Clone>(
    app: &AppHandle<R>,
    event: &'static str,
    payload: S,
) {
    if let Err(err) = app.emit(event, payload) {
        tracing::warn!("emitting {event} failed: {err}");
    }
}

fn on_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        ITEM_STATUS => {
            show_main_window(app);
            emit_or_warn(app, EVENT_OPEN_TRANSFERS, ());
        }
        ITEM_SHOW => show_main_window(app),
        ITEM_CHECK_UPDATE => {
            show_main_window(app);
            emit_or_warn(
                app,
                EVENT_OPEN_SETTINGS,
                serde_json::json!({ "pane": "update", "auto_check": true }),
            );
        }
        ITEM_SETTINGS => {
            show_main_window(app);
            emit_or_warn(
                app,
                EVENT_OPEN_SETTINGS,
                serde_json::json!({ "pane": null, "auto_check": false }),
            );
        }
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
    let texts = TrayTexts::english_fallback();
    let status_text = render_status(&texts, None);
    let (menu, status_item) = build_menu(app, &texts, &status_text)?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("BucketCat")
        // Full-color artwork. Left as a non-template image so macOS renders it
        // as-is; as a template it would be flattened to a black silhouette and
        // the bucket-and-cats would be unreadable at menu-bar size.
        .icon_as_template(false)
        .on_menu_event(on_menu_event);

    // A dedicated icon rather than `default_window_icon()`: that one is macOS
    // app-icon artwork -- a white squircle whose bucket-and-cats cover only
    // ~64% of the canvas -- so the drawing came out tiny once the system
    // scaled the whole square down to menu-bar height. This is the same
    // artwork with the squircle floodfilled away and cropped flush, bringing
    // the drawing to ~91%.
    //
    // `include_image!` decodes at compile time and embeds raw RGBA, which is
    // why it needs no `image-png` feature (and adds no lock entries) but makes
    // the icon's dimensions a direct binary cost. 64x64 is the deliberate
    // ceiling: it downsamples to a Retina menu bar (44px) and a 200%-DPI
    // notification area (32px) without ever upscaling, for 16KB.
    builder = builder.icon(tauri::include_image!("./icons/tray.png"));

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

    app.manage(TrayState {
        texts: Mutex::new(texts),
        status_item: Mutex::new(Some(status_item)),
        last: Mutex::new(None),
        last_rendered: Mutex::new(status_text),
    });

    Ok(())
}

/// Swaps in localized menu labels and re-renders the status line in the new
/// language. See this module's doc comment for why the menu is not simply
/// built with them in the first place.
pub fn set_labels<R: Runtime>(app: &AppHandle<R>, texts: TrayTexts) -> tauri::Result<()> {
    let state = app.state::<TrayState<R>>();

    *state.texts.lock().unwrap() = texts.clone();

    let last = *state.last.lock().unwrap();
    let status_text = render_status(&texts, last);

    let (menu, status_item) = build_menu(app, &texts, &status_text)?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))?;
    }

    *state.status_item.lock().unwrap() = Some(status_item);
    *state.last_rendered.lock().unwrap() = status_text;
    Ok(())
}

/// Formats a byte-per-second rate for the status line, 1024-based with one
/// decimal place from KB/s up (whole bytes below that, where a fraction would
/// be noise): `"999 B/s"`, `"12.3 KB/s"`, `"12.3 MB/s"`, `"1.2 GB/s"`.
fn format_speed(bps: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if bps < 1024 {
        return format!("{bps} B/s");
    }
    let bps = bps as f64;
    if bps < MB {
        format!("{:.1} KB/s", bps / KB)
    } else if bps < GB {
        format!("{:.1} MB/s", bps / MB)
    } else {
        format!("{:.1} GB/s", bps / GB)
    }
}

/// Renders the status line: the idle copy verbatim, or the active template
/// with `{count}`/`{pct}`/`{speed}` filled in by plain string substitution --
/// single braces are not i18next interpolation syntax, see [`TrayTexts`].
fn render_status(texts: &TrayTexts, numbers: Option<StatusNumbers>) -> String {
    let Some(numbers) = numbers else {
        return texts.status_idle.clone();
    };
    texts
        .status_active
        .replace("{count}", &numbers.count.to_string())
        .replace("{pct}", &numbers.pct.to_string())
        .replace("{speed}", &format_speed(numbers.speed_bps))
}

/// A fixed 5-sample sliding window over a cumulative byte counter --
/// [`crate::transfer::EngineSummary::all_transferred`], polled once a second
/// by [`spawn_status_ticker`] -- used to estimate the status line's speed.
///
/// Deliberately sample-counted rather than time-windowed like
/// `transfer::progress::SpeedWindow`: the ticker's cadence is fixed (one
/// push per second, `MissedTickBehavior::Skip` aside), so five samples is
/// already "the last ~5 seconds" without needing to track and trim by
/// elapsed time the way a variable-rate per-task window does.
///
/// The clock is a parameter, not read internally, for the same testability
/// reason as that other window: production passes `Instant::now()`, tests
/// pass synthetic instants.
#[derive(Default)]
struct SpeedWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedWindow {
    const CAPACITY: usize = 5;

    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, at: Instant, total_bytes: u64) {
        if self.samples.len() == Self::CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back((at, total_bytes));
    }

    /// Bytes per second between the window's oldest and newest sample, or `0`
    /// with fewer than two distinct-in-time samples to diff.
    ///
    /// `checked_sub` rather than a signed subtraction: `all_transferred` can
    /// fall as well as rise -- a task leaving the active set removes its
    /// bytes from the sum, so the total can dip for one tick -- and that must
    /// read as stalled (`0`), not wrap into a near-`u64::MAX` speed reading.
    fn bps(&self) -> u64 {
        let Some((oldest, first_bytes)) = self.samples.front().copied() else {
            return 0;
        };
        let Some((newest, last_bytes)) = self.samples.back().copied() else {
            return 0;
        };
        let elapsed = newest.saturating_duration_since(oldest).as_secs_f64();
        if elapsed <= 0.0 {
            return 0;
        }
        let delta = last_bytes.saturating_sub(first_bytes);
        (delta as f64 / elapsed) as u64
    }
}

/// Pushes `n` (`None` = idle) into the tray: stores it, re-renders the status
/// line, and -- only if the rendered text actually changed, see this
/// module's doc comment -- retexts the status menu item and, on macOS,
/// updates the tray icon's own title.
///
/// Every fallible step warns and continues rather than propagating: the
/// caller is a 1Hz background ticker with no one to report an `Err` to, and
/// per this module's doc comment, a proxy-to-main-thread failure only
/// happens while the app is exiting anyway.
///
/// Not `pub`: [`spawn_status_ticker`], its only caller, lives right here in
/// this module, and keeping it module-private is also what lets
/// [`StatusNumbers`] itself stay private -- nothing outside `tray.rs` needs
/// to name either.
fn update_status<R: Runtime>(app: &AppHandle<R>, n: Option<StatusNumbers>) {
    let state = app.state::<TrayState<R>>();
    *state.last.lock().unwrap() = n;

    let texts = state.texts.lock().unwrap().clone();
    let rendered = render_status(&texts, n);
    {
        let mut last_rendered = state.last_rendered.lock().unwrap();
        if *last_rendered == rendered {
            return;
        }
        *last_rendered = rendered.clone();
    }

    if let Some(item) = state.status_item.lock().unwrap().as_ref() {
        if let Err(err) = item.set_text(&rendered) {
            tracing::warn!("updating tray status text failed: {err}");
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let title = n.map(|numbers| format!("{}%", numbers.pct));
        if let Err(err) = tray.set_title(title) {
            tracing::warn!("updating tray title failed: {err}");
        }
    }
}

/// Starts the 1Hz poll that drives [`update_status`]. Call once, from
/// `setup`, after [`build`] (which manages [`TrayState`]) and after the
/// transfer engine is itself managed -- see `lib.rs`.
pub fn spawn_status_ticker<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut window = SpeedWindow::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        // Skipping missed ticks keeps a stalled executor from firing a burst
        // of catch-up polls once it gets scheduled again.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;

            // `summary` awaits the engine's own lock, not `TrayState`'s --
            // that one is only ever touched synchronously, below.
            let summary = app
                .state::<crate::transfer::TransferEngine>()
                .summary()
                .await;

            window.push(Instant::now(), summary.all_transferred);

            // `checked_div` folds the `active_total == 0` guard into the
            // division itself; `min(100)` guards the `as u8` cast against
            // the numerator ever nudging past the denominator (e.g. a
            // `total` read one tick stale) -- not expected in practice, but
            // a saturated percentage is a much cheaper mistake than a
            // wrapped one.
            let pct = summary
                .active_transferred
                .saturating_mul(100)
                .checked_div(summary.active_total)
                .unwrap_or(0)
                .min(100) as u8;

            let numbers = (summary.active_count > 0).then_some(StatusNumbers {
                count: summary.active_count,
                pct,
                speed_bps: window.bps(),
            });
            update_status(&app, numbers);
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- format_speed ----

    #[test]
    fn format_speed_stays_in_bytes_below_one_kib() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(999), "999 B/s");
    }

    #[test]
    fn format_speed_switches_to_kib_at_the_1024_boundary() {
        assert_eq!(format_speed(1024), "1.0 KB/s");
        assert_eq!(format_speed(12_595), "12.3 KB/s");
    }

    #[test]
    fn format_speed_scales_through_mib_and_gib() {
        assert_eq!(format_speed(12_897_996), "12.3 MB/s");
        assert_eq!(format_speed(1_288_490_189), "1.2 GB/s");
    }

    // ---- render_status ----

    #[test]
    fn render_status_is_the_idle_copy_verbatim_when_nothing_is_active() {
        let texts = TrayTexts::english_fallback();
        assert_eq!(render_status(&texts, None), "No active transfers");
    }

    #[test]
    fn render_status_fills_in_the_active_template() {
        let texts = TrayTexts::english_fallback();
        let numbers = StatusNumbers {
            count: 3,
            pct: 42,
            speed_bps: 12_595,
        };
        assert_eq!(
            render_status(&texts, Some(numbers)),
            "3 transferring · 42% · 12.3 KB/s"
        );
    }

    // ---- SpeedWindow ----

    #[test]
    fn speed_window_reports_zero_with_fewer_than_two_samples() {
        let mut w = SpeedWindow::new();
        assert_eq!(w.bps(), 0);
        w.push(Instant::now(), 1_000);
        assert_eq!(w.bps(), 0);
    }

    #[test]
    fn speed_window_diffs_the_oldest_and_newest_sample() {
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.push(t0, 0);
        w.push(t0 + Duration::from_secs(1), 1_000);
        w.push(t0 + Duration::from_secs(2), 3_000);
        // 3,000 bytes over the 2s from the oldest sample to the newest.
        assert_eq!(w.bps(), 1_500);
    }

    #[test]
    fn speed_window_clamps_a_rollback_to_zero() {
        // `all_transferred` can dip when a task leaves the active set; that
        // must read as stalled, not wrap into a huge speed.
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.push(t0, 5_000);
        w.push(t0 + Duration::from_secs(1), 1_000);
        assert_eq!(w.bps(), 0);
    }

    #[test]
    fn speed_window_drops_samples_past_its_five_slot_capacity() {
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.push(t0, 0);
        w.push(t0 + Duration::from_secs(1), 1_000_000); // ages out; must not skew the reading
        for i in 2..=6u64 {
            w.push(t0 + Duration::from_secs(i), 1_000_000 + i * 100);
        }
        // Only the last 5 pushes survive: t0+2s (1,000,200) .. t0+6s
        // (1,000,600), a 400-byte delta over 4s.
        assert_eq!(w.bps(), 100);
    }
}

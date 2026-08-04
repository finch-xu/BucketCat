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
//! "macos")]` around that half. Once the last active task finishes, the
//! title lingers on `100%` for a few seconds ([`FINISH_LINGER`]) rather than
//! blanking immediately, so a fast glance at the menu bar still catches the
//! completion; after that it falls back to a frozen `{pct}%` if paused tasks
//! remain, or to blank otherwise. Paused tasks with nothing active pin the
//! title and status line to the paused set's own transferred/total ratio --
//! it does not idle to blank -- since that ratio is frozen until the user
//! resumes or removes them.
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
//!
//! That blocking is also why no `TrayState` lock may ever be *held across*
//! one of these proxied calls (`set_text`, `set_title`, `set_menu`, or
//! `MenuItem`/`Menu::with_items` building a new item). [`set_labels`]
//! normally runs on the main thread itself (a synchronous
//! `#[tauri::command]` executes inline on the IPC thread); if it, or the
//! ticker's [`update_status`], blocked inside a proxied call while still
//! holding a lock the other side needs, the two would deadlock -- the main
//! thread stuck waiting on the lock, the ticker stuck waiting for the main
//! thread's event loop to come back around. Every lock acquisition here is
//! kept to a self-contained read or write of plain data for exactly that
//! reason.

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
    /// Status line shown for `TrayStatus::Idle` and `TrayStatus::JustFinished`
    /// alike -- see [`render_status`] for why the latter, despite the title
    /// still reading `100%`, uses this same copy rather than its own.
    pub status_idle: String,
    /// Status line template for when something is transferring, with the
    /// literal placeholders `{count}`, `{pct}`, `{speed}`, filled in by
    /// [`render_status`] with a plain string replace. Single braces are
    /// deliberate, not a typo -- i18next only interpolates `{{double}}`, so
    /// this string reaches Rust unexpanded.
    pub status_active: String,
    /// Status line template for `TrayStatus::Paused` -- nothing active, but
    /// one or more tasks sit paused with their progress frozen. Same
    /// single-brace placeholder convention as `status_active`, just without
    /// `{speed}`: a paused task is not moving, so a rate would only ever
    /// read `0 B/s`.
    pub status_paused: String,
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
            status_paused: "{count} paused · {pct}%".to_string(),
        }
    }
}

/// The numbers behind an active status line -- everything [`render_status`]
/// needs besides the template itself, for [`TrayStatus::Active`].
#[derive(Clone, Copy)]
struct StatusNumbers {
    count: usize,
    pct: u8,
    speed_bps: u64,
}

/// The tray's four-way status, in priority order high to low -- what
/// [`next_status`] computes each tick and [`update_status`] renders into the
/// status line and (macOS only) the menu-bar title. Replaces a plain
/// `Option<StatusNumbers>`: idle used to be the only state besides "active
/// with these numbers", but the status line now also has to tell a
/// just-finished transfer and a merely-paused one apart from true idle, and
/// from each other, so a bare `None` no longer carries enough information.
#[derive(Clone, Copy)]
enum TrayStatus {
    /// Nothing active, nothing paused, and past any [`FINISH_LINGER`]
    /// window. Status line reads `status_idle`; title clears.
    Idle,
    /// At least one task is transferring right now. Status line and title
    /// both reflect `numbers`, recomputed fresh every tick.
    Active(StatusNumbers),
    /// The active set just emptied out by completing (as opposed to being
    /// cancelled or failing) and [`FINISH_LINGER`] has not yet elapsed.
    /// Status line still reads `status_idle` -- "finished" and "idle" say
    /// the same thing in the menu -- but the title pins to `100%` so the
    /// completion is visible for a beat even to someone who only glances at
    /// the menu bar.
    JustFinished,
    /// Nothing active, no linger window running, but one or more tasks sit
    /// paused. `pct` is the paused set's own transferred/total ratio, frozen
    /// until the user resumes, cancels, or removes them.
    Paused { count: usize, pct: u8 },
}

/// Tauri-managed state backing the tray's live parts. See this module's doc
/// comment for why the lock is a plain `std::sync::Mutex` and what
/// `last_rendered`/`last_title` are for.
pub struct TrayState<R: Runtime> {
    texts: Mutex<TrayTexts>,
    /// The status item's handle, so [`update_status`] can retext it without
    /// rebuilding the whole menu. Replaced, not mutated in place, whenever
    /// [`set_labels`] rebuilds the menu for a locale switch: a stale handle
    /// left over from before that swap still points at a real item, just one
    /// no longer attached to the tray, so a `set_text` racing in against it
    /// from the ticker is a harmless no-op rather than an error.
    status_item: Mutex<Option<MenuItem<R>>>,
    /// The status [`update_status`] last received. Re-read by [`set_labels`]
    /// so a locale switch can re-render the status line in the new language
    /// immediately, without waiting for the ticker's next tick.
    last: Mutex<TrayStatus>,
    /// The status text last actually pushed to the menu item, so
    /// [`update_status`] can skip the main-thread round trip when nothing
    /// changed -- see this module's doc comment.
    last_rendered: Mutex<String>,
    /// The macOS title string last actually pushed to the tray icon, deduped
    /// independently of `last_rendered` -- see [`update_status`] for why the
    /// two need separate tracking. Starts `""`, matching the untitled tray
    /// [`build`] creates (no `set_title` call has ever run yet).
    ///
    /// Only read inside `update_status`'s `#[cfg(target_os = "macos")]`
    /// block -- Windows/Linux trays have no `set_title` to dedupe against --
    /// so a non-macOS build never reads it and would otherwise warn "field
    /// is never read".
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    last_title: Mutex<String>,
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
    let status_text = render_status(&texts, TrayStatus::Idle);
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
        last: Mutex::new(TrayStatus::Idle),
        last_rendered: Mutex::new(status_text),
        last_title: Mutex::new(String::new()),
    });

    Ok(())
}

/// Swaps in localized menu labels and re-renders the status line in the new
/// language. See this module's doc comment for why the menu is not simply
/// built with them in the first place.
///
/// `try_state`, not `state`: [`TrayState`] is only `manage`d after
/// [`build`]'s `builder.build(app)?` succeeds, and `lib.rs` keeps running
/// with a merely logged error if that fails. This command is still wired up
/// unconditionally on that path, so a missing `TrayState` has to degrade to a
/// no-op rather than the panic `state()` would give -- the same graceful
/// fallback the old `tray_by_id`-returns-`None` code path had.
pub fn set_labels<R: Runtime>(app: &AppHandle<R>, texts: TrayTexts) -> tauri::Result<()> {
    let Some(state) = app.try_state::<TrayState<R>>() else {
        return Ok(());
    };

    // Each `lock()` below is its own statement, released at the semicolon --
    // never held across `build_menu` or `set_menu`, both of which proxy to
    // the main thread and block. That matters here specifically because this
    // function typically *runs on* the main thread (a synchronous
    // `#[tauri::command]` executes inline on the IPC thread): holding one of
    // these locks into a proxied call would let it deadlock against
    // `update_status`'s ticker task the same way fixed there -- one side
    // blocked in the proxy waiting for the main thread's event loop, the
    // other blocked on the lock the main thread is holding.
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

/// Renders the status line for `status`: the idle copy verbatim for
/// [`TrayStatus::Idle`] and [`TrayStatus::JustFinished`] alike -- "finished"
/// and "idle" read the same in the menu, only the title tells them apart,
/// see [`render_title`] -- or the matching template with `{count}`/`{pct}`
/// (and, for `Active`, `{speed}`) filled in by plain string substitution.
/// Single braces are not i18next interpolation syntax, see [`TrayTexts`].
fn render_status(texts: &TrayTexts, status: TrayStatus) -> String {
    match status {
        TrayStatus::Idle | TrayStatus::JustFinished => texts.status_idle.clone(),
        TrayStatus::Active(numbers) => texts
            .status_active
            .replace("{count}", &numbers.count.to_string())
            .replace("{pct}", &numbers.pct.to_string())
            .replace("{speed}", &format_speed(numbers.speed_bps)),
        TrayStatus::Paused { count, pct } => texts
            .status_paused
            .replace("{count}", &count.to_string())
            .replace("{pct}", &pct.to_string()),
    }
}

/// Renders the macOS menu-bar title for `status`: a bare percentage for
/// `Active`/`Paused`, a pinned `100%` for `JustFinished` (the active
/// percentage that would have been showing the instant before is gone by
/// then, so it is spelled out rather than carried over), and an empty string
/// for `Idle` -- see [`update_status`] for why an empty string, not `None`,
/// is what actually clears the title.
///
/// Only called from `update_status`'s `#[cfg(target_os = "macos")]` branch
/// (and from tests, which compile on every platform) -- Windows/Linux trays
/// have no `set_title` equivalent -- so a non-macOS build never calls it
/// outside `cfg(test)` and would otherwise warn `dead_code`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn render_title(status: TrayStatus) -> String {
    match status {
        TrayStatus::Idle => String::new(),
        TrayStatus::Active(numbers) => format!("{}%", numbers.pct),
        TrayStatus::JustFinished => "100%".to_string(),
        TrayStatus::Paused { pct, .. } => format!("{pct}%"),
    }
}

/// Percent complete, `0..=100`, for a `transferred`-of-`total` byte pair.
/// Used for both the active and the paused set, each computed from its own
/// pair by [`next_status`].
///
/// `checked_div` folds the `total == 0` guard into the division itself;
/// `min(100)` guards the `as u8` cast against the numerator ever nudging
/// past the denominator (e.g. a `total` read one tick stale) -- not
/// expected in practice, but a saturated percentage is a much cheaper
/// mistake than a wrapped one.
fn pct(transferred: u64, total: u64) -> u8 {
    transferred
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100) as u8
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

/// How long [`TrayStatus::JustFinished`] lingers after the active set empties
/// out by completing, before [`next_status`] lets it fall through to
/// `Paused` or `Idle`. Long enough to catch a glance at the menu bar that
/// lands a second or two after the fact, short enough that the tray is not
/// stuck claiming "100%" long after it stopped being true.
const FINISH_LINGER: Duration = Duration::from_secs(3);

/// Computes this tick's [`TrayStatus`] from the engine's latest summary, in
/// the same high-to-low priority the type's variants are documented in.
/// Pulled out of [`spawn_status_ticker`]'s loop as a pure function (modulo
/// the two pieces of state it threads through by `&mut`/return) so the
/// priority logic -- especially the linger window's edge cases -- can be
/// unit-tested without spinning up a ticker or a real `TransferEngine`.
///
/// `prev_completed` and `linger_until` are the two bits of memory this needs
/// across ticks that a single `EngineSummary` snapshot cannot supply by
/// itself: whether `completed_count` just went up (a table of running
/// totals gives no "just" without a previous reading to diff against), and,
/// once it has, when the resulting linger window ends. `linger_until` is
/// `&mut` rather than returned because it can also be *cleared* early, by a
/// new task going active mid-linger -- a plain return value would make the
/// caller respond to two different "here is the new value" signals instead
/// of one.
fn next_status(
    summary: &crate::transfer::EngineSummary,
    prev_completed: u64,
    linger_until: &mut Option<Instant>,
    now: Instant,
    speed_bps: u64,
) -> TrayStatus {
    if summary.active_count > 0 {
        // A fresh task starting mid-linger outranks the leftover "just
        // finished" window from the previous batch -- there is something to
        // report right now, so the stale window is dropped rather than left
        // to expire and briefly resurrect `JustFinished` once this task
        // itself finishes.
        *linger_until = None;
        return TrayStatus::Active(StatusNumbers {
            count: summary.active_count,
            pct: pct(summary.active_transferred, summary.active_total),
            speed_bps,
        });
    }

    if summary.completed_count > prev_completed {
        *linger_until = Some(now + FINISH_LINGER);
    }
    if let Some(t) = *linger_until {
        if now < t {
            return TrayStatus::JustFinished;
        }
        *linger_until = None;
    }

    if summary.paused_count > 0 {
        return TrayStatus::Paused {
            count: summary.paused_count,
            pct: pct(summary.paused_transferred, summary.paused_total),
        };
    }

    TrayStatus::Idle
}

/// Pushes `status` into the tray: stores it, re-renders the status line and
/// (macOS only) the menu-bar title, and retexts/retitles only whichever of
/// the two actually changed.
///
/// The two are deliberately deduped against *separate* "last rendered"
/// records (`last_rendered` for the menu text, `last_title` for the title)
/// rather than one combined check. `JustFinished` and `Idle` render the same
/// menu text -- see [`render_status`] -- but a different title (`"100%"`
/// lingering vs. cleared, see [`render_title`]); a single shared dedup keyed
/// off the menu text would see "text unchanged" on that transition and skip
/// the title update along with it, leaving `"100%"` stuck in the menu bar
/// forever. Checking them independently means each of the two proxied calls
/// below only ever fires when its own rendered output actually moved, and
/// -- the idle steady state this module's doc comment calls out -- neither
/// fires when nothing did.
///
/// Every fallible step warns and continues rather than propagating: the
/// caller is a 1Hz background ticker with no one to report an `Err` to, and
/// per this module's doc comment, a proxy-to-main-thread failure only
/// happens while the app is exiting anyway.
///
/// Not `pub`: [`spawn_status_ticker`], its only caller, lives right here in
/// this module, and keeping it module-private is also what lets
/// [`TrayStatus`] itself stay private -- nothing outside `tray.rs` needs to
/// name either.
fn update_status<R: Runtime>(app: &AppHandle<R>, status: TrayStatus) {
    // `try_state`, not `state`: `spawn_status_ticker` (this function's only
    // caller) is only started after `build` succeeds in `lib.rs`, so
    // `TrayState` is expected to exist by the time any tick lands here. Still
    // guarded rather than assumed, the same defensive no-op as `set_labels`,
    // in case that call order ever changes.
    let Some(state) = app.try_state::<TrayState<R>>() else {
        return;
    };
    *state.last.lock().unwrap() = status;

    let texts = state.texts.lock().unwrap().clone();
    let rendered = render_status(&texts, status);
    let text_changed = {
        let mut last_rendered = state.last_rendered.lock().unwrap();
        let changed = *last_rendered != rendered;
        if changed {
            *last_rendered = rendered.clone();
        }
        changed
    };

    if text_changed {
        // The `MutexGuard` must not still be held when `set_text` runs: it
        // proxies to the main thread and blocks this task until that thread
        // services it (see this module's doc comment). If the main thread is
        // meanwhile inside `set_labels`, blocked acquiring this very lock (a
        // locale switch races an in-flight tick), neither side can move --
        // the main thread never gets back to its event loop to run the
        // proxied closure, and this task never releases the lock the main
        // thread wants. Cloning the handle (an `Arc` bump -- `MenuItem`'s
        // `Clone` impl, the same thing `run_item_main_thread!` does
        // internally before proxying) and letting the guard drop *before*
        // the call sidesteps that cycle.
        let item = state.status_item.lock().unwrap().clone();
        if let Some(item) = item {
            if let Err(err) = item.set_text(&rendered) {
                tracing::warn!("updating tray status text failed: {err}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let title = render_title(status);
        let title_changed = {
            let mut last_title = state.last_title.lock().unwrap();
            let changed = *last_title != title;
            if changed {
                *last_title = title.clone();
            }
            changed
        };

        if title_changed {
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                // Must always pass `Some`, never `None`, even to clear the
                // title down to nothing. tray-icon 0.24.1's
                // `set_title_inner` (`platform_impl/macos/mod.rs:169-191` in
                // that crate) is `if let Some(title) = title { ... }` with no
                // `else` branch at all -- `set_title(None)` reaches the
                // `NSStatusItem` and does *nothing*, silently returning as if
                // it had succeeded, rather than clearing whatever title was
                // already showing. tauri 2.11.5's `TrayIcon::set_title`
                // forwards the `Option` straight through to that function
                // unchanged. `Some("")` is the only way to actually clear a
                // title: it takes the `Some` branch, calls
                // `setTitle(@"")`, and (back in `set_title`, not
                // `set_title_inner`) runs `update_dimensions()` afterwards
                // either way, which is what shrinks the status item back
                // down once there is no text left to make room for.
                if let Err(err) = tray.set_title(Some(title.as_str())) {
                    tracing::warn!("updating tray title failed: {err}");
                }
            }
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

        // `prev_completed` starts at `0`, not read from a first summary
        // before the loop: `completed_count` is a process-lifetime counter
        // that itself starts at `0` (see `EngineSummary::completed_count`),
        // so seeding this at `0` too still correctly reports tasks that
        // completed in the instant between engine startup and this task's
        // first poll -- the first tick's `completed_count > prev_completed`
        // check still trips, and surfacing that as `JustFinished` rather
        // than silently swallowing it is the right call: those tasks really
        // did just finish, from the user's point of view.
        let mut prev_completed = 0u64;
        let mut linger_until: Option<Instant> = None;

        loop {
            ticker.tick().await;

            // `summary` awaits the engine's own lock, not `TrayState`'s --
            // that one is only ever touched synchronously, below.
            let summary = app
                .state::<crate::transfer::TransferEngine>()
                .summary()
                .await;

            let now = Instant::now();
            window.push(now, summary.all_transferred);

            let status = next_status(
                &summary,
                prev_completed,
                &mut linger_until,
                now,
                window.bps(),
            );
            prev_completed = summary.completed_count;

            update_status(&app, status);
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
    fn render_status_is_the_idle_copy_verbatim_when_idle() {
        let texts = TrayTexts::english_fallback();
        assert_eq!(
            render_status(&texts, TrayStatus::Idle),
            "No active transfers"
        );
    }

    #[test]
    fn render_status_is_also_the_idle_copy_when_just_finished() {
        // JustFinished and Idle share menu copy -- only the title (see
        // render_title below) tells them apart.
        let texts = TrayTexts::english_fallback();
        assert_eq!(
            render_status(&texts, TrayStatus::JustFinished),
            "No active transfers"
        );
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
            render_status(&texts, TrayStatus::Active(numbers)),
            "3 transferring · 42% · 12.3 KB/s"
        );
    }

    #[test]
    fn render_status_fills_in_the_paused_template() {
        let texts = TrayTexts::english_fallback();
        assert_eq!(
            render_status(&texts, TrayStatus::Paused { count: 2, pct: 55 }),
            "2 paused · 55%"
        );
    }

    // ---- render_title ----

    #[test]
    fn render_title_maps_all_four_states() {
        let numbers = StatusNumbers {
            count: 3,
            pct: 42,
            speed_bps: 0,
        };
        assert_eq!(render_title(TrayStatus::Idle), "");
        assert_eq!(render_title(TrayStatus::Active(numbers)), "42%");
        assert_eq!(render_title(TrayStatus::JustFinished), "100%");
        assert_eq!(
            render_title(TrayStatus::Paused { count: 2, pct: 55 }),
            "55%"
        );
    }

    // ---- pct ----

    #[test]
    fn pct_is_zero_for_an_empty_total() {
        assert_eq!(pct(0, 0), 0);
        assert_eq!(pct(5, 0), 0);
    }

    #[test]
    fn pct_saturates_at_100_when_transferred_exceeds_total() {
        // A `total` read one tick stale can leave `transferred` briefly
        // ahead of it; that must clamp, not wrap the `as u8` cast.
        assert_eq!(pct(150, 100), 100);
    }

    #[test]
    fn pct_divides_normally_within_range() {
        assert_eq!(pct(50, 100), 50);
    }

    // ---- next_status ----

    /// Builds a synthetic [`crate::transfer::EngineSummary`] with only the
    /// fields a given test cares about spelled out at the call site; the
    /// rest default to "nothing going on" so every test only has to name
    /// what it is actually exercising.
    fn summary(
        active_count: usize,
        active_transferred: u64,
        active_total: u64,
        completed_count: u64,
        paused_count: usize,
        paused_transferred: u64,
        paused_total: u64,
    ) -> crate::transfer::EngineSummary {
        crate::transfer::EngineSummary {
            active_count,
            active_transferred,
            active_total,
            all_transferred: active_transferred + paused_transferred,
            completed_count,
            paused_count,
            paused_transferred,
            paused_total,
        }
    }

    #[test]
    fn next_status_lingers_on_just_finished_then_falls_back_to_idle() {
        let t0 = Instant::now();
        let mut linger_until = None;

        // Active set just emptied out by completing: completed_count ticked
        // up from the 0 `prev_completed` this test starts from.
        let finished = summary(0, 0, 0, 1, 0, 0, 0);
        let status = next_status(&finished, 0, &mut linger_until, t0, 0);
        assert!(matches!(status, TrayStatus::JustFinished));
        assert_eq!(linger_until, Some(t0 + FINISH_LINGER));

        // Same summary, no further completions, polled right as the linger
        // window closes: falls through to Idle (nothing paused).
        let status = next_status(&finished, 1, &mut linger_until, t0 + FINISH_LINGER, 0);
        assert!(matches!(status, TrayStatus::Idle));
        assert_eq!(linger_until, None);
    }

    #[test]
    fn next_status_falls_back_to_paused_when_the_linger_window_closes() {
        let t0 = Instant::now();
        let mut linger_until = None;

        let finished_with_paused = summary(0, 0, 0, 1, 2, 40, 200);
        let status = next_status(&finished_with_paused, 0, &mut linger_until, t0, 0);
        assert!(matches!(status, TrayStatus::JustFinished));

        let status = next_status(
            &finished_with_paused,
            1,
            &mut linger_until,
            t0 + FINISH_LINGER,
            0,
        );
        match status {
            TrayStatus::Paused { count, pct } => {
                assert_eq!(count, 2);
                assert_eq!(pct, 20);
            }
            _ => panic!("expected Paused"),
        }
    }

    #[test]
    fn next_status_is_idle_immediately_when_a_paused_set_clears_without_a_completion() {
        // paused_count dropping to 0 (cancelled/removed, not completed)
        // with completed_count unchanged: no linger window ever opens, so
        // this reads as Idle on the very next tick, not JustFinished.
        let t0 = Instant::now();
        let mut linger_until = None;
        let cleared = summary(0, 0, 0, 3, 0, 0, 0);
        let status = next_status(&cleared, 3, &mut linger_until, t0, 0);
        assert!(matches!(status, TrayStatus::Idle));
        assert_eq!(linger_until, None);
    }

    #[test]
    fn next_status_lets_a_new_active_task_interrupt_the_linger_window() {
        let t0 = Instant::now();
        // Mid-linger from some earlier completion.
        let mut linger_until = Some(t0 + Duration::from_secs(1));

        let active = summary(1, 30, 100, 5, 0, 0, 0);
        let status = next_status(&active, 5, &mut linger_until, t0, 999);
        match status {
            TrayStatus::Active(numbers) => {
                assert_eq!(numbers.count, 1);
                assert_eq!(numbers.pct, 30);
                assert_eq!(numbers.speed_bps, 999);
            }
            _ => panic!("expected Active"),
        }
        // The stale window must not survive to resurrect JustFinished once
        // this task itself finishes.
        assert_eq!(linger_until, None);
    }

    #[test]
    fn next_status_ignores_a_completed_count_bump_while_still_active() {
        // completed_count rising alongside active_count > 0 (one task
        // finished while another is still running) must not open a linger
        // window -- the active branch returns before that check ever runs.
        let t0 = Instant::now();
        let mut linger_until = None;
        let still_active = summary(1, 10, 100, 1, 0, 0, 0);
        let status = next_status(&still_active, 0, &mut linger_until, t0, 0);
        assert!(matches!(status, TrayStatus::Active(_)));
        assert_eq!(linger_until, None);
    }

    #[test]
    fn next_status_reports_paused_with_its_own_percentage() {
        let t0 = Instant::now();
        let mut linger_until = None;
        let paused_only = summary(0, 0, 0, 0, 3, 75, 100);
        let status = next_status(&paused_only, 0, &mut linger_until, t0, 0);
        match status {
            TrayStatus::Paused { count, pct } => {
                assert_eq!(count, 3);
                assert_eq!(pct, 75);
            }
            _ => panic!("expected Paused"),
        }
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

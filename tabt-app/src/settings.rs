//! UI settings: everything the settings dialog can change except the theme (main-thread exclusive,
//! read on demand like theme) — font family/size, cursor shape and blink, content padding, window
//! opacity, scrollback depth, the login shell and where a new tab starts.
//!
//! All of it is global: a change takes effect uniformly across every terminal. The render layer
//! takes the current NSFont, cell metrics, cursor style and padding from here; the controller uses
//! them to relayout each tab's grid and notify the PTY, and `pty::spawn` reads the shell.
//!
//! Enumerated settings ([`CursorShape`], [`NewTabDir`]) carry their own persisted `name()`, because
//! `config` stores them by name rather than by index — see the note at the top of `config.rs`.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2_app_kit::NSFont;
use objc2_foundation::NSString;

/// Selectable monospace font families: classic/iconic monospace fonts, `Menlo` first because
/// it is the app's default (see `DEFAULT_FAMILY` and config.rs), the rest roughly in order of
/// how likely they are to already be installed. A font that is not installed falls back to the
/// system monospace font (SF Mono) — see `make_font`.
pub const FAMILIES: [&str; 9] = [
    "Menlo", "Monaco", "Courier New", "Courier", "Andale Mono", "Consolas",
    "Lucida Console", "Inconsolata", "Source Code Pro",
];

/// Default font family, used before ~/.tabt is loaded (matches the config load default).
pub const DEFAULT_FAMILY: &str = "Menlo";

/// Default font size, restored by ⌘0 (matches the config load default).
pub const DEFAULT_SIZE: f64 = 13.0;

/// Default inset between the terminal view's edge and the first cell.
pub const DEFAULT_PAD: f64 = 10.0;
/// Highest padding the settings dialog offers; also the parse clamp.
pub const MAX_PAD: f64 = 40.0;
/// Lowest window opacity offered — below this the text stops being readable over a busy desktop.
pub const MIN_OPACITY: f64 = 0.5;

/// How the text cursor is drawn on the active terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

impl CursorShape {
    /// In the order the settings pop-up lists them.
    pub const ALL: [CursorShape; 3] = [CursorShape::Block, CursorShape::Bar, CursorShape::Underline];

    /// The persisted name (`config`), lowercase and stable across releases.
    pub fn name(self) -> &'static str {
        match self {
            CursorShape::Block => "block",
            CursorShape::Bar => "bar",
            CursorShape::Underline => "underline",
        }
    }

    /// The pop-up's label.
    pub fn label(self) -> &'static str {
        match self {
            CursorShape::Block => "Block",
            CursorShape::Bar => "Bar",
            CursorShape::Underline => "Underline",
        }
    }

    /// Parse a persisted name; anything unrecognized is the default.
    pub fn from_name(s: &str) -> Self {
        Self::ALL.into_iter().find(|c| c.name() == s).unwrap_or(CursorShape::Block)
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or(CursorShape::Block)
    }
}

/// Which directory a newly opened tab starts in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NewTabDir {
    /// The user's home directory (what a fresh login shell would do).
    Home,
    /// The active tab's current directory, as reported by OSC 7.
    Active,
}

impl NewTabDir {
    /// In the order the settings pop-up lists them — the default first.
    pub const ALL: [NewTabDir; 2] = [NewTabDir::Active, NewTabDir::Home];

    pub fn name(self) -> &'static str {
        match self {
            NewTabDir::Home => "home",
            NewTabDir::Active => "active",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NewTabDir::Home => "Home",
            NewTabDir::Active => "Active tab’s directory",
        }
    }

    pub fn from_name(s: &str) -> Self {
        Self::ALL.into_iter().find(|d| d.name() == s).unwrap_or(NewTabDir::Active)
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|d| *d == self).unwrap_or(0)
    }

    pub fn from_index(i: usize) -> Self {
        Self::ALL.get(i).copied().unwrap_or(NewTabDir::Active)
    }
}

struct FontState {
    family: String, // font family name
    size: f64,
    regular: Retained<NSFont>,
    bold: Retained<NSFont>,
    cell_w: f64,
    line_h: f64,
}

thread_local! {
    static STATE: RefCell<Option<FontState>> = const { RefCell::new(None) };
    // Whether to draw the sidebar/header separator lines (default off).
    static SHOW_BORDER: Cell<bool> = const { Cell::new(false) };
    static CURSOR_SHAPE: Cell<CursorShape> = const { Cell::new(CursorShape::Block) };
    static CURSOR_BLINK: Cell<bool> = const { Cell::new(false) };
    // Which half of the blink cycle we are in (true = cursor shown). Driven by the controller's
    // blink timer, and forced back to true by typing/output — see `show_cursor_phase`.
    static CURSOR_PHASE: Cell<bool> = const { Cell::new(true) };
    static PAD: Cell<f64> = const { Cell::new(DEFAULT_PAD) };
    static OPACITY: Cell<f64> = const { Cell::new(1.0) };
    static SCROLLBACK: Cell<usize> = const { Cell::new(tabt_core::DEFAULT_HISTORY_MAX) };
    // Empty = resolve $SHELL at spawn time, then fall back to /bin/zsh (see pty::spawn).
    static SHELL: RefCell<String> = const { RefCell::new(String::new()) };
    // Inheriting the active tab's directory is what the app did before the setting existed, so it
    // stays the default: an upgrade must not change where ⌘T lands.
    static NEW_TAB_DIR: Cell<NewTabDir> = const { Cell::new(NewTabDir::Active) };
    // Toolbar buttons the user has switched off (see `toolbar_shows`).
    static TOOLBAR_HIDDEN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Whether a toolbar button is shown. Stored as the *hidden* set, so a button added by a later
/// version appears by default rather than staying invisible on an older config.
pub fn toolbar_shows(key: &str) -> bool {
    TOOLBAR_HIDDEN.with(|h| !h.borrow().iter().any(|k| k == key))
}

pub fn toolbar_hidden() -> Vec<String> {
    TOOLBAR_HIDDEN.with(|h| h.borrow().clone())
}

pub fn set_toolbar_hidden(v: Vec<String>) {
    TOOLBAR_HIDDEN.with(|h| *h.borrow_mut() = v);
}

/// How the text cursor is drawn.
pub fn cursor_shape() -> CursorShape {
    CURSOR_SHAPE.with(|c| c.get())
}
pub fn set_cursor_shape(v: CursorShape) {
    CURSOR_SHAPE.with(|c| c.set(v));
}

/// Whether the text cursor blinks.
pub fn cursor_blink() -> bool {
    CURSOR_BLINK.with(|c| c.get())
}
pub fn set_cursor_blink(v: bool) {
    CURSOR_BLINK.with(|c| c.set(v));
    // Turning blinking off must leave the cursor *shown*, not stuck in whatever half of the cycle
    // the timer happened to stop in.
    show_cursor_phase();
}

/// Whether the blink cycle is currently in its visible half. Always true when blinking is off, so
/// the renderer can read this alone and not special-case the setting.
pub fn cursor_phase_on() -> bool {
    !cursor_blink() || CURSOR_PHASE.with(|c| c.get())
}

/// Flip the blink phase (the blink timer's tick).
pub fn toggle_cursor_phase() {
    CURSOR_PHASE.with(|c| c.set(!c.get()));
}

/// Force the cursor visible and restart the cycle from there. Called on every keystroke and every
/// chunk of shell output: a cursor that is dark exactly while you are typing is the one thing a
/// blinking cursor must not do.
pub fn show_cursor_phase() {
    CURSOR_PHASE.with(|c| c.set(true));
}

/// Inset between the terminal view's edge and the first cell. Changing it changes how many
/// cols/rows fit, so the caller must reflow every grid (`AppController::reflow_all`).
pub fn pad() -> f64 {
    PAD.with(|c| c.get())
}
pub fn set_pad(v: f64) {
    PAD.with(|c| c.set(v.clamp(0.0, MAX_PAD)));
}

/// Window/terminal background opacity (1.0 = fully opaque). Applied to the *fills* only; glyphs
/// and the card's hairline stay opaque.
pub fn opacity() -> f64 {
    OPACITY.with(|c| c.get())
}
pub fn set_opacity(v: f64) {
    OPACITY.with(|c| c.set(v.clamp(MIN_OPACITY, 1.0)));
}

/// How many scrolled-off lines each terminal keeps.
pub fn scrollback() -> usize {
    SCROLLBACK.with(|c| c.get())
}
pub fn set_scrollback(v: usize) {
    SCROLLBACK.with(|c| c.set(v));
}

/// The configured login shell, or empty for "resolve it at spawn time".
pub fn shell() -> String {
    SHELL.with(|s| s.borrow().clone())
}
pub fn set_shell(v: &str) {
    SHELL.with(|s| *s.borrow_mut() = v.trim().to_string());
}

/// Where a newly opened tab starts.
pub fn new_tab_dir() -> NewTabDir {
    NEW_TAB_DIR.with(|c| c.get())
}
pub fn set_new_tab_dir(v: NewTabDir) {
    NEW_TAB_DIR.with(|c| c.set(v));
}

/// Initialize / update the current font.
pub fn set(family: &str, size: f64) {
    let size = size.clamp(8.0, 40.0);
    let regular = make_font(family, size, false);
    let bold = make_font(family, size, true);
    let (cell_w, line_h) = crate::view::cell_metrics(&regular);
    STATE.with(|s| {
        *s.borrow_mut() = Some(FontState {
            family: family.to_string(),
            size,
            regular,
            bold,
            cell_w,
            line_h,
        });
    });
}

fn make_font(family: &str, size: f64, bold: bool) -> Retained<NSFont> {
    let weight = if bold { 0.4 } else { 0.0 };
    unsafe {
        // Fall back to the system monospace font if the family is not installed; the bold
        // variant of a named font is not required.
        NSFont::fontWithName_size(&NSString::from_str(family), size)
            .unwrap_or_else(|| NSFont::monospacedSystemFontOfSize_weight(size, weight))
    }
}

fn with<T>(f: impl FnOnce(&FontState) -> T) -> T {
    STATE.with(|s| f(s.borrow().as_ref().expect("settings not initialized")))
}

pub fn font() -> Retained<NSFont> {
    with(|s| s.regular.clone())
}
pub fn font_bold() -> Retained<NSFont> {
    with(|s| s.bold.clone())
}
pub fn cell_w() -> f64 {
    with(|s| s.cell_w)
}
pub fn line_h() -> f64 {
    with(|s| s.line_h)
}
pub fn size() -> f64 {
    with(|s| s.size)
}
pub fn family() -> String {
    with(|s| s.family.clone())
}

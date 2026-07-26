//! UI settings: global font family + font size (main-thread exclusive, read on demand like theme).
//!
//! The font is global: changing the size/font takes effect uniformly across all terminals.
//! The render layer takes the current NSFont and cell metrics from here; the controller uses
//! them to relayout each tab's grid and notify the PTY.

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
}

/// Whether the sidebar/header separator borders are drawn.
pub fn show_border() -> bool {
    SHOW_BORDER.with(|c| c.get())
}
pub fn set_show_border(v: bool) {
    SHOW_BORDER.with(|c| c.set(v));
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

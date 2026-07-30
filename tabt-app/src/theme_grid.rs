//! Theme picker: a grid of live preview swatches, one card per theme in the registry.
//!
//! Replaces the settings dialog's theme pop-up, which named twelve themes and showed none of them.
//! Each card is painted *in* the theme it selects — that theme's background, its foreground for the
//! name, six of its ANSI colors as chips — so the choice is made by looking rather than by
//! recognizing a name. Self-drawn like the sidebar: the standard AppKit controls the rest of
//! `settings_dialog.rs` uses have nothing that previews a color scheme.
//!
//! The view sizes itself to the whole registry ([`ThemeGrid::height_for_width`]) and is mounted in
//! an `NSScrollView`, because the registry is a user-editable file (`themes.conf`) and can hold any
//! number of themes.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSColor, NSEvent, NSFont, NSFontWeightSemibold, NSStringDrawing, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::app::AppController;
use crate::theme::{self, Rgb, Theme};
use crate::view::{draw_truncated, make_attrs, ns_color, rect, round_fill, round_stroke};

/// Cards per row. Two, not three: at the settings panel's width three cards are too narrow for
/// the longest shipped theme name ("Catppuccin Mocha"), and a preview whose label is elided is
/// most of the way back to the pop-up this replaced.
const COLS: usize = 2;
/// Tall enough for the miniature (sidebar + three terminal lines) plus the name/chips footer.
const CARD_H: f64 = 106.0;
/// Padding inside a card.
const PAD: f64 = 10.0;
/// The miniature app window inside a card: its height, and the width of its sidebar.
const MINI_H: f64 = 60.0;
const SIDE_W: f64 = 48.0;
const GAP: f64 = 8.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 12.0;
const RADIUS: f64 = 7.0;
/// One ANSI color chip. Kept small: the chips share the footer with the theme name, and the
/// longest shipped name ("Catppuccin Mocha") must not be elided to make room for them.
const CHIP: f64 = 9.0;
const CHIP_GAP: f64 = 5.0;
/// The palette entries previewed, in this order: red, green, yellow, blue, magenta, cyan. The
/// six that carry a theme's character; black and white say nothing next to the background.
const CHIP_COLORS: [usize; 6] = [1, 2, 3, 4, 5, 6];

/// No card under the pointer. A sentinel rather than an `Option` so the whole thing stays a `Cell`.
const NO_HOVER: usize = usize::MAX;

pub struct ThemeGridIvars {
    controller: Cell<*const AppController>,
    selected: Cell<usize>,
    hover: Cell<usize>,
    tracking_added: Cell<bool>,
}

declare_class!(
    pub struct ThemeGrid;

    // SAFETY: plain NSView subclass, main-thread only like every view here, no Drop.
    unsafe impl ClassType for ThemeGrid {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ThemeGrid";
    }

    impl DeclaredClass for ThemeGrid {
        type Ivars = ThemeGridIvars;
    }

    unsafe impl NSObjectProtocol for ThemeGrid {}

    unsafe impl ThemeGrid {
        // Top-down, like the rest of the app's self-drawn views: card 0 is the first one you see,
        // and a scroll view starts at the top of a flipped document view rather than the bottom.
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.local_point(event);
            if let Some(i) = self.index_at(p) {
                self.select(i);
            }
        }

        #[method(mouseMoved:)]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.local_point(event);
            self.set_hover(self.index_at(p).unwrap_or(NO_HOVER));
        }

        #[method(mouseExited:)]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_hover(NO_HOVER);
        }

        /// `InVisibleRect` makes the area follow the view, so scrolling and resizing never need it
        /// rebuilt — and `MouseExited` is what clears the hover when the pointer leaves the pane.
        #[method(updateTrackingAreas)]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            if self.ivars().tracking_added.get() {
                return;
            }
            let mtm = MainThreadMarker::new().expect("main thread");
            let opts = NSTrackingAreaOptions::NSTrackingMouseMoved
                | NSTrackingAreaOptions::NSTrackingMouseEnteredAndExited
                | NSTrackingAreaOptions::NSTrackingActiveInKeyWindow
                | NSTrackingAreaOptions::NSTrackingInVisibleRect;
            let owner: &AnyObject = unsafe { &*(self as *const Self as *const AnyObject) };
            let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(mtm.alloc(), zero, opts, Some(owner), None)
            };
            unsafe { self.addTrackingArea(&area) };
            self.ivars().tracking_added.set(true);
        }
    }
);

impl ThemeGrid {
    pub fn new(mtm: MainThreadMarker, width: f64) -> Retained<Self> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, Self::height_for_width(width)));
        let this = mtm.alloc();
        let this = this.set_ivars(ThemeGridIvars {
            controller: Cell::new(std::ptr::null()),
            selected: Cell::new(usize::MAX),
            hover: Cell::new(NO_HOVER),
            tracking_added: Cell::new(false),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    pub fn set_controller(&self, c: *const AppController) {
        self.ivars().controller.set(c);
    }

    /// How tall the whole grid is at `width` — what the scroll view scrolls through.
    pub fn height_for_width(_width: f64) -> f64 {
        let rows = theme::count().div_ceil(COLS).max(1) as f64;
        2.0 * PAD_Y + rows * (CARD_H + GAP) - GAP
    }

    /// Mark `idx` as the current theme (the dialog calls this when it re-seeds).
    pub fn set_selected(&self, idx: usize) {
        if self.ivars().selected.get() != idx {
            self.ivars().selected.set(idx);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn select(&self, idx: usize) {
        self.set_selected(idx);
        let p = self.ivars().controller.get();
        if !p.is_null() {
            unsafe { &*p }.set_style(idx);
        }
    }

    fn set_hover(&self, idx: usize) {
        if self.ivars().hover.get() != idx {
            self.ivars().hover.set(idx);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn local_point(&self, event: &NSEvent) -> NSPoint {
        let win = unsafe { event.locationInWindow() };
        self.convertPoint_fromView(win, None)
    }

    fn index_at(&self, p: NSPoint) -> Option<usize> {
        let w = self.frame().size.width;
        (0..theme::count()).find(|i| {
            let r = card_rect(*i, w);
            p.x >= r.origin.x
                && p.x <= r.origin.x + r.size.width
                && p.y >= r.origin.y
                && p.y <= r.origin.y + r.size.height
        })
    }

    fn render(&self) {
        let w = self.frame().size.width;
        let name_font = unsafe { NSFont::systemFontOfSize_weight(11.5, NSFontWeightSemibold) };
        let mono = unsafe { NSFont::monospacedSystemFontOfSize_weight(9.0, 0.0) };
        let (cw, lh) = crate::view::cell_metrics(&mono);
        let selected = self.ivars().selected.get();
        let hover = self.ivars().hover.get();
        let names = theme::names(); // one registry read for the whole grid

        for i in 0..names.len() {
            let t = theme::by_index(i);
            let r = card_rect(i, w);
            let (x0, y0) = (r.origin.x, r.origin.y);

            // The card is the theme, laid out like the app itself: terminal background, a recessed
            // sidebar beside it, text in the colors that theme would actually use.
            round_fill(r, RADIUS, &ns_color(t.bg));
            round_stroke(inset(r, 0.5), RADIUS, 1.0, &ns_color(t.card_border()));

            // ---- Miniature sidebar: the card tone, a row highlight, three session rows ----
            let side = rect(x0 + PAD, y0 + PAD, SIDE_W, MINI_H);
            round_fill(side, 5.0, &ns_color(t.card_bg()));
            round_stroke(inset(side, 0.5), 5.0, 1.0, &ns_color(t.card_border()));
            for row in 0..3 {
                let ry = side.origin.y + 11.0 + row as f64 * 13.0;
                let bar = rect(side.origin.x + 7.0, ry, SIDE_W - 14.0, 6.0);
                // The first row is the selected session, the tier the sidebar itself uses.
                let tone = theme::mix(t.bg, t.fg, if row == 0 { 0.34 } else { 0.16 });
                round_fill(bar, 3.0, &ns_color(tone));
            }

            // ---- Miniature terminal: a prompt, a listing, a cursor ----
            let tx = x0 + PAD + SIDE_W + 9.0;
            let ty = y0 + PAD + 7.0;
            let dim = theme::mix(t.fg, t.bg, 0.45);
            let (blue, green) = (sample(&t, 4, 0.55), sample(&t, 2, 0.25));
            let mut x = draw_run(&mono, "~/tabt", tx, ty, cw, blue);
            x = draw_run(&mono, " $ ", x, ty, cw, green);
            draw_run(&mono, "ls", x, ty, cw, t.fg);

            let y2 = ty + lh;
            let x = draw_run(&mono, "bundle", tx, y2, cw, blue);
            draw_run(&mono, "  README.md", x, y2, cw, dim);

            let y3 = ty + 2.0 * lh;
            let mut x = draw_run(&mono, "~/tabt", tx, y3, cw, blue);
            x = draw_run(&mono, " $ ", x, y3, cw, green);
            // The cursor is a filled cell, exactly as the terminal draws a block cursor.
            round_fill(rect(x, y3 + 1.0, cw, lh - 3.0), 1.0, &ns_color(t.fg));

            // ---- Name + ANSI chips, on one line under the miniature ----
            let foot_y = y0 + PAD + MINI_H + 9.0;
            let chips_w = CHIP_COLORS.len() as f64 * (CHIP + CHIP_GAP) - CHIP_GAP;
            let label = rect(x0 + PAD, foot_y, r.size.width - 2.0 * PAD - chips_w - 10.0, 16.0);
            draw_truncated(&names[i], label, &name_font, t.fg);
            for (k, idx) in CHIP_COLORS.iter().enumerate() {
                let c = sample(&t, *idx, k as f64 * 0.14);
                let cx = x0 + r.size.width - PAD - chips_w + k as f64 * (CHIP + CHIP_GAP);
                round_fill(rect(cx, foot_y + 3.0, CHIP, CHIP), 2.5, &ns_color(c));
            }

            // Selection ring outside the card, in the system accent color — the one thing here that
            // must read the same on every theme, so it is deliberately not theme-derived.
            if i == selected {
                let ring = unsafe { NSColor::controlAccentColor() };
                round_stroke(outset(r, 2.0), RADIUS + 2.0, 2.0, &ring);
            } else if i == hover {
                let faint = unsafe { NSColor::controlAccentColor().colorWithAlphaComponent(0.45) };
                round_stroke(outset(r, 2.0), RADIUS + 2.0, 1.5, &faint);
            }
        }
    }
}

/// One color of the theme's palette, or — for a monochrome phosphor theme, which ignores SGR
/// entirely and would be misrepresented by its unused palette — the phosphor dimmed by `mono_dim`.
fn sample(t: &Theme, idx: usize, mono_dim: f64) -> Rgb {
    if t.mono {
        return theme::mix(t.fg, t.bg, mono_dim);
    }
    let (r, g, b) = t.palette[idx];
    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

/// Draw one run of the miniature's monospace text and return the x the next run starts at.
/// Advancing by cell width rather than by measured text keeps the runs on the terminal's own grid.
fn draw_run(font: &Retained<NSFont>, text: &str, x: f64, y: f64, cw: f64, color: Rgb) -> f64 {
    let attrs = make_attrs(font, Some(&ns_color(color)));
    unsafe { NSString::from_str(text).drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&attrs)) };
    x + text.chars().count() as f64 * cw
}

/// Where card `i` sits in a grid `w` points wide.
fn card_rect(i: usize, w: f64) -> NSRect {
    let card_w = ((w - 2.0 * PAD_X - (COLS as f64 - 1.0) * GAP) / COLS as f64).max(1.0);
    let (col, row) = (i % COLS, i / COLS);
    rect(
        PAD_X + col as f64 * (card_w + GAP),
        PAD_Y + row as f64 * (CARD_H + GAP),
        card_w,
        CARD_H,
    )
}

/// `r` shrunk by `d` on every side — a stroke is centered on its path, so a border drawn on the
/// card's own bounds would be half outside it.
fn inset(r: NSRect, d: f64) -> NSRect {
    rect(r.origin.x + d, r.origin.y + d, r.size.width - 2.0 * d, r.size.height - 2.0 * d)
}

/// `r` grown by `d` on every side (the selection ring sits outside the card).
fn outset(r: NSRect, d: f64) -> NSRect {
    inset(r, -d)
}

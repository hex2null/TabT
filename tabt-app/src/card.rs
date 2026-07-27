//! The floating sidebar card: the rounded, inset panel the sidebar draws inside.
//!
//! The card is a plain layer-backed `NSView` whose layer carries the whole look — fill, hairline
//! border, corner radius, drop shadow — so `SidebarView` can go on painting only its rows. The fill
//! is the theme's plain background, the same color the terminal draws on, so the window reads as one
//! continuous surface; the card is told apart by its border and shadow alone, not by a lighter tint.
//!
//! The layer deliberately does **not** mask to its bounds — masking would clip the shadow away.
//! Nothing the sidebar draws reaches the rounded corners (rows and boxes are inset by `HPAD`, and
//! the footer separator sits far from them), so there is nothing to clip.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSColor, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use crate::theme;
use crate::view::ns_color;

// Spacing sits on AppKit's usual 4/8/12 scale: an 8pt gutter all round, and the same 8pt between
// the card and the terminal, so the card reads as floating without opening a wide moat.
/// Gutter between the card and the window edges (left/top/bottom when docked left).
pub const CARD_INSET: f64 = 8.0;
/// Gap between the card and the terminal pane.
pub const CARD_GAP: f64 = 8.0;
/// Corner radius of the card.
///
/// Concentric with the window: macOS rounds the window itself at ~24pt, and an inset shape stays
/// concentric with its container at `outer − inset`. Anything smaller reads as a chip sitting in
/// the corner rather than a panel following it.
const CARD_RADIUS: f64 = 24.0 - CARD_INSET;

/// Build the card and mount `content` inside it, filling the card's bounds.
pub fn new(mtm: MainThreadMarker, frame: NSRect, content: &NSView) -> Retained<NSView> {
    let card: Retained<NSView> = unsafe { NSView::initWithFrame(mtm.alloc(), frame) };
    unsafe {
        card.setWantsLayer(true);
        content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        content.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewWidthSizable | NSAutoresizingMaskOptions::NSViewHeightSizable,
        );
        card.addSubview(content);
    }
    apply_theme(&card);
    card
}

/// Repaint the card's fill and border for the current theme. Called on every theme change — the
/// layer holds concrete colors, so unlike the `drawRect:`-based views it does not re-read the
/// theme on its own.
pub fn apply_theme(card: &NSView) {
    let t = theme::current();
    unsafe {
        let layer: *mut AnyObject = msg_send![card, layer];
        if layer.is_null() {
            return;
        }
        let _: () = msg_send![layer, setCornerRadius: CARD_RADIUS];
        let _: () = msg_send![layer, setMasksToBounds: false];
        let _: () = msg_send![layer, setBackgroundColor: cg(&ns_color(t.bg))];
        let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        let _: () = msg_send![layer, setBorderColor: cg(&ns_color(t.card_border()))];
        // A soft ambient shadow on all four sides: no offset, so it reads as the card lifted off
        // the background rather than as a light source somewhere off-screen.
        //
        // The radius is deliberately much wider than the 8pt gutter it falls into. A tight, opaque
        // shadow would still be dark where the window edge cuts it off, leaving a visible band with
        // a hard outer edge; spreading it wide and keeping it faint means it has already faded to
        // near nothing by the time it is clipped, so the transition reads as a gradient rather than
        // a border. Dark themes carry more of it — a black shadow on a near-black backdrop barely
        // registers otherwise.
        let _: () = msg_send![layer, setShadowColor: cg(&ns_color((0.0, 0.0, 0.0)))];
        let _: () = msg_send![layer, setShadowOpacity: if t.is_dark() { 0.28f32 } else { 0.10f32 }];
        let _: () = msg_send![layer, setShadowRadius: 20.0f64];
        let _: () = msg_send![layer, setShadowOffset: NSSize::new(0.0, 0.0)];
    }
}

/// `NSColor` -> `CGColorRef`, for the layer properties above.
unsafe fn cg(color: &NSColor) -> *mut AnyObject {
    msg_send![color, CGColor]
}

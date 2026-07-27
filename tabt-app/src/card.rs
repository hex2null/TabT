//! The floating sidebar card: the rounded, inset panel the sidebar draws inside.
//!
//! The card is a plain layer-backed `NSView` whose layer carries the whole look — fill, hairline
//! border, corner radius — so `SidebarView` can go on painting only its rows.
//!
//! Every surface in the window is the theme's plain background: the card's fill, the terminal
//! beside it, and the gutter around it (the window's own background color). The card is told apart
//! by its hairline alone. It carries no drop shadow for the same reason — a shadow tints the gutter
//! it falls into, and a gutter darker than the panes on either side is exactly the seam this layout
//! is trying not to have.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSColor, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect};

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
        // Safe to clip now that no shadow has to escape the bounds, and it keeps whatever the
        // sidebar draws inside the rounded corners.
        let _: () = msg_send![layer, setMasksToBounds: true];
        let _: () = msg_send![layer, setBackgroundColor: cg(&ns_color(t.bg))];
        let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        let _: () = msg_send![layer, setBorderColor: cg(&ns_color(t.card_border()))];
    }
}

/// `NSColor` -> `CGColorRef`, for the layer properties above.
unsafe fn cg(color: &NSColor) -> *mut AnyObject {
    msg_send![color, CGColor]
}

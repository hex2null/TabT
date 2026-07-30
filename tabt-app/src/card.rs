//! The floating sidebar card: the rounded, inset panel the sidebar draws inside.
//!
//! The card is a plain layer-backed `NSView` whose layer carries the whole look — fill, hairline
//! border, corner radius — so `SidebarView` can go on painting only its rows.
//!
//! The terminal and the gutter around the card are the theme's plain background; the card itself is
//! that background stepped one notch deeper ([`theme::Theme::card_bg`]), so the panel reads as its
//! own recessed surface. The hairline is left much fainter than that step — it only finishes the
//! rounded edge — and a tight, very faint drop shadow lifts the card off the gutter.
//!
//! The shadow is deliberately near the limit of visibility, and its spread stays close to CARD_GAP:
//! a heavier or wider one tints the gutter it falls into, and that gutter is shared with the
//! terminal, so it turns into a dark seam down the middle of the window. Weak and short, it reads
//! as depth instead.
//!
//! A layer's shadow is drawn outside its bounds, so `masksToBounds` would erase it. The clip that
//! keeps the sidebar's own drawing inside the rounded corners therefore lives on the *content*
//! layer, which has the card's exact bounds, while the card layer stays unmasked to let the shadow
//! out.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSColor, NSView};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

use crate::theme;
use crate::view::{ns_color, ns_color_bg};

// Spacing sits on AppKit's usual 4/8/12 scale: an 8pt gutter all round, and the same 8pt between
// the card and the terminal, so the card reads as floating without opening a wide moat.
/// Gutter between the card and the window edges (left/top/bottom when docked left).
pub const CARD_INSET: f64 = 8.0;
/// Gap between the card and the terminal pane.
pub const CARD_GAP: f64 = 8.0;
/// Blur radius of the card's drop shadow.
const SHADOW_RADIUS: f64 = 7.0;
/// The same on a light theme, where the falloff is the part that is actually visible (see
/// `apply_theme`) and so has room to be a little wider.
const SHADOW_RADIUS_LIGHT: f64 = 9.0;
/// How far the shadow can still be seen past the card's own edge. The blur is Gaussian, so it does
/// not stop dead at the radius; `relayout` has to park the collapsed card at least this far
/// off-screen or the tail of the shadow stays visible as a smudge down the window edge. Taken from
/// the wider of the two radii, since the parking distance cannot depend on the theme in force.
pub const SHADOW_REACH: f64 = SHADOW_RADIUS_LIGHT * 2.0;
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
        // The rounded clip lives here rather than on the card, so the card's shadow can escape its
        // bounds. Both layers carry the same radius and the same frame, so the seam is invisible.
        content.setWantsLayer(true);
        let content_layer: *mut AnyObject = msg_send![content, layer];
        if !content_layer.is_null() {
            let _: () = msg_send![content_layer, setCornerRadius: CARD_RADIUS];
            let _: () = msg_send![content_layer, setMasksToBounds: true];
        }
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
        // Unmasked on purpose: the clip that keeps the sidebar's drawing inside the corners is on
        // the content layer (see `new`), because masking here would cut the shadow off.
        let _: () = msg_send![layer, setMasksToBounds: false];
        let _: () = msg_send![layer, setBackgroundColor: cg(&ns_color_bg(t.card_bg()))];
        let _: () = msg_send![layer, setBorderWidth: 1.0f64];
        let _: () = msg_send![layer, setBorderColor: cg(&ns_color(t.card_border()))];
        // Weak and tight, offset a hair downward: enough to lift the card, not enough to darken the
        // gutter it shares with the terminal — the spread is kept close to CARD_GAP so the gradient
        // has faded out by the time it reaches the text. Black on every theme: a light theme's
        // gutter needs the same "something in front of something" cue, and tinting the shadow with
        // the theme would only make it a colored smudge.
        //
        // A touch stronger on a light theme, for the same reason `Theme::card_border` steps less
        // there: black over a light gutter is the only case where the falloff has room to be read as
        // a gradient. Over a dark one it is nearly black on black, so raising it would darken the
        // seam without ever looking like a shadow.
        let _: () = msg_send![layer, setShadowColor: cg(&NSColor::blackColor())];
        let (opacity, radius): (f32, f64) = if t.is_dark() {
            (0.06, SHADOW_RADIUS)
        } else {
            (0.09, SHADOW_RADIUS_LIGHT)
        };
        let _: () = msg_send![layer, setShadowOpacity: opacity];
        let _: () = msg_send![layer, setShadowRadius: radius];
        let _: () = msg_send![layer, setShadowOffset: NSSize::new(0.0, -1.0)];
    }
}

/// `NSColor` -> `CGColorRef`, for the layer properties above.
unsafe fn cg(color: &NSColor) -> *mut AnyObject {
    msg_send![color, CGColor]
}

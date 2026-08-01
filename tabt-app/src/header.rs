//! The band under the toolbar, across the top of the terminal pane.
//!
//! The window's title bar is transparent and full-size, so this view is what gives the top strip
//! the terminal's own background instead of the system's, and it draws the active session name and
//! its `~ · zsh` meta string on top of that. The sidebar toggle beside them is not drawn here: it
//! is a real `NSToolbar` item (see `toolbar.rs`), and the toolbar's band is laid over this one.
//!
//! The title is drawn here rather than made a toolbar item of its own because it has to start at
//! the *terminal's* left edge, which moves with the sidebar. A toolbar spans the window, and AppKit
//! measures a custom item view once, when the item is inserted — a title item could be placed but
//! never re-placed, so it would part company with the terminal the first time the sidebar moved.
//! This view already spans exactly the host, so the same alignment costs nothing.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSEvent, NSFont, NSRectFill, NSStringDrawing, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSString};

use crate::theme;
use crate::view::{make_attrs, ns_color, ns_color_bg};

pub const HEADER_H: f64 = 44.0; // fallback band height, used until the window can report the real
                                // one — with a toolbar attached that height is the system's, see
                                // `AppController::band_h`

pub struct HeaderIvars {
    title: RefCell<String>,
    /// The dimmed line after the name: where the session is and what it is running.
    meta: RefCell<String>,
    font: Retained<NSFont>,     // session name (semibold)
    font_sub: Retained<NSFont>, // meta / "running"
    // Left inset for the title. When the sidebar is collapsed the traffic lights + toggle
    // sit at the window's top-left over this pane, so the title must clear them.
    left_inset: Cell<f64>,
    // Centerline the title sits on, measured down from this view's top. The controller sets it to
    // the same value the traffic lights and the collapse toggle use, so the top strip reads as one
    // line — it shifts with the sidebar card, which is inset from the window's top edge.
    center_y: Cell<f64>,
}

declare_class!(
    pub struct HeaderView;

    unsafe impl ClassType for HeaderView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "HeaderView";
    }

    impl DeclaredClass for HeaderView {
        type Ivars = HeaderIvars;
    }

    unsafe impl NSObjectProtocol for HeaderView {}

    unsafe impl HeaderView {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        // Title-bar behavior for whatever part of the band the toolbar does not cover: double-click
        // zooms (maximize/restore) the window like the native title bar; a single click/drag moves
        // it. Clicks that land on the toolbar are AppKit's, and it does the same two things.
        #[method(mouseDown:)]
        fn mouse_down(&self, event: &NSEvent) {
            let window = match self.window() {
                Some(w) => w,
                None => return,
            };
            if unsafe { event.clickCount() } >= 2 {
                window.zoom(None);
            } else {
                let _: () = unsafe { msg_send![&*window, performWindowDragWithEvent: event] };
            }
        }
    }
);

impl HeaderView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let font = unsafe { NSFont::systemFontOfSize(12.0) }; // regular weight (not bold)
        let font_sub = unsafe { NSFont::systemFontOfSize(10.5) };
        let this = mtm.alloc();
        let this = this.set_ivars(HeaderIvars {
            title: RefCell::new(String::new()),
            font,
            font_sub,
            meta: RefCell::new(String::new()),
            left_inset: Cell::new(16.0),
            center_y: Cell::new(HEADER_H / 2.0),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
    }

    /// Update the active session name shown on the left.
    pub fn set_title(&self, name: &str) {
        *self.ivars().title.borrow_mut() = name.to_string();
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Update the dimmed meta line after the name. Separate from `set_title` because it changes far
    /// more often — every `cd` — and must not drag the window's title bar along with it.
    pub fn set_meta(&self, meta: &str) {
        if *self.ivars().meta.borrow() == meta {
            return;
        }
        *self.ivars().meta.borrow_mut() = meta.to_string();
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Set the title's left inset (larger when the sidebar is collapsed, to clear the traffic
    /// lights and the toolbar's toggle, which then overlay this pane).
    pub fn set_left_inset(&self, x: f64) {
        if self.ivars().left_inset.get() != x {
            self.ivars().left_inset.set(x);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    /// Put the title on `y` (measured down from this view's top) — the centerline shared with the
    /// traffic lights and the toolbar's own items.
    pub fn set_center_y(&self, y: f64) {
        if self.ivars().center_y.get() != y {
            self.ivars().center_y.set(y);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn render(&self) {
        let b = self.bounds();
        let t = theme::current();
        unsafe {
            // Background matches the terminal exactly (theme bg), and nothing else: the band and the
            // terminal under it are one surface, so a rule between them would only draw a line
            // across the middle of it.
            ns_color_bg(t.bg).set();
            NSRectFill(b);
        }

        // Colors derive from the theme so the title stays legible on light and dark themes.
        let lx = self.ivars().left_inset.get();
        let title = self.ivars().title.borrow().clone();
        // Session name (theme foreground).
        let name_attrs = make_attrs(&self.ivars().font, Some(&ns_color(t.fg)));
        let name = NSString::from_str(&title);
        let cy = self.ivars().center_y.get();
        unsafe { name.drawAtPoint_withAttributes(NSPoint::new(lx, cy - 8.0), Some(&name_attrs)) };
        let name_w = unsafe { name.sizeWithAttributes(Some(&name_attrs)).width };

        // Meta string after the name (dimmed toward the background): the session's directory and
        // shell, or that it has ended. Was a hard-coded "~ · zsh" until the app tracked either.
        let meta_attrs = make_attrs(&self.ivars().font_sub, Some(&ns_color(theme::mix(t.fg, t.bg, 0.50))));
        let meta = NSString::from_str(&self.ivars().meta.borrow());
        unsafe { meta.drawAtPoint_withAttributes(NSPoint::new(lx + name_w + 10.0, cy - 7.5), Some(&meta_attrs)) };
    }
}

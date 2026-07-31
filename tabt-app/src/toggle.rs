//! The card's top-strip icon buttons: a system-style SF Symbol on the strip the traffic lights
//! share, with no background of its own.
//!
//! Two of them, both [`StripButton`]s differing only in their [`StripAction`]: **Search** (the
//! magnifier, which opens the search box — hidden until then) and **Collapse** (`sidebar.left`, the
//! same glyph as the show/hide-sidebar button in Finder/Mail). They sit at the end of the strip
//! furthest from the traffic lights, search first.
//!
//! They only exist while the sidebar is showing. `relayout` parks them past the window edge with
//! the card, so they slide out on a collapse rather than blinking away; the button that brings the
//! sidebar *back* is the toolbar's (see `toolbar.rs`), which is a different button by design —
//! these belong to the card, and there is no card to put them on once it is gone.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{NSEvent, NSView};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSRect, NSString};

use crate::app::AppController;
use crate::theme;
use crate::view::{draw_symbol, rect};

/// Glyph box. The button's own frame is wider (see `relayout`), so the two sit as an icon row
/// rather than as two touching squares.
pub const TOGGLE_W: f64 = 22.0;

/// What a button does, and which symbol says so.
#[derive(Clone, Copy, PartialEq)]
pub enum StripAction {
    Search,
    Collapse,
}

impl StripAction {
    fn symbol(self) -> &'static str {
        match self {
            StripAction::Search => "magnifyingglass",
            StripAction::Collapse => "sidebar.left",
        }
    }

    /// Tooltip, with the keyboard equivalent — the search box used to advertise ⌘F on its own face,
    /// and now that the box only appears once search is on, this is where that hint lives.
    fn tooltip(self) -> &'static str {
        match self {
            StripAction::Search => "Search Sessions (⌘F)",
            StripAction::Collapse => "Hide Sidebar (⌘B)",
        }
    }
}

pub struct ToggleIvars {
    controller: Cell<*const AppController>,
    action: Cell<StripAction>,
}

declare_class!(
    pub struct StripButton;

    unsafe impl ClassType for StripButton {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "StripButton";
    }

    impl DeclaredClass for StripButton {
        type Ivars = ToggleIvars;
    }

    unsafe impl NSObjectProtocol for StripButton {}

    unsafe impl StripButton {
        #[method(isFlipped)]
        fn is_flipped(&self) -> bool {
            true
        }

        #[method(drawRect:)]
        fn draw_rect(&self, _dirty: NSRect) {
            self.render();
        }

        #[method(mouseDown:)]
        fn mouse_down(&self, _event: &NSEvent) {
            let Some(c) = self.controller() else { return };
            match self.ivars().action.get() {
                // Both are the same calls the menu items make, so the sidebar cannot end up in a
                // state one route can reach and the other cannot.
                StripAction::Search => c.focus_search(),
                StripAction::Collapse => c.toggle_sidebar(),
            }
        }
    }
);

impl StripButton {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, action: StripAction) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(ToggleIvars {
            controller: Cell::new(std::ptr::null()),
            action: Cell::new(action),
        });
        let this: Retained<Self> = unsafe { msg_send_id![super(this), initWithFrame: frame] };
        unsafe { this.setToolTip(Some(&NSString::from_str(action.tooltip()))) };
        this
    }

    pub fn set_controller(&self, c: *const AppController) {
        self.ivars().controller.set(c);
    }

    fn controller(&self) -> Option<&AppController> {
        let p = self.ivars().controller.get();
        if p.is_null() {
            None
        } else {
            Some(unsafe { &*p })
        }
    }

    fn render(&self) {
        let b = self.bounds();
        // No background, blends into the title bar; system symbols, so both read as the same
        // control the native apps put here. The tone is the theme's, dimmed by the same fraction
        // the toolbar's own icons use — the two sets share a row whenever the sidebar is collapsed
        // and back, so one fixed gray for one of them would show.
        let t = theme::current();
        let s = 17.0;
        draw_symbol(
            self.ivars().action.get().symbol(),
            rect((b.size.width - s) / 2.0, (b.size.height - s) / 2.0, s, s),
            theme::mix(t.fg, t.bg, crate::toolbar::ICON_DIM),
        );
    }
}

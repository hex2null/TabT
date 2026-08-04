//! Sidebar: self-drawn group + tab list.
//!
//! A custom NSView that draws group titles and tab rows itself, and handles clicks and drags itself:
//!   - two buttons at the top: "＋ New Terminal" and "＋ New Group";
//!   - each group has one title row, with its tabs listed indented below; click a tab to switch,
//!     drag a tab to another group to move it.
//! All actions are forwarded to [`AppController`](crate::app::AppController). The view only takes
//! keyboard focus while its search box or an in-place rename box is active (see `acceptsFirstResponder`);
//! the rest of the time focus stays on the terminal, and a click outside the box hands it straight back.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventModifierFlags, NSFont, NSFontWeightSemibold,
    NSGraphicsContext, NSMenu, NSMenuItem, NSPasteboard, NSPasteboardTypeString,
    NSRectClip, NSRectFill, NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::app::{AppController, SessionState, Snapshot, TabSnap};
use crate::card::CARD_INSET;
use crate::config;
use crate::header::HEADER_H;
use crate::theme;
use crate::view::{self, draw_symbol, draw_truncated, make_attrs, ns_color, rect, round_fill, round_stroke, TimerToken};

/// Default sidebar width; `MIN_SIDEBAR_W`..`MAX_SIDEBAR_W` bound the divider drag. These mirror the
/// system sidebar metrics a `NavigationSplitView` asks for (`columnWidth(min: 200, ideal: 240)`).
pub const SIDEBAR_W: f64 = 240.0;
pub const MIN_SIDEBAR_W: f64 = 200.0;
pub const MAX_SIDEBAR_W: f64 = 480.0;
const ROW_H: f64 = 32.0; // session/settings row (per design spec)
const SECTION_H: f64 = 24.0; // "Sessions" section label row above the ungrouped tabs
const BTN_H: f64 = 28.0; // action button row — the search box's height, so the two chips at the
                         // top of the card read as one size rather than nearly one
const SEARCH_H: f64 = 28.0;
/// Room reserved inside the search box's right edge for the "esc" hint, so a long query scrolls
/// under its own clip rather than under the badge.
const ESC_HINT_W: f64 = 24.0;
/// How long the search box takes to unroll or fold away, in seconds. A shade quicker than the
/// sidebar's own slide (`SIDEBAR_ANIM`): that one moves the whole panel, this one moves one row.
const SEARCH_ANIM: f64 = 0.16;
/// Animation tick. ~60 Hz, which is what a 160 ms motion needs to read as motion rather than steps.
const ANIM_MS: u64 = 16;
const PAD: f64 = 14.0; // content left inset
/// Top strip of the card, holding the traffic lights and the collapse toggle. The card's top
/// edge already sits CARD_INSET below the window's, so this is that much shorter than HEADER_H.
const TOP_INSET: f64 = HEADER_H - CARD_INSET;
const HPAD: f64 = 10.0; // row background (selected/hover/search box) inset from the sidebar's left and right edges
const ACTIONS_GAP: f64 = 8.0; // between the two buttons of the "Terminal"/"Group" row
/// Width of the "Group" button. It is icon-only, so it is square on `BTN_H` rather than taking half
/// the row: the two are not peers — a new terminal is what that row is mostly for, and a group is the
/// occasional one — and the label it dropped is width "Terminal" can use.
const ACTIONS_ICON_W: f64 = BTN_H;
/// The "chip" fill shared by the search box, the two action buttons and the selected session row,
/// so a selected session and a button read as the same surface. `CHIP_HOVER` is the same chip under
/// the pointer. `ROW_HOVER` is every *list* row's hover wash — one step below the chip, so hovering
/// a group title or an unselected session never paints it exactly like the selected row.
const CHIP_BG: f64 = 0.06;
const CHIP_HOVER: f64 = 0.10;
const ROW_HOVER: f64 = 0.04;
const GAP: f64 = 10.0;
const FROW_H: f64 = 32.0; // bottom settings row (same height as session rows)
const FPAD: f64 = 8.0; // settings row top/bottom margin (symmetric)
/// `group` sentinel for ungrouped tab rows (distinct from the button's usize::MAX).
const UNGROUPED: usize = usize::MAX - 1;
/// How many ⌘Z steps the search/rename boxes keep (they hold one short line, so this is plenty).
const UNDO_DEPTH: usize = 64;

/// The session icon's box in a tab row. Wider than the old 14pt terminal glyph because it stands
/// where the status dot used to as well, and it is now the row's one interactive mark.
const ICON_W: f64 = 15.0;
const ICON_H: f64 = 14.0;

/// The symbol the session icon draws, resolved once.
///
/// `apple.terminal` is SF Symbols 4 (macOS 13) and this app targets 12, where the lookup returns
/// nil and the icon would simply not draw — so the plain `terminal` stands in there. Asked once
/// and cached: the sidebar repaints on hover and on every keystroke that edits a title.
fn tab_symbol() -> &'static str {
    thread_local! {
        static NAME: &'static str = if view::symbol_available("apple.terminal") {
            "apple.terminal"
        } else {
            "terminal"
        };
    }
    NAME.with(|n| *n)
}

/// Hue of a default-colored session icon while a job is running. Idle and ended take the row's own
/// text tiers instead, so an ordinary session is not colored at all — the green means "working",
/// not "a terminal". A tab with an explicit color of its own keeps that instead, and only the
/// *form* then tracks the state.
///
/// The theme's own green, not a fixed system one: every scheme defines one, so "working" is said in
/// the colors the rest of the window is speaking. Same slot and same lift the picker's Green uses
/// ([`dot_colors`]), so a running session and a tab painted green are the one green.
fn dot_running() -> (f64, f64, f64) {
    let t = theme::current();
    t.on_card(t.vivid(2, 10))
}

// ---- Text hierarchy: derived from the active theme so it stays legible on light and dark themes.
// The primary color is the theme foreground; weaker tiers blend it toward the background.
fn text_primary() -> (f64, f64, f64) {
    theme::current().fg
}
fn text_secondary() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.30)
}
fn text_placeholder() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.55)
}
/// The focus accent — the theme's, not the app's (see [`theme::Theme::accent`]). Used for the ring
/// around a focused input and the selection behind its text, the two marks that say "this box has
/// the keyboard".
fn accent() -> (f64, f64, f64) {
    theme::current().accent()
}
fn text_weakest() -> (f64, f64, f64) {
    let t = theme::current();
    theme::mix(t.fg, t.bg, 0.66)
}
/// Neutral overlay for box fills/hover (an alpha wash of the theme foreground): white-ish on
/// dark themes, dark on light themes — unlike a fixed white wash that vanishes on a light background.
fn overlay(alpha: f64) -> Retained<NSColor> {
    let f = theme::current().fg;
    rgba(f.0, f.1, f.2, alpha)
}

/// The colors a tab can be given. Index 0 = Default (auto: the state's own hue); 1..=8 are explicit
/// colors shown regardless of what the session is doing. Kept in sync with the tab color menu and
/// the per-tab `dot` index persisted in the layout file.
///
/// They are the **theme's** colors, not a fixed set of system ones: a tab marked red on Gruvbox is
/// that scheme's red, so a colored tab belongs to the window it sits in rather than importing
/// another palette into it. Five of the eight are the ANSI 16's own hues, each taken from whichever
/// of its two slots actually carries the color ([`theme::Theme::vivid`]); gray is the palette's
/// bright black, the one neutral that is not already the uncolored default's tone; and the two the
/// ANSI set has no color for are mixed from their neighbours in the theme's own hues — orange
/// between red and yellow, pink between magenta and red. Each is then lifted for the card it is
/// drawn on ([`theme::Theme::on_card`]), or a scheme that puts one at the panel's own luminance
/// would leave that tab looking uncolored.
///
/// The slots are **positional and permanent**: the index is what `layout.conf` stores, so renaming
/// or reordering one silently recolors tabs a user has already marked.
pub fn dot_colors() -> [(&'static str, (f64, f64, f64)); 9] {
    let t = theme::current();
    let (red, yellow, purple) = (t.vivid(1, 9), t.vivid(3, 11), t.vivid(5, 13));
    [
        ("Default", (0.0, 0.0, 0.0)), // sentinel: never drawn directly (see draw_session_icon)
        ("Red", t.on_card(red)),
        ("Orange", t.on_card(theme::mix(red, yellow, 0.5))),
        ("Yellow", t.on_card(yellow)),
        ("Green", t.on_card(t.vivid(2, 10))),
        ("Blue", t.on_card(t.vivid(4, 12))),
        ("Purple", t.on_card(purple)),
        ("Pink", t.on_card(theme::mix(purple, red, 0.5))),
        ("Gray", t.on_card(t.color(8))),
    ]
}

/// Target hit by a single press (Copy, so it fits in a Cell).
#[derive(Clone, Copy, PartialEq)]
enum Press {
    None,
    Search,
    Actions, // the row holding the side-by-side "Terminal" and "Group" buttons
    NewTab,
    NewGroup,
    Group(usize),
    Tab(u64, usize),  // (tab id, index of its group)
    GroupMenu(usize), // "⋯" at the right of a group row; click to pop up the group menu
    TabMenu(u64),     // "⋯" at the right of a tab row; click to pop up the tab menu
    TabDot(u64),      // the session icon at the left of a tab row; click to pick its color
    TabsLabel,        // "Sessions" section header above the ungrouped tabs (non-interactive)
    StyleMenu,        // bottom style row; click to pop up the color scheme menu
}

/// What the pointer is over, as far as *drawing* is concerned.
///
/// AppKit delivers mouse motion per pixel, but the sidebar's appearance depends only on which row
/// the cursor is in — plus, on the dual-button row, which half of it. Comparing this between two
/// motion events is what keeps a mouse sweep from repainting the whole card (and re-running
/// `snapshot()`, which clones every title) once per pixel.
#[derive(Clone, Copy, PartialEq)]
struct HoverKey {
    row: Press,
    half: u8, // dual-button row only: 0 = "Terminal" (left), 1 = "Group" (right)
}

/// A single laid-out row.
#[derive(Clone)]
struct Row {
    top: f64,
    h: f64,
    indent: f64,
    label: String,
    kind: Press,
    selected: bool,
    collapsed: bool, // only meaningful for group rows: collapsed state
    group: usize,    // the group this row belongs to (usize::MAX for button rows)
    dot: u8,         // tab rows: status-dot color index (0 = default/auto)
    state: SessionState, // tab rows: what the session is doing (drives the dot's form)
    activity: bool,      // tab rows: unseen output (brightens the label)
    bell: bool,          // tab rows: unseen BEL (shows a bell glyph)
    /// Group rows: how many sessions the group is hiding. 0 when it is expanded — the rows are
    /// right there to be counted — so this doubles as "should a count be drawn".
    count: usize,
    locked: bool,    // tab rows: whether the tab is locked (protected from close)
}

pub struct SidebarIvars {
    controller: Cell<*const AppController>,
    font: Retained<NSFont>,
    font_small: Retained<NSFont>,   // small font used for the ⌘F / ⌘, shortcut hints
    font_section: Retained<NSFont>, // semibold label font of the "Sessions" / group section rows
    press: Cell<Press>,
    start_y: Cell<f64>,
    cur_x: Cell<f64>, // x of the most recent press (used to pop up menus at the mouse)
    cur_y: Cell<f64>,
    dragging: Cell<bool>,
    scroll: Cell<f64>, // vertical scroll amount of the list area (>=0, pixels the content shifts up)
    // Mouse hover: x/y within the view + whether inside the view (used for row highlight / dual-button split / hover "⋯").
    hover_x: Cell<f64>,
    hover_y: Cell<f64>,
    hovering: Cell<bool>,
    hover_key: Cell<HoverKey>, // what the last motion event resolved to; see HoverKey
    /// The vertical bands of the rows `render` last drew, in its own order: `(top, height, kind)`.
    /// Written once per redraw and read by the hover hit test, which runs per *motion event* — see
    /// [`SidebarView::row_at_cached`] for why the two cannot be the same code path.
    bands: RefCell<Vec<(f64, f64, Press)>>,
    tracking_added: Cell<bool>,
    // Search: query string + whether in search (focused) state.
    query: RefCell<String>,
    searching: Cell<bool>,
    // How far the search box is unrolled, 0..1. Its own value rather than a function of
    // `searching`, because the two disagree for exactly as long as the animation runs — which is
    // the point: `searching` is where the box is going, this is where it is.
    reveal: Cell<f64>,
    // Runs only while `reveal` is still travelling toward `searching`, and cancels itself on
    // arrival. The context it carries is this view as a raw pointer, so it must not outlive it —
    // the sidebar lives as long as the app, and the timer is far shorter-lived than that.
    anim: RefCell<Option<TimerToken>>,
    // Rename: object being edited (None = not editing) + edit buffer.
    editing: Cell<Option<Editing>>,
    edit_buf: RefCell<String>,
    // Text caret position (char index) for the active input (search query or rename buffer).
    caret: Cell<usize>,
    // Selection anchor (char index) of the active input: the selection spans anchor..caret.
    // None means no selection; the anchor is dropped as soon as the selection collapses.
    sel: Cell<Option<usize>>,
    // ⌘Z/⇧⌘Z history for the active input: (text, caret) snapshots taken before each mutation.
    undo: RefCell<Vec<(String, usize)>>,
    redo: RefCell<Vec<(String, usize)>>,
    // Kind of the previous mutation, so a run of typing (or of deleting) collapses into one undo step.
    last_edit: Cell<EditKind>,
    // Tab id the color-picker menu currently applies to (set when the menu opens).
    dot_target: Cell<u64>,
}

/// What the last text mutation was, used to coalesce undo steps: consecutive mutations of the same
/// kind share one snapshot, so ⌘Z undoes a whole typed word rather than one character.
#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    None,
    Insert,
    Delete,
}

/// The object being renamed in place.
#[derive(Clone, Copy, PartialEq)]
enum Editing {
    Tab(u64),
    Group(usize),
}

declare_class!(
    pub struct SidebarView;

    unsafe impl ClassType for SidebarView {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "SidebarView";
    }

    impl DeclaredClass for SidebarView {
        type Ivars = SidebarIvars;
    }

    unsafe impl NSObjectProtocol for SidebarView {}

    unsafe impl SidebarView {
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
            self.on_down(event);
        }

        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.on_drag(event);
        }

        #[method(mouseUp:)]
        fn mouse_up(&self, event: &NSEvent) {
            self.on_up(event);
        }

        #[method(scrollWheel:)]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.on_scroll(event);
        }

        // Right-click: on a group/tab row, pop up the corresponding "more" menu.
        #[method(rightMouseDown:)]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.on_right_down(event);
        }

        // Attach a mouse tracking area covering the visible region (InVisibleRect auto-adapts to size, so add it only once).
        #[method(updateTrackingAreas)]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            if self.ivars().tracking_added.get() {
                return;
            }
            let mtm = MainThreadMarker::new().expect("main thread");
            let opts = NSTrackingAreaOptions::NSTrackingMouseEnteredAndExited
                | NSTrackingAreaOptions::NSTrackingMouseMoved
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

        #[method(mouseMoved:)]
        fn mouse_moved(&self, event: &NSEvent) {
            self.set_hover(event);
        }

        #[method(mouseEntered:)]
        fn mouse_entered(&self, event: &NSEvent) {
            self.set_hover(event);
        }

        #[method(mouseExited:)]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().hovering.set(false);
            // Forget the cached row, or re-entering onto the same one would look unchanged and skip
            // the redraw that has to bring its hover wash back.
            self.ivars().hover_key.set(HoverKey { row: Press::None, half: 0 });
            unsafe { self.setNeedsDisplay(true) };
        }

        // Only take over the keyboard in search/rename state; otherwise leave focus to the terminal.
        #[method(acceptsFirstResponder)]
        fn accepts_first_responder(&self) -> bool {
            self.ivars().searching.get() || self.ivars().editing.get().is_some()
        }

        // Click elsewhere (e.g. the terminal) → abandon a rename, but *keep* the search box.
        //
        // The box closes only when the user says so — Esc, ⌘F on an empty box, the magnifier — so a
        // filtered list stays filtered while they work in the terminal. A rename is the opposite:
        // it is a modal edit of one row, and leaving it half-typed on screen with the keyboard
        // somewhere else would be a trap.
        #[method(resignFirstResponder)]
        fn resign_first_responder(&self) -> bool {
            self.ivars().editing.set(None);
            self.ivars().edit_buf.borrow_mut().clear();
            self.reset_input();
            unsafe { self.setNeedsDisplay(true) };
            true
        }

        #[method(keyDown:)]
        fn key_down(&self, event: &NSEvent) {
            self.on_key(event);
        }

        // ---- Group/tab "⋯" menu-item handlers (tag holds the group index or tab id) ----
        #[method(groupNewTab:)]
        fn group_new_tab(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            if let Some(ctrl) = self.controller() {
                ctrl.add_tab_in_group(gi);
            }
        }

        #[method(groupRename:)]
        fn group_rename(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            self.start_edit(Editing::Group(gi));
        }

        #[method(groupToggle:)]
        fn group_toggle(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            if let Some(ctrl) = self.controller() {
                ctrl.toggle_group_collapsed(gi);
            }
        }

        #[method(groupDelete:)]
        fn group_delete(&self, item: &NSMenuItem) {
            let gi = unsafe { item.tag() } as usize;
            if let Some(ctrl) = self.controller() {
                ctrl.delete_group(gi);
            }
        }

        #[method(tabRename:)]
        fn tab_rename(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            self.start_edit(Editing::Tab(id));
        }

        #[method(tabRevealInFinder:)]
        fn tab_reveal_in_finder(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.reveal_in_finder_id(id);
            }
        }

        #[method(tabToggleLock:)]
        fn tab_toggle_lock(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.toggle_tab_lock(id);
            }
        }

        #[method(tabClose:)]
        fn tab_close(&self, item: &NSMenuItem) {
            let id = unsafe { item.tag() } as u64;
            if let Some(ctrl) = self.controller() {
                ctrl.close_tab_user(id);
            }
        }

        // Color picked from the status-dot menu: tag = DOT_COLORS index, applied to the pending tab.
        #[method(pickDotColor:)]
        fn pick_dot_color(&self, item: &NSMenuItem) {
            let idx = unsafe { item.tag() } as u8;
            let id = self.ivars().dot_target.get();
            if let Some(ctrl) = self.controller() {
                ctrl.set_tab_dot(id, idx);
            }
        }
    }
);

impl SidebarView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        // 13pt is the macOS sidebar item size; section labels are 11pt semibold beside it.
        let font = unsafe { NSFont::systemFontOfSize(13.0) };
        let font_small = unsafe { NSFont::systemFontOfSize(10.5) };
        let font_section = unsafe { NSFont::systemFontOfSize_weight(11.0, NSFontWeightSemibold) };
        let this = mtm.alloc();
        let this = this.set_ivars(SidebarIvars {
            controller: Cell::new(std::ptr::null()),
            font,
            font_small,
            font_section,
            press: Cell::new(Press::None),
            start_y: Cell::new(0.0),
            cur_x: Cell::new(0.0),
            cur_y: Cell::new(0.0),
            dragging: Cell::new(false),
            scroll: Cell::new(0.0),
            hover_x: Cell::new(-1.0),
            hover_y: Cell::new(-1.0),
            hovering: Cell::new(false),
            hover_key: Cell::new(HoverKey { row: Press::None, half: 0 }),
            tracking_added: Cell::new(false),
            query: RefCell::new(String::new()),
            searching: Cell::new(false),
            reveal: Cell::new(0.0),
            anim: RefCell::new(None),
            editing: Cell::new(None),
            edit_buf: RefCell::new(String::new()),
            caret: Cell::new(0),
            sel: Cell::new(None),
            undo: RefCell::new(Vec::new()),
            redo: RefCell::new(Vec::new()),
            last_edit: Cell::new(EditKind::None),
            dot_target: Cell::new(0),
            bands: RefCell::new(Vec::new()),
        });
        unsafe { msg_send_id![super(this), initWithFrame: frame] }
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

    /// y coordinate within the view (already a flipped coordinate system, origin at top-left).
    /// Record the hover point (x/y) and request a redraw — but only when the pointer actually
    /// moved onto something else, since nothing about the drawing changes within a row (see
    /// [`HoverKey`]). The point itself is always stored: `draw_actions` and `open_context_menu`
    /// read the raw x/y, and the cache has to keep following the pointer across skipped frames.
    fn set_hover(&self, event: &NSEvent) {
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        self.ivars().hover_x.set(p.x);
        self.ivars().hover_y.set(p.y);
        let entered = !self.ivars().hovering.replace(true); // the overlay scrollbar appears on entry
        let key = self.hover_key_at(p.x, p.y);
        let moved = self.ivars().hover_key.replace(key) != key;
        if entered || moved {
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    /// Resolve a point to its [`HoverKey`]. Deliberately built on `row_at`, the same hit test a
    /// press uses: it rejects list rows scrolled out of the visible band, and those are exactly the
    /// rows `render` clips away — so a hover that lands on one changes nothing on screen.
    fn hover_key_at(&self, x: f64, y: f64) -> HoverKey {
        let w = self.bounds().size.width;
        let row = self.row_at_cached(y);
        HoverKey { row, half: u8::from(row == Press::Actions && x >= Self::actions_split_x(w)) }
    }

    /// The hit test a *motion* event runs: the same rule as [`Self::row_at`], against the bands the
    /// last redraw recorded rather than a freshly laid-out list.
    ///
    /// It has to be the cheap one. AppKit delivers `mouseMoved:` per pixel of travel, and a press
    /// happens once, so what `row_at` does per call — `snapshot()`, which clones every tab's title
    /// and cwd, then `build_rows`, which clones every one of those again into a `Row` — is fine
    /// there and is a sweep's worth of allocation here. It ran *before* the `HoverKey` comparison
    /// that exists to skip the redraw, so the cache saved the painting and paid for the layout
    /// anyway: a fast drag across a window full of sessions cost more than the unconditional
    /// repaint the key was added to avoid.
    ///
    /// Reading a snapshot of the last frame is sound because that frame is what the pointer is over:
    /// the bands are only ever stale between a model change and the redraw it already requested, and
    /// that redraw resolves the hover from the raw pointer position, not from this key. A press
    /// keeps using `row_at` — that one must act on the list as it is now, not as it was drawn.
    fn row_at_cached(&self, y: f64) -> Press {
        let bands = self.ivars().bands.borrow();
        if bands.is_empty() {
            // Nothing drawn yet (a motion event can precede the first redraw). Lay it out the slow
            // way rather than answering None, which would mark the row under the pointer unhovered.
            drop(bands);
            let Some(ctrl) = self.controller() else { return Press::None };
            let query = self.ivars().query.borrow().clone();
            return self.row_at(&ctrl.snapshot(), y, &query);
        }
        self.hit_bands(y, bands.iter().copied())
    }

    /// Resolve a y against laid-out rows: the first band containing it wins, and a *list* row is
    /// skipped when the point is outside the band the list is clipped to — those rows are scrolled
    /// out of sight, and `render` does not draw them either.
    fn hit_bands(&self, y: f64, bands: impl Iterator<Item = (f64, f64, Press)>) -> Press {
        let footer_top = self.bounds().size.height - Self::footer_height();
        let list_top = self.list_top();
        for (top, h, kind) in bands {
            let list_row = matches!(kind, Press::Group(_) | Press::Tab(..) | Press::TabsLabel);
            if list_row && (y < list_top || y >= footer_top) {
                continue;
            }
            if y >= top && y < top + h {
                return kind;
            }
        }
        Press::None
    }

    fn point_y(&self, event: &NSEvent) -> f64 {
        let p = unsafe { event.locationInWindow() };
        self.convertPoint_fromView(p, None).y
    }

    /// The x that separates the dual-button row's two sides. Shared by the press split in
    /// `on_down`, the hover split in `draw_actions` and the hover key, so the three cannot drift.
    /// Note it splits the whole row width, not just the buttons: a press in either margin still
    /// lands on the button beside it, which is what makes the pair feel like one control.
    fn actions_split_x(w: f64) -> f64 {
        Self::actions_group_x(w) - ACTIONS_GAP / 2.0
    }

    /// Left edge of the square "Group" button, which sits at the row's trailing edge.
    fn actions_group_x(w: f64) -> f64 {
        w - HPAD - ACTIONS_ICON_W
    }

    /// Top y of the group/tab list area (below the search box, when there is one, + the two
    /// buttons). Above this is the fixed area, which does not scroll.
    fn list_top(&self) -> f64 {
        TOP_INSET + self.search_h() + BTN_H + GAP
    }

    /// What the search box currently costs the layout: nothing at rest, its full row plus the gap
    /// once open, and everything in between while it animates. Every measurement that used to
    /// assume a permanent row has to ask this instead — off by one row and the list draws into the
    /// clip of the row above it.
    fn search_h(&self) -> f64 {
        (SEARCH_H + GAP) * self.reveal()
    }

    /// The unroll fraction, eased. Smoothstep rather than the raw linear ramp: the box and the
    /// whole list below it move together, and a linear start/stop on that much travel reads as a
    /// jerk at both ends.
    fn reveal(&self) -> f64 {
        let p = self.ivars().reveal.get().clamp(0.0, 1.0);
        p * p * (3.0 - 2.0 * p)
    }

    /// Whether a session matches the search query: by name, or by the directory it is in.
    ///
    /// The directory is worth searching precisely because it is *not* drawn — most sessions are
    /// distinguished by where they are rather than by a name anyone bothered to give them, and
    /// "tabt" should find the one sitting in ~/Code/tabt whatever it happens to be called.
    ///
    /// `q` is already lowercased by the caller, and the empty case short-circuits before either
    /// `to_lowercase` runs, so an unfiltered list allocates nothing here.
    fn matches(t: &TabSnap, q: &str) -> bool {
        q.is_empty() || t.title.to_lowercase().contains(q) || t.cwd.to_lowercase().contains(q)
    }

    /// Build all rows. `scroll` only affects groups/tabs (the list area); search/buttons stay fixed.
    /// A list row's `top` is returned directly as a screen coordinate (scroll already subtracted), so hit testing and dragging need no further conversion.
    fn build_rows(&self, snap: &Snapshot, query: &str, scroll: f64) -> Vec<Row> {
        let q = query.to_lowercase();
        let mut rows = Vec::new();
        let mut y = TOP_INSET;
        // Search box (label holds the current query; drawn specially in render). Only while the
        // search is on — or on its way in or out: at rest the sessions start right under the top
        // strip, and the box is reached through the strip's magnifier or ⌘F.
        //
        // Its `h` is the *visible* height, which is what the drawing clips to and what hit testing
        // uses, so a click during the animation lands on whatever is actually under the pointer.
        let slot = self.search_h();
        if slot > 0.0 {
            rows.push(Row { top: y, h: SEARCH_H * self.reveal(), indent: PAD, label: query.to_string(), kind: Press::Search, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false, state: SessionState::Idle, activity: false, bell: false, count: 0 });
            y += slot;
        }

        // Side-by-side "Terminal" and "Group" buttons, occupying one row.
        rows.push(Row { top: y, h: BTN_H, indent: PAD, label: String::new(), kind: Press::Actions, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false, state: SessionState::Idle, activity: false, bell: false, count: 0 });
        y += BTN_H + GAP;

        // Ungrouped tabs, rendered at the top with a shallow indent.
        let matched_ung: Vec<&TabSnap> = snap.ungrouped.iter().filter(|t| Self::matches(t, &q)).collect();
        // The "Sessions" section label is always shown (even with no ungrouped tabs), so the session
        // list stays anchored and remains a visible drop target. During a search it's hidden only when
        // no session matches, matching how empty groups drop out of the filtered list.
        if q.is_empty() || !matched_ung.is_empty() {
            // "Sessions" section label above the tabs (matches the GROUP labels below).
            rows.push(Row { top: y - scroll, h: SECTION_H, indent: PAD, label: "Sessions".to_string(), kind: Press::TabsLabel, selected: false, collapsed: false, group: usize::MAX, dot: 0, locked: false, state: SessionState::Idle, activity: false, bell: false, count: 0 });
            y += SECTION_H;
            for t in matched_ung {
                let selected = snap.active == Some(t.id);
                rows.push(Row { top: y - scroll, h: ROW_H, indent: 16.0, label: t.title.clone(), kind: Press::Tab(t.id, UNGROUPED), selected, collapsed: false, group: UNGROUPED, dot: t.dot, locked: t.locked, state: t.state, activity: t.activity, bell: t.bell, count: 0 });
                y += ROW_H;
            }
        }

        for (gi, g) in snap.groups.iter().enumerate() {
            // Filter: when the query is non-empty, keep only tabs whose title matches, and hide groups with no match.
            let matched: Vec<&TabSnap> = g.tabs.iter().filter(|t| Self::matches(t, &q)).collect();
            if !q.is_empty() && matched.is_empty() {
                continue;
            }
            // A count only while the group is actually hiding something. During a search its
            // matches are listed underneath it regardless of collapsed state, so a count there
            // would contradict what is on screen.
            let hidden = if g.collapsed && q.is_empty() { g.tabs.len() } else { 0 };
            rows.push(Row { top: y - scroll, h: ROW_H, indent: PAD, label: g.name.clone(), kind: Press::Group(gi), selected: false, collapsed: g.collapsed, group: gi, dot: 0, locked: false, state: SessionState::Idle, activity: false, bell: false, count: hidden });
            y += ROW_H;
            // Hide tabs when collapsed and not in search state; while searching, always show matches (to make collapsed tabs findable).
            if g.collapsed && q.is_empty() {
                continue;
            }
            for t in matched {
                let selected = snap.active == Some(t.id);
                rows.push(Row { top: y - scroll, h: ROW_H, indent: 26.0, label: t.title.clone(), kind: Press::Tab(t.id, gi), selected, collapsed: false, group: gi, dot: t.dot, locked: t.locked, state: t.state, activity: t.activity, bell: t.bell, count: 0 });
                y += ROW_H;
            }
        }
        rows
    }

    /// Height of the bottom settings area (settings row + symmetric top/bottom margins).
    fn footer_height() -> f64 {
        FROW_H + 2.0 * FPAD
    }

    /// Bottom settings row: click to pop up the settings menu (color scheme / font / font size). Top/bottom margins are symmetric.
    fn footer_rows(_snap: &Snapshot, height: f64) -> Vec<Row> {
        let y = (height - Self::footer_height()).max(0.0) + FPAD;
        vec![Row {
            top: y,
            h: FROW_H,
            indent: PAD,
            label: "Settings".to_string(),
            kind: Press::StyleMenu,
            selected: false,
            collapsed: false,
            group: usize::MAX,
            dot: 0,
            locked: false,
            state: SessionState::Idle,
            activity: false,
            bell: false,
            count: 0,
        }]
    }

    /// Merged rows of the top list + bottom style area (used for drawing and hit testing).
    fn all_rows(&self, snap: &Snapshot, height: f64, query: &str, scroll: f64) -> Vec<Row> {
        let mut rows = self.build_rows(snap, query, scroll);
        rows.extend(Self::footer_rows(snap, height));
        rows
    }

    /// Current scroll amount (reads the ivar).
    fn scroll(&self) -> f64 {
        self.ivars().scroll.get()
    }

    /// Maximum scroll amount when list content exceeds the visible height (0 if content is short).
    fn max_scroll(&self, snap: &Snapshot, query: &str, height: f64) -> f64 {
        let rows = self.build_rows(snap, query, 0.0);
        self.max_scroll_of(&rows, height)
    }

    /// Same computation as `max_scroll`, but from an already-built (unscrolled) row list —
    /// lets `render()` measure and position rows from a single `build_rows` call instead of two.
    fn max_scroll_of(&self, rows: &[Row], height: f64) -> f64 {
        let content_bottom = rows
            .iter()
            .filter(|r| matches!(r.kind, Press::Group(_) | Press::Tab(..)))
            .map(|r| r.top + r.h)
            .fold(self.list_top(), f64::max);
        let footer_top = height - Self::footer_height();
        (content_bottom - footer_top).max(0.0)
    }

    fn render(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        let query = self.ivars().query.borrow().clone();
        let editing = self.ivars().editing.get();
        let (w, h) = (self.bounds().size.width, self.bounds().size.height);
        let footer_top = h - Self::footer_height();
        let list_top = self.list_top();

        // Build the scrollable rows once (unscrolled), derive max_scroll from that same build,
        // then shift positions for the actual scroll offset — avoids cloning every tab/group
        // title a second time via a second build_rows call.
        let mut rows = self.build_rows(&snap, &query, 0.0);
        let max_scroll = self.max_scroll_of(&rows, h);
        let scroll = self.scroll().clamp(0.0, max_scroll);
        self.ivars().scroll.set(scroll);
        if scroll > 0.0 {
            for r in &mut rows {
                if matches!(r.kind, Press::Group(_) | Press::Tab(..) | Press::TabsLabel) {
                    r.top -= scroll;
                }
            }
        }
        rows.extend(Self::footer_rows(&snap, h));

        // The list is laid out; hand the hover hit test its geometry. Recorded here, at the one
        // place that has it already, because the alternative is laying it out again per motion
        // event — see `row_at_cached`. Positions only: nothing that would keep a title alive.
        *self.ivars().bands.borrow_mut() = rows.iter().map(|r| (r.top, r.h, r.kind)).collect();

        // No background fill: this view is mounted inside the card (see `card.rs`), whose layer
        // paints the fill, border and shadow — and no separator above the settings row either, the
        // card's own rounded edge having replaced the line that used to mark that seam.

        // Hover hit (don't show hover highlight while dragging, to avoid overlapping the drop line).
        let hovering = self.ivars().hovering.get() && !self.ivars().dragging.get();
        let hy = self.ivars().hover_y.get();
        let hovered = |row: &Row| hovering && hy >= row.top && hy < row.top + row.h;

        // The fixed area (search box + two buttons + bottom style row) doesn't scroll; draw directly.
        for row in &rows {
            if !matches!(row.kind, Press::Group(_) | Press::Tab(..) | Press::TabsLabel) {
                self.draw_row(row, w, &query, editing, hovered(row));
            }
        }

        // The list area (groups/tabs) is clipped to [list_top, footer_top) and drawn scrolled.
        let list_rect = rect(0.0, list_top, w, (footer_top - list_top).max(0.0));
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(list_rect) };
        for row in &rows {
            if matches!(row.kind, Press::Group(_) | Press::Tab(..) | Press::TabsLabel) {
                self.draw_row(row, w, &query, editing, hovered(row));
            }
        }
        // Drop feedback, clipped within the list area so it won't spill into the fixed one. Two
        // shapes, and only ever one of them: a *container* highlight when the drop would put the
        // tab inside a collapsed group, and otherwise the insertion line between two rows. They
        // answer different questions — "into what" versus "between which two" — and a collapsed
        // group has no visible gap for a line to mean anything in.
        if self.ivars().dragging.get() {
            if let Some(top) = self.drop_into_group(&snap) {
                // Fill only, no outline: the row is already bounded by the rows above and below it,
                // so a ring around it just draws a second edge inside those — and the sidebar's
                // other "this row is the one" marks (selection, hover) are all plain washes too.
                let a = accent();
                round_fill(rect(HPAD, top + 1.0, w - 2.0 * HPAD, ROW_H - 2.0), 7.0, &rgba(a.0, a.1, a.2, 0.22));
            } else if let Some(y) = self.drop_indicator_y(&snap) {
                // A 2pt bar, the weight the system's own drop indicators use: any heavier and it
                // stops reading as a line and starts reading as a row of its own. Snapped to whole
                // points, since the y it is given is a row top minus the scroll offset and a
                // trackpad leaves that fractional — off the grid, a bar this thin antialiases
                // across three device pixels and looks both thicker and blurrier than it is.
                round_fill(rect(8.0, y.round() - 1.0, w - 16.0, 2.0), 1.0, &overlay(0.7));
            }
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }

        // Right-edge scrollbar indicator: hidden by default, shown only while the mouse hovers the sidebar (like the system overlay style).
        if max_scroll > 0.0 && self.ivars().hovering.get() {
            let track_h = footer_top - list_top;
            let content_h = track_h + max_scroll;
            let thumb_h = (track_h * track_h / content_h).max(28.0);
            let thumb_y = list_top + (scroll / max_scroll) * (track_h - thumb_h);
            round_fill(rect(w - 7.0, thumb_y, 3.0, thumb_h), 1.5, &rgba(0.45, 0.45, 0.52, 0.9));
        }
    }

    /// Draw a single row (search box / rename box / button / group / tab / style row).
    fn draw_row(&self, row: &Row, w: f64, query: &str, editing: Option<Editing>, hovered: bool) {
        if let Press::Search = row.kind {
            self.draw_search(row, w, query);
            return;
        }
        // Tab/group being renamed: draw the in-place edit box instead of the normal title.
        let editing_this = match row.kind {
            Press::Tab(id, _) => editing == Some(Editing::Tab(id)),
            Press::Group(gi) => editing == Some(Editing::Group(gi)),
            _ => false,
        };
        if editing_this {
            self.draw_edit(row, w);
            return;
        }

        // Side-by-side "Terminal" and "Group" buttons.
        if let Press::Actions = row.kind {
            self.draw_actions(row, w);
            return;
        }

        let inset = rect(HPAD, row.top + 1.0, w - 2.0 * HPAD, row.h - 2.0);
        let vmid = |ih: f64| row.top + (row.h - ih) / 2.0; // vertically center icon/text

        // ---- "Sessions" session-list label (mirrors the group labels, no folder icon) ----
        // Sentence case, not uppercase micro-caps: macOS 13+ sidebar sections read "Favorites",
        // not "FAVORITES".
        if let Press::TabsLabel = row.kind {
            draw_truncated(&row.label, rect(row.indent, vmid(13.0), (w - 2.0 * PAD).max(0.0), 15.0), &self.ivars().font_section, text_placeholder());
            return;
        }

        // ---- Group title: section label + system folder icon ----
        if let Press::Group(_) = row.kind {
            if hovered {
                round_fill(inset, 7.0, &overlay(ROW_HOVER));
            }
            let folder = if row.collapsed { "folder.fill" } else { "folder" };
            draw_symbol(folder, rect(row.indent, vmid(12.0), 14.0, 12.0), text_placeholder());
            let label_x = row.indent + 20.0;
            draw_truncated(&row.label, rect(label_x, vmid(13.0), (w - 34.0 - label_x).max(0.0), 15.0), &self.ivars().font_section, text_placeholder());
            // The "⋯" takes the slot on hover, the same way a session row's lock glyph yields to
            // it: the count is a standing fact and can wait until the pointer leaves.
            if hovered {
                draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
            } else if row.count > 0 {
                self.draw_badge(&row.count.to_string(), w - HPAD - 8.0, row.top + row.h / 2.0);
            }
            return;
        }

        // ---- Bottom settings row: gear + Settings + ⌘, badge (8px inset inside the container) ----
        if let Press::StyleMenu = row.kind {
            if hovered {
                round_fill(inset, 7.0, &overlay(ROW_HOVER));
            }
            let ip = 8.0; // inset relative to the hover container (HPAD..w-HPAD)
            let col = text_secondary(); // theme-aligned foreground
            draw_symbol("gearshape", rect(HPAD + ip, vmid(15.0), 15.0, 15.0), col);
            let attrs = make_attrs(&self.ivars().font, Some(&ns_color(col)));
            let ns = NSString::from_str("Settings");
            unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(HPAD + ip + 22.0, vmid(16.0)), Some(&attrs)) };
            self.draw_badge("⌘,", w - HPAD - ip, row.top + row.h / 2.0);
            return;
        }

        // ---- Session row (Tab) ----
        // Selection highlight is a theme-aligned neutral wash (adapts to light/dark), not a fixed accent,
        // and it is the same chip the action buttons draw. Kept deliberately faint: the row is already
        // marked by its brighter label, its dot and the "⋯", so the wash only has to place them — a
        // heavier one reads as a block of color in a sidebar that is otherwise all background.
        if row.selected {
            round_fill(inset, 7.0, &overlay(if hovered { CHIP_HOVER } else { CHIP_BG }));
        } else if hovered {
            round_fill(inset, 7.0, &overlay(ROW_HOVER));
        }
        // The session icon carries the state; the *label's* tier still depends on selection and
        // nothing else: a row's brightness is a fact about the list, not about the session, and
        // having unseen output raise it left every row that had printed anything sitting at full
        // weight until it was visited — the contrast that says "this is the one you are in" gone
        // until then. The unseen mark is a dot on the right instead (below), which says the same
        // thing without touching the text.
        let fg = if row.selected { text_primary() } else { text_secondary() };
        Self::draw_session_icon(row, fg, vmid(ICON_H));
        // Name truncates with an ellipsis; leaves room for the right-side meta / "⋯".
        let name_x = row.indent + ICON_W + 8.0;
        draw_truncated(&row.label, rect(name_x, vmid(16.0), (w - 40.0 - name_x).max(0.0), 18.0), &self.ivars().font, fg);
        // Right side, in priority order: "⋯" while hovered (so the menu — including Unlock — is
        // reachable on any tab); then a bell, which is an event and outranks the standing facts;
        // then a lock glyph for locked tabs, "⋯" for the selected tab, and nothing at rest.
        if hovered {
            draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
        } else if row.bell {
            draw_symbol("bell.fill", rect(w - 26.0, vmid(12.0), 12.0, 12.0), fg);
        } else if row.activity {
            // Unseen output: the unread dot every mail client has, and gone the moment the tab is
            // selected. Below the bell, which is an event rather than a standing fact.
            round_fill(rect(w - 23.0, vmid(6.0), 6.0, 6.0), 3.0, &ns_color(text_placeholder()));
        } else if row.locked {
            draw_symbol("lock.fill", rect(w - 26.0, vmid(12.0), 11.0, 12.0), text_placeholder());
        } else if row.selected {
            draw_symbol("ellipsis", rect(w - 28.0, vmid(11.0), 16.0, 11.0), text_placeholder());
        }
    }

    /// The session icon: one glyph carrying what the session is doing *and* the color the user
    /// picked for the tab.
    ///
    /// It replaced a separate status dot that sat to the left of it — two marks in a row, one of
    /// them a bare disc, saying two halves of the same thing.
    ///
    /// **The glyph itself never changes; only its color does.** It used to swap in the filled
    /// variant while a job ran, so the icon carried the state as *form* and the user's tab color as
    /// *hue* at once. That is one axis too many for a 14pt glyph: the fill reads as a weight change
    /// at that size, so a row appeared to bold itself whenever a command ran, competing with
    /// selection — which is the only thing in this list allowed to change a row's weight.
    ///
    /// So hue carries both, with the user's choice on top: an explicit tab color tints the icon in
    /// the theme's own shade of what was chosen (see [`dot_colors`]) whatever the session is doing,
    /// faded toward the placeholder tier once that session is dead so the color survives and "dead"
    /// still reads. Only a tab left on the default color takes its hue from the state — green while
    /// a job runs, and otherwise the label's own tone, so an ordinary idle row still brightens with
    /// selection along with its text.
    ///
    /// Note what is absent: selection, for the state's part of it. It used to make the dot green,
    /// which is why green has always meant "the tab you are looking at" rather than "running".
    fn draw_session_icon(row: &Row, fg: theme::Rgb, y: f64) {
        // Index defensively: the color index comes from the on-disk layout file and may be anything.
        let explicit = dot_colors().get(row.dot as usize).filter(|_| row.dot != 0).map(|(_, c)| *c);
        let hue = match (explicit, row.state) {
            (Some(c), SessionState::Ended) => theme::mix(c, text_placeholder(), 0.45),
            (Some(c), _) => c,
            (None, SessionState::Running) => dot_running(),
            (None, SessionState::Ended) => text_placeholder(),
            // A tab whose shell has not been started yet is drawn exactly as an idle one, and that
            // is the point rather than an omission: it is a session the user has simply not opened,
            // and marking it would advertise an implementation detail as a state worth reading.
            (None, SessionState::Idle | SessionState::Dormant) => fg,
        };
        draw_symbol(tab_symbol(), rect(row.indent, y, ICON_W, ICON_H), hue);
    }

    /// The row of side-by-side "Terminal" and "Group" buttons.
    ///
    /// "Group" is the icon alone, in a square button at the trailing edge: `plus.rectangle.on.folder`
    /// draws the whole sentence its label used to spell out, and the width that frees goes to
    /// "Terminal", the one of the two that is pressed all day.
    fn draw_actions(&self, row: &Row, w: f64) {
        let hovering = self.ivars().hovering.get() && !self.ivars().dragging.get();
        let hx = self.ivars().hover_x.get();
        let hy = self.ivars().hover_y.get();
        let in_row = hovering && hy >= row.top && hy < row.top + row.h;
        let gx = Self::actions_group_x(w);
        let left = rect(HPAD, row.top, gx - ACTIONS_GAP - HPAD, row.h);
        let right = rect(gx, row.top, ACTIONS_ICON_W, row.h);
        let hover_left = in_row && hx < Self::actions_split_x(w);
        let hover_right = in_row && !hover_left;

        // Both buttons share the same neutral low-opacity wash as a selected session row (no accent
        // highlight) and carry no outline: the fill alone already lifts them off the card, and a
        // hairline around them was the only stroke left in the list, so it read as a stray box.
        let lbg = if hover_left { CHIP_HOVER } else { CHIP_BG };
        round_fill(left, 7.0, &overlay(lbg));
        self.draw_btn_content(left, "plus", "Terminal", text_secondary());

        let rbg = if hover_right { CHIP_HOVER } else { CHIP_BG };
        round_fill(right, 7.0, &overlay(rbg));
        Self::draw_btn_icon(right, "plus.rectangle.on.folder", text_secondary());
    }

    /// Button content: icon + text, horizontally centered.
    fn draw_btn_content(&self, r: NSRect, icon: &str, label: &str, col: (f64, f64, f64)) {
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(col)));
        let ns = NSString::from_str(label);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        let (icon_w, ig) = (13.0, 6.0);
        let start = r.origin.x + (r.size.width - (icon_w + ig + tw)) / 2.0;
        let cy = r.origin.y + r.size.height / 2.0;
        draw_symbol(icon, rect(start, cy - 6.5, icon_w, 13.0), col);
        unsafe {
            ns.drawAtPoint_withAttributes(NSPoint::new(start + icon_w + ig, cy - 8.0), Some(&attrs));
        }
    }

    /// Icon-only button content: the symbol centered in the button, at the size a labeled one gives
    /// its icon plus a shade — with no text beside it to set the scale, the glyph is the button.
    /// `draw_symbol` centers at the symbol's natural size, so a wide glyph in a square box stays
    /// centered rather than being squeezed into it.
    fn draw_btn_icon(r: NSRect, icon: &str, col: (f64, f64, f64)) {
        const S: f64 = 15.0;
        let cx = r.origin.x + r.size.width / 2.0;
        let cy = r.origin.y + r.size.height / 2.0;
        draw_symbol(icon, rect(cx - S / 2.0, cy - S / 2.0, S, S), col);
    }

    /// Shortcut hint: small monospace text right-aligned to `right_x`, vertically centered at `cy` (no background).
    fn draw_badge(&self, text: &str, right_x: f64, cy: f64) {
        let attrs = make_attrs(&self.ivars().font_small, Some(&ns_color(text_weakest())));
        let ns = NSString::from_str(text);
        let tw = unsafe { ns.sizeWithAttributes(Some(&attrs)).width };
        unsafe {
            ns.drawAtPoint_withAttributes(NSPoint::new(right_x - tw, cy - 13.0 / 2.0), Some(&attrs));
        }
    }

    /// Which "region" a row belongs to: None = ungrouped region, Some(gi) = a group; non-list rows return the outer None.
    /// The "Sessions" label counts as part of the ungrouped region (just as a group title counts as part of its
    /// group), so an empty session list is still a hit-testable drop target with a place to anchor the drop line.
    fn row_region(row: &Row) -> Option<Option<usize>> {
        match row.kind {
            Press::Group(gi) => Some(Some(gi)),
            Press::TabsLabel => Some(None),
            Press::Tab(_, _) => Some(if row.group == UNGROUPED { None } else { Some(row.group) }),
            _ => None,
        }
    }

    /// Tab drag drop target: returns (target region Some(group)/None(ungrouped), which tab to insert before / None = end).
    /// Excludes the dragged tab itself during computation, to support reordering within the same region.
    fn tab_drop_target(&self, snap: &Snapshot, dragged: u64) -> Option<(Option<usize>, Option<u64>)> {
        let query = self.ivars().query.borrow().clone();
        let rows = self.build_rows(snap, &query, self.scroll());
        let y = self.ivars().cur_y.get();
        // The region of the row that's hit is the target region.
        let mut region = None;
        for row in &rows {
            if y >= row.top && y < row.top + row.h {
                if let Some(r) = Self::row_region(row) {
                    region = Some(r);
                    break;
                }
            }
        }
        let region = region.unwrap_or_else(|| {
            // No row hit: above the first group title → ungrouped region; otherwise the last group (ungrouped region if there are no groups).
            let first_group_top = rows.iter().find_map(|r| match r.kind {
                Press::Group(_) => Some(r.top),
                _ => None,
            });
            match first_group_top {
                Some(gt) if y < gt => None,
                _ => snap.groups.len().checked_sub(1).map(Some).unwrap_or(None),
            }
        });
        // Insert before the first tab in the target region whose vertical midpoint is below the cursor; append if all are above.
        let before = rows
            .iter()
            .filter(|r| Self::row_region(r) == Some(region) && matches!(r.kind, Press::Tab(..)))
            .filter_map(|r| match r.kind {
                Press::Tab(tid, _) if tid != dragged => Some((tid, r.top + r.h / 2.0)),
                _ => None,
            })
            .find(|(_, mid)| y < *mid)
            .map(|(tid, _)| tid);
        Some((region, before))
    }

    /// The row top of the collapsed group the drag is currently over, if dropping there would put
    /// the tab *into* that group rather than between two rows.
    ///
    /// Only for a tab drag, and only with the search box empty: a query lists a collapsed group's
    /// matches underneath it regardless of its state, so there the rows are on screen and the
    /// insertion line is the honest answer again.
    ///
    /// The drop itself needed no change — a group row has always resolved to that group's region,
    /// and with none of its tabs on screen the insertion lands at the end of it. What was missing
    /// was saying so: the line drew under the group title, which reads as "after this group".
    fn drop_into_group(&self, snap: &Snapshot) -> Option<f64> {
        if !matches!(self.ivars().press.get(), Press::Tab(..)) {
            return None;
        }
        let query = self.ivars().query.borrow().clone();
        if !query.is_empty() {
            return None;
        }
        let y = self.ivars().cur_y.get();
        self.build_rows(snap, &query, self.scroll())
            .into_iter()
            .find(|r| matches!(r.kind, Press::Group(_)) && r.collapsed && y >= r.top && y < r.top + r.h)
            .map(|r| r.top)
    }

    /// The y (insertion position) the drag placeholder line should snap to; None means don't draw it.
    /// Kept consistent with `on_up`'s drop decision, so the preview line faithfully reflects the final drop position.
    fn drop_indicator_y(&self, snap: &Snapshot) -> Option<f64> {
        let query = self.ivars().query.borrow().clone();
        let rows = self.build_rows(snap, &query, self.scroll());
        let list_bottom = rows.last().map(|r| r.top + r.h).unwrap_or(0.0);
        let y = self.ivars().cur_y.get();
        match self.ivars().press.get() {
            // Group: the line snaps to the top edge of the target-th group title; past the end, snaps to the list bottom.
            Press::Group(_) => {
                let heads: Vec<f64> = rows
                    .iter()
                    .filter_map(|r| match r.kind {
                        Press::Group(_) => Some(r.top),
                        _ => None,
                    })
                    .collect();
                let mut target = 0usize;
                for (i, top) in heads.iter().enumerate() {
                    let bottom = heads.get(i + 1).copied().unwrap_or(list_bottom);
                    if (top + bottom) / 2.0 < y {
                        target += 1;
                    }
                }
                Some(heads.get(target).copied().unwrap_or(list_bottom))
            }
            // Tab: the line snaps to the insertion position — the top edge of the `before` tab, or the bottom edge of the target region's last row.
            Press::Tab(dragged, _) => {
                let (region, before) = self.tab_drop_target(snap, dragged)?;
                match before {
                    Some(bid) => rows
                        .iter()
                        .find(|r| matches!(r.kind, Press::Tab(tid, _) if tid == bid))
                        .map(|r| r.top),
                    None => rows
                        .iter()
                        .filter(|r| Self::row_region(r) == Some(region))
                        .map(|r| r.top + r.h)
                        .fold(None, |acc: Option<f64>, b| Some(acc.map_or(b, |a: f64| a.max(b)))),
                }
            }
            _ => None,
        }
    }

    /// Scroll wheel/trackpad: scroll the list area up/down (clamped to 0..max_scroll).
    fn on_scroll(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let query = self.ivars().query.borrow().clone();
        let h = self.bounds().size.height;
        let max = self.max_scroll(&snap, &query, h);
        if max <= 0.0 {
            if self.scroll() != 0.0 {
                self.ivars().scroll.set(0.0);
                unsafe { self.setNeedsDisplay(true) };
            }
            return;
        }
        let dy = unsafe { event.scrollingDeltaY() };
        let next = (self.scroll() - dy).clamp(0.0, max);
        if next != self.scroll() {
            self.ivars().scroll.set(next);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    /// Hit test: which row the y within the view hits (list rows are only valid within the visible area, to avoid mis-hits on scrolled-out rows).
    fn row_at(&self, snap: &Snapshot, y: f64, query: &str) -> Press {
        let rows = self.all_rows(snap, self.bounds().size.height, query, self.scroll());
        self.hit_bands(y, rows.iter().map(|r| (r.top, r.h, r.kind)))
    }

    /// Pop up the "more" menu of the group/tab row at `(x, y)`, anchored at that point.
    /// Returns false when the point hits no group/tab row (nothing is shown).
    fn open_menu_at(&self, snap: &Snapshot, x: f64, y: f64) -> bool {
        let query = self.ivars().query.borrow().clone();
        self.ivars().cur_x.set(x); // so the popup positions at this point
        self.ivars().cur_y.set(y);
        match self.row_at(snap, y, &query) {
            Press::Group(gi) => self.open_group_menu(gi),
            Press::Tab(id, _) => self.open_tab_menu(id),
            _ => return false,
        }
        true
    }

    /// Right-click: on a group/tab row, pop up the corresponding "more" menu (positioned at the mouse).
    fn on_right_down(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        self.open_menu_at(&snap, p.x, p.y);
    }

    /// ⌃↩: open a row's "more" menu from the keyboard. The row under the pointer wins (so hover +
    /// shortcut reads as a right-click); with the pointer elsewhere it falls back to the active tab,
    /// expanding its group and scrolling it into view so the menu — and any rename box it opens —
    /// anchors somewhere visible rather than to a clipped coordinate.
    pub fn open_context_menu(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        if self.ivars().hovering.get()
            && self.open_menu_at(&snap, self.ivars().hover_x.get(), self.ivars().hover_y.get())
        {
            return;
        }
        match self.reveal_active_row() {
            Some((active, top)) => {
                self.ivars().cur_x.set(PAD + 30.0);
                self.ivars().cur_y.set(top + ROW_H);
                self.open_tab_menu(active);
            }
            None => {}
        }
    }

    /// ⌘R: rename the active session in place. Unlike ⌃↩ this ignores the pointer — a rename is a
    /// deliberate edit of one thing, and having it land on whatever the mouse happens to rest over
    /// would be a nasty surprise.
    pub fn begin_rename_active(&self) {
        if let Some((active, _)) = self.reveal_active_row() {
            self.start_edit(Editing::Tab(active));
        }
    }

    /// Bring the active tab's row on screen — expanding a collapsed group, scrolling the list, and
    /// flushing the redraw — and return its id with its top in view coordinates.
    ///
    /// Shared by ⌃↩ and ⌘R because both anchor UI to that row (a menu, an edit box) and both would
    /// otherwise anchor to a clipped coordinate. The redraw is flushed rather than merely
    /// invalidated: `popUpMenuPositioning…` runs its own modal loop, so an invalidated view would
    /// still show the pre-scroll rows underneath the menu.
    fn reveal_active_row(&self) -> Option<(u64, f64)> {
        let ctrl = self.controller()?;
        let snap = ctrl.snapshot();
        let active = snap.active?;
        let query = self.ivars().query.borrow().clone();
        // Outside search, a collapsed group hides its tabs entirely: expand it so the active row exists.
        if query.is_empty() {
            let holder = snap
                .groups
                .iter()
                .position(|g| g.collapsed && g.tabs.iter().any(|t| t.id == active));
            if let Some(gi) = holder {
                ctrl.toggle_group_collapsed(gi);
            }
        }
        let snap = ctrl.snapshot();
        let rows = self.build_rows(&snap, &query, 0.0);
        // None = filtered out by the search query.
        let top = rows.iter().find(|r| matches!(r.kind, Press::Tab(id, _) if id == active))?.top;
        let h = self.bounds().size.height;
        let (list_top, footer_top) = (self.list_top(), h - Self::footer_height());
        let mut s = self.scroll();
        if top - s < list_top {
            s = top - list_top;
        }
        if top - s + ROW_H > footer_top {
            s = top + ROW_H - footer_top;
        }
        let s = s.clamp(0.0, self.max_scroll_of(&rows, h));
        self.ivars().scroll.set(s);
        unsafe {
            self.setNeedsDisplay(true);
            self.displayIfNeeded();
        }
        Some((active, top - s))
    }

    /// Scroll the list to the bottom (called after creating a group, so the new group at the end is visible).
    pub fn scroll_to_bottom(&self) {
        if let Some(ctrl) = self.controller() {
            let snap = ctrl.snapshot();
            let query = self.ivars().query.borrow().clone();
            let h = self.bounds().size.height;
            let max = self.max_scroll(&snap, &query, h);
            self.ivars().scroll.set(max);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn on_down(&self, event: &NSEvent) {
        let snap = match self.controller() {
            Some(c) => c.snapshot(),
            None => return,
        };
        let p = self.convertPoint_fromView(unsafe { event.locationInWindow() }, None);
        let (x, y) = (p.x, p.y);
        let w = self.bounds().size.width;
        let query = self.ivars().query.borrow().clone();
        let mut press = self.row_at(&snap, y, &query);
        // A press outside the focused input leaves it: the rename is abandoned, the search box drops
        // back to its resting look. A press inside the box is swallowed so the input keeps focus.
        // Both tests use the raw row hit, before the "⋯"/dot sub-areas below are split out of it.
        if let Some(what) = self.ivars().editing.get() {
            // The rename box covers its whole row, so any press on that row lands inside the box.
            let on_edit_row = match (what, press) {
                (Editing::Tab(a), Press::Tab(b, _)) => a == b,
                (Editing::Group(a), Press::Group(b)) => a == b,
                _ => false,
            };
            if on_edit_row {
                self.ivars().press.set(Press::None);
                self.ivars().dragging.set(false);
                return;
            }
            self.cancel_rename();
        }
        // A press elsewhere used to close the search box; it no longer does. The box is dismissed
        // only by Esc, by ⌘F on an empty one, or by the magnifier — clicking a session while a
        // filter is up is *using* the filter, not leaving it.
        // Dual-button row: by x, land on "Terminal" (left) or "Group" (right).
        if press == Press::Actions {
            press = if x < Self::actions_split_x(w) { Press::NewTab } else { Press::NewGroup };
        }
        // When hitting the "⋯" area at the right of a group/tab row, pop up the corresponding more menu instead.
        if x >= w - 28.0 {
            press = match press {
                Press::Group(gi) => Press::GroupMenu(gi),
                Press::Tab(id, _) => Press::TabMenu(id),
                other => other,
            };
        }
        // Clicking the session icon → open its color picker. It is the same target the status dot
        // used to be, now that the icon has absorbed it.
        if let Press::Tab(id, grp) = press {
            let indent = if grp == UNGROUPED { 16.0 } else { 26.0 };
            if x >= indent - 3.0 && x <= indent + ICON_W + 3.0 {
                press = Press::TabDot(id);
            }
        }
        self.ivars().press.set(press);
        self.ivars().start_y.set(y);
        self.ivars().cur_x.set(x);
        self.ivars().cur_y.set(y);
        self.ivars().dragging.set(false);
    }

    fn on_drag(&self, event: &NSEvent) {
        // Dragging is only enabled when there's no search filter: while filtering, rows are a matched subset, so the drop position would be misaligned with the actual reordering.
        let draggable = matches!(self.ivars().press.get(), Press::Tab(..) | Press::Group(_))
            && self.ivars().query.borrow().is_empty();
        if draggable {
            let y = self.point_y(event);
            if (y - self.ivars().start_y.get()).abs() > 4.0 {
                self.ivars().dragging.set(true);
                // A drag suppresses the hover wash entirely, and no motion event arrives while it
                // runs — so drop the cached row, or a gesture that ends where it began would leave
                // the pointer's row looking un-hovered until it moves to a different one.
                self.ivars().hover_key.set(HoverKey { row: Press::None, half: 0 });
            }
            self.ivars().cur_y.set(y);
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn on_up(&self, event: &NSEvent) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let press = self.ivars().press.get();
        if self.ivars().dragging.get() {
            match press {
                Press::Tab(id, _) => {
                    let snap = ctrl.snapshot();
                    if let Some((g, before)) = self.tab_drop_target(&snap, id) {
                        ctrl.move_tab_to(id, g, before);
                    }
                }
                Press::Group(gi) => {
                    // The new insertion index is how many group vertical midpoints the drop position crossed.
                    let snap = ctrl.snapshot();
                    let rows = self.build_rows(&snap, "", self.scroll());
                    let heads: Vec<f64> = rows
                        .iter()
                        .filter_map(|r| match r.kind {
                            Press::Group(_) => Some(r.top),
                            _ => None,
                        })
                        .collect();
                    let list_bottom = rows.last().map(|r| r.top + r.h).unwrap_or(0.0);
                    let y = self.ivars().cur_y.get();
                    let mut target = 0usize;
                    for (i, top) in heads.iter().enumerate() {
                        let bottom = heads.get(i + 1).copied().unwrap_or(list_bottom);
                        if (top + bottom) / 2.0 < y {
                            target += 1;
                        }
                    }
                    ctrl.move_group(gi, target);
                }
                _ => {}
            }
        } else {
            match press {
                Press::Search => self.enter_search(),
                Press::NewTab => ctrl.add_tab_default(),
                Press::NewGroup => ctrl.add_group_default(),
                Press::Tab(id, _) => {
                    // Double-click → rename in place; single click → select, or deselect when it is
                    // already the active tab (the terminal area falls back to the placeholder).
                    if unsafe { event.clickCount() } >= 2 {
                        self.start_edit(Editing::Tab(id));
                    } else {
                        ctrl.toggle_select(id);
                    }
                }
                Press::Group(gi) => {
                    // Single-click a group title → collapse/expand; double-click → rename in place.
                    // AppKit still delivers the double-click's first press as clickCount 1, so that
                    // press already toggled the group; undo it here so a double-click only renames.
                    let clicks = unsafe { event.clickCount() };
                    if clicks >= 2 {
                        if clicks == 2 {
                            ctrl.toggle_group_collapsed(gi);
                        }
                        self.start_edit(Editing::Group(gi));
                    } else {
                        ctrl.toggle_group_collapsed(gi);
                    }
                }
                Press::GroupMenu(gi) => self.open_group_menu(gi),
                Press::TabMenu(id) => self.open_tab_menu(id),
                Press::TabDot(id) => self.open_dot_menu(id),
                Press::StyleMenu => {
                    if let Some(c) = self.controller() {
                        c.open_settings();
                    }
                }
                _ => {}
            }
        }
        self.ivars().press.set(Press::None);
        self.ivars().dragging.set(false);
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Draw the search box: chip fill + magnifier + placeholder/query + focus ring and caret.
    ///
    /// Always the focused state — the row it draws only exists while the search has the keyboard
    /// (see `build_rows`), so there is no resting appearance left to draw and no ⌘F badge to
    /// advertise the shortcut with. That hint moved to the strip magnifier's tooltip.
    fn draw_search(&self, row: &Row, w: f64, query: &str) {
        // The box is always drawn at its full size and clipped to however much of it is unrolled —
        // it opens like a drawer rather than squashing, which would put the text through a 28-to-0
        // vertical scale on the way past.
        let outer = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &outer {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(rect(0.0, row.top, w, row.h)) };
        let row = &Row { h: SEARCH_H, ..row.clone() };
        let box_rect = rect(HPAD, row.top, w - 2.0 * HPAD, row.h);
        round_fill(box_rect, 7.0, &overlay(CHIP_BG));
        // The accent ring is what marks the focus: the caret alone is easy to miss on an empty box,
        // and the chip fill is the same one the action buttons below carry.
        //
        // Stroked half a point *inside* the box, because a stroke is centered on its path: on the
        // box's own rect its outer half falls outside, which the reveal clip above then cuts off —
        // the ring loses its top and bottom edges and keeps only the sides.
        //
        // Only while the box actually holds the keyboard: it now stays open after losing focus, and
        // a ring on a box that keystrokes no longer reach would be pointing at the wrong place.
        let focused = self.has_keyboard();
        if focused {
            let a = accent();
            round_stroke(
                rect(box_rect.origin.x + 0.5, box_rect.origin.y + 0.5, box_rect.size.width - 1.0, box_rect.size.height - 1.0),
                6.5,
                1.0,
                &rgba(a.0, a.1, a.2, 0.7),
            );
        }
        // Magnifier SF icon on the left.
        draw_symbol("magnifyingglass", rect(HPAD + 9.0, row.top + (row.h - 13.0) / 2.0, 13.0, 13.0), text_placeholder());
        let text_x = HPAD + 27.0;
        let (text, color) = if query.is_empty() {
            ("Search…".to_string(), text_placeholder())
        } else {
            (query.to_string(), text_primary())
        };
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(color)));
        let ns = NSString::from_str(&text);
        // Scroll + clip so a long query never spills past the box (matches the rename box) — and
        // stops short of the Esc hint, which sits inside the box's right edge.
        let right_pad = 9.0 + ESC_HINT_W;
        let avail = (w - HPAD - right_pad - text_x).max(0.0);
        let caret_w = self.caret_width(query);
        let offset = (caret_w - avail).max(0.0);
        let clip = rect(text_x, row.top, avail + right_pad, row.h);
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(clip) };
        self.draw_selection(query, text_x, offset, row.top + 4.0, row.h - 9.0);
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(text_x - offset, row.top + (row.h - 16.0) / 2.0), Some(&attrs)) };
        // Cursor: at the caret position within the query (hugs the left for an empty query).
        if focused {
            unsafe {
                ns_color(text_primary()).set();
                NSRectFill(rect(text_x + caret_w - offset + 1.0, row.top + 4.0, 1.0, row.h - 9.0));
            }
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }
        // "esc" at the right, the way the box used to advertise ⌘F. Only while the box holds the
        // keyboard, because that is the only time Esc reaches it — otherwise the key belongs to the
        // terminal and the hint would be advertising someone else's shortcut.
        if focused {
            self.draw_badge("esc", w - HPAD - 9.0, row.top + row.h / 2.0);
        }
        if let Some(c) = &outer {
            unsafe { c.restoreGraphicsState() };
        }
    }

    /// In-place rename edit box: a rounded box the same width as a normal row + vertically centered text + cursor.
    fn draw_edit(&self, row: &Row, w: f64) {
        let box_rect = rect(HPAD, row.top + 1.0, w - 2.0 * HPAD, row.h - 2.0);
        let text = self.ivars().edit_buf.borrow().clone();
        round_fill(box_rect, 7.0, &overlay(0.08));
        let a = accent();
        round_stroke(box_rect, 7.0, 1.0, &rgba(a.0, a.1, a.2, 0.7));
        let text_x = HPAD + 9.0;
        let right_pad = 9.0;
        let avail = (w - HPAD - right_pad - text_x).max(0.0); // visible text width inside the box
        let attrs = make_attrs(&self.ivars().font, Some(&ns_color(text_primary())));
        let ns = NSString::from_str(&text);
        // Scroll horizontally so the caret stays inside the box when the text is longer than the box.
        let caret_w = self.caret_width(&text);
        let offset = (caret_w - avail).max(0.0);
        // Clip the text to the box interior so long content never spills past the rounded border.
        let clip = rect(text_x, row.top, avail + right_pad, row.h);
        let ctx = unsafe { NSGraphicsContext::currentContext() };
        if let Some(c) = &ctx {
            unsafe { c.saveGraphicsState() };
        }
        unsafe { NSRectClip(clip) };
        self.draw_selection(&text, text_x, offset, row.top + (row.h - 15.0) / 2.0, 15.0);
        unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(text_x - offset, row.top + (row.h - 16.0) / 2.0), Some(&attrs)) };
        // Cursor at the caret (shifted by the same scroll offset).
        unsafe {
            ns_color(text_primary()).set();
            NSRectFill(rect(text_x + caret_w - offset + 1.0, row.top + (row.h - 15.0) / 2.0, 1.0, 15.0));
        }
        if let Some(c) = &ctx {
            unsafe { c.restoreGraphicsState() };
        }
    }

    /// Glyph width of the substring before the caret, for cursor positioning (uses the sidebar font).
    fn caret_width(&self, text: &str) -> f64 {
        self.text_width(text, self.ivars().caret.get())
    }

    /// Glyph width of the first `chars` characters (sidebar font).
    fn text_width(&self, text: &str, chars: usize) -> f64 {
        let upto = &text[..Self::char_byte(text, chars)];
        if upto.is_empty() {
            return 0.0;
        }
        let attrs = make_attrs(&self.ivars().font, None);
        let ns = NSString::from_str(upto);
        unsafe { ns.sizeWithAttributes(Some(&attrs)).width }
    }

    /// Selection highlight behind the text of an input box, drawn inside its clipped region.
    /// `text_x`/`offset` are the box's text origin and horizontal scroll, as used for the caret.
    fn draw_selection(&self, text: &str, text_x: f64, offset: f64, top: f64, h: f64) {
        let (a, b) = match self.selection() {
            Some(r) => r,
            None => return,
        };
        let xa = text_x + self.text_width(text, a) - offset + 1.0;
        let xb = text_x + self.text_width(text, b) - offset + 1.0;
        let a = accent();
        unsafe {
            rgba(a.0, a.1, a.2, 0.30).set();
            NSRectFill(rect(xa, top, xb - xa, h));
        }
    }

    /// Build a menu item: title + action targeting this view + tag (holds the group index or tab id).
    fn menu_item(&self, title: &str, action: Sel, tag: isize) -> Retained<NSMenuItem> {
        let mtm = MainThreadMarker::new().expect("main thread");
        let empty = NSString::from_str("");
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), Some(action), &empty)
        };
        unsafe {
            item.setTag(tag);
            let _: () = msg_send![&item, setTarget: self];
        }
        item
    }

    /// Pop up a menu at the mouse position (the click point of "⋯" or a right-click).
    fn popup(&self, menu: &Retained<NSMenu>) {
        let loc = NSPoint::new(self.ivars().cur_x.get(), self.ivars().cur_y.get());
        unsafe { menu.popUpMenuPositioningItem_atLocation_inView(None, loc, Some(&**self)) };
    }

    /// Group "more" menu: rename / collapse-expand / delete.
    fn open_group_menu(&self, gi: usize) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let collapsed = self.controller().map(|c| c.group_collapsed(gi)).unwrap_or(false);
        let menu = NSMenu::new(mtm);
        menu.addItem(&self.menu_item("New Terminal", sel!(groupNewTab:), gi as isize));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&self.menu_item("Rename", sel!(groupRename:), gi as isize));
        let toggle = if collapsed { "Expand" } else { "Collapse" };
        menu.addItem(&self.menu_item(toggle, sel!(groupToggle:), gi as isize));
        let sep = NSMenuItem::separatorItem(mtm);
        menu.addItem(&sep);
        menu.addItem(&self.menu_item("Delete Group", sel!(groupDelete:), gi as isize));
        self.popup(&menu);
    }

    /// Whether tab `id` is currently locked.
    fn tab_locked(&self, id: u64) -> bool {
        self.controller().map(|c| c.is_tab_locked(id)).unwrap_or(false)
    }

    /// Tab "more" menu: rename / reveal / lock-unlock / close (Close is disabled while locked).
    fn open_tab_menu(&self, id: u64) {
        let mtm = MainThreadMarker::new().expect("main thread");
        let locked = self.tab_locked(id);
        let menu = NSMenu::new(mtm);
        // Manage item enablement ourselves so a locked tab's Close renders greyed-out (AppKit's
        // auto-enable would re-enable it since the item has a valid target/action).
        unsafe { menu.setAutoenablesItems(false) };
        menu.addItem(&self.menu_item("Rename", sel!(tabRename:), id as isize));
        menu.addItem(&self.menu_item("Reveal in Finder", sel!(tabRevealInFinder:), id as isize));
        let sep = NSMenuItem::separatorItem(mtm);
        menu.addItem(&sep);
        menu.addItem(&self.menu_item(if locked { "Unlock" } else { "Lock" }, sel!(tabToggleLock:), id as isize));
        let close = self.menu_item("Close", sel!(tabClose:), id as isize);
        if locked {
            unsafe { close.setEnabled(false) };
        }
        menu.addItem(&close);
        self.popup(&menu);
    }

    /// Tab color picker: the classic colors + "Default", each with a swatch and a checkmark on the
    /// tab's current color.
    ///
    /// The swatch is the **session icon itself**, in the color being offered, not the round dot it
    /// used to be. Now that hue is the icon's only variable ([`Self::draw_session_icon`]), a disc
    /// here would be a preview of something the sidebar never draws; the glyph shows the row exactly
    /// as picking that entry would leave it.
    ///
    /// It goes to the item as a **vector** symbol image ([`view::symbol_image`]), at the point size
    /// the row's own icon resolves to. Rasterizing it here — lockFocus over an `ICON_W`×`ICON_H`
    /// image, then [`draw_symbol`] — would clip it: `draw_symbol` centers a symbol at its *natural*
    /// size, which for a wide glyph overruns that box, harmlessly in an unclipped row view and
    /// invisibly-but-really inside an image whose bounds are the clip.
    fn open_dot_menu(&self, id: u64) {
        let mtm = MainThreadMarker::new().expect("main thread");
        self.ivars().dot_target.set(id);
        // Look up this tab's current color index to check the matching item.
        let cur = self.controller().map(|c| c.tab_dot(id)).unwrap_or(0);
        let menu = NSMenu::new(mtm);
        for (i, (name, rgb)) in dot_colors().iter().enumerate() {
            let item = self.menu_item(name, sel!(pickDotColor:), i as isize);
            // Slot 0's color is a sentinel and must never be drawn; "Default" previews as the tone
            // an uncolored row actually gets, so every entry in the list carries a glyph.
            let tone = if i == 0 { text_secondary() } else { *rgb };
            if let Some(img) = view::symbol_image(tab_symbol(), ICON_H * 0.92, tone) {
                unsafe { item.setImage(Some(&img)) };
            }
            if i as u8 == cur {
                unsafe {
                    let _: () = msg_send![&item, setState: 1isize];
                }
            }
            menu.addItem(&item);
        }
        self.popup(&menu);
    }

    /// Flip the search state and start the box moving toward it. Every path that opens or closes
    /// the search goes through here, so none of them can leave `reveal` stranded.
    fn set_searching(&self, on: bool) {
        self.ivars().searching.set(on);
        if self.ivars().anim.borrow().is_some() {
            return; // already travelling; the tick reads the target each time, so it just turns around
        }
        let ctx = self as *const Self as *mut c_void;
        let token = view::attach_timer(ANIM_MS, ctx, search_anim_tick);
        *self.ivars().anim.borrow_mut() = Some(token);
    }

    /// Advance `reveal` one tick toward `searching`, and stop the timer on arrival.
    fn step_search_anim(&self) {
        let target = if self.ivars().searching.get() { 1.0 } else { 0.0 };
        let step = ANIM_MS as f64 / 1000.0 / SEARCH_ANIM;
        let cur = self.ivars().reveal.get();
        let next = if target > cur { (cur + step).min(1.0) } else { (cur - step).max(0.0) };
        self.ivars().reveal.set(next);
        unsafe { self.setNeedsDisplay(true) };
        if next == target {
            if let Some(t) = self.ivars().anim.borrow_mut().take() {
                view::cancel_timer(&t);
            }
        }
    }

    fn enter_search(&self) {
        // Opening an already-open box must not reset its caret and undo history — but it must still
        // take the keyboard back, because the box now stays on screen after losing it.
        if !self.ivars().searching.get() {
            self.set_searching(true);
            self.ivars().caret.set(self.ivars().query.borrow().chars().count());
            self.reset_input();
        }
        if let Some(ctrl) = self.controller() {
            ctrl.focus_sidebar();
        }
    }

    /// Enter search state and redraw (⌘F triggered from the menu).
    pub fn begin_search(&self) {
        // ⌘F on a box that is already open, still empty and still holding the keyboard closes it
        // again: the same key that opened it, and with nothing typed there is nothing to lose by
        // doing so. With a query in it the box stays — that text is the user's work, and ⌘F is not
        // where they would expect to throw it away (Esc is).
        //
        // The keyboard is the load-bearing half of that test, because losing it no longer closes
        // the box: an empty box the user has clicked away from — or left behind by ⌘B, which hands
        // the keyboard back deliberately — is on screen and *not* listening, and there ⌘F means
        // "put me back in it". Without this the shortcut answered by folding the box away, and the
        // user had to press it twice to type anything.
        if self.has_keyboard() && self.ivars().searching.get() && self.ivars().query.borrow().is_empty() {
            self.exit_search();
        } else {
            self.enter_search();
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    fn exit_search(&self) {
        self.set_searching(false);
        self.ivars().query.borrow_mut().clear();
        self.reset_input();
        if let Some(ctrl) = self.controller() {
            ctrl.focus_terminal();
        }
    }

    /// The buffer the keyboard is editing: the rename box takes priority over the search box.
    fn active_buf(&self) -> Option<&RefCell<String>> {
        if self.ivars().editing.get().is_some() {
            Some(&self.ivars().edit_buf)
        } else if self.ivars().searching.get() {
            Some(&self.ivars().query)
        } else {
            None
        }
    }

    /// Keyboard input: only Esc/Return differ between the rename and search boxes; everything else
    /// is the shared single-line text editing below. Both only trigger when the sidebar is focused.
    fn on_key(&self, event: &NSEvent) {
        let code = unsafe { event.keyCode() };
        let editing = self.ivars().editing.get().is_some();
        let buf = match self.active_buf() {
            Some(b) => b,
            None => return,
        };
        match code {
            53 if editing => self.cancel_rename(),        // Esc
            53 => self.exit_search(),
            36 | 76 if editing => self.commit_rename(),   // Return / Enter
            36 | 76 => self.select_first_match(),
            _ => self.text_key(buf, event, code),
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    /// The standard single-line text-field keys, shared by the search and rename boxes:
    /// ⌘A/⌘C/⌘X/⌘V/⌘Z/⇧⌘Z, caret movement by character (arrows), word (⌥) and line (⌘ / Home / End /
    /// ⌃A / ⌃E), all extending the selection when ⇧ is held, plus the matching deletions.
    /// Any other ⌘-combo is swallowed rather than typed, so e.g. ⌘T doesn't insert a bare "t".
    fn text_key(&self, buf: &RefCell<String>, event: &NSEvent, code: u16) {
        let flags = unsafe { event.modifierFlags() };
        let cmd = flags.contains(NSEventModifierFlags::NSEventModifierFlagCommand);
        let shift = flags.contains(NSEventModifierFlags::NSEventModifierFlagShift);
        let alt = flags.contains(NSEventModifierFlags::NSEventModifierFlagOption);
        let ctrl = flags.contains(NSEventModifierFlags::NSEventModifierFlagControl);
        let text = buf.borrow().clone();
        let caret = self.ivars().caret.get();
        let end = text.chars().count();
        match code {
            // ---- Clipboard / undo ----
            0 if cmd => self.select_all(&text),                     // ⌘A
            8 if cmd => self.copy_selection(&text),                 // ⌘C
            7 if cmd => {                                           // ⌘X
                self.copy_selection(&text);
                self.delete_selection(buf, EditKind::None);
            }
            9 if cmd => self.paste_clipboard(buf),                  // ⌘V
            6 if cmd && shift => self.undo_redo(buf, false),        // ⇧⌘Z
            6 if cmd => self.undo_redo(buf, true),                  // ⌘Z
            // ---- Caret movement (⇧ extends the selection) ----
            123 if cmd || ctrl => self.move_caret(0, shift),        // ⌘← / ⌃←
            124 if cmd || ctrl => self.move_caret(end, shift),      // ⌘→ / ⌃→
            123 if alt => self.move_caret(word_left(&text, caret), shift),   // ⌥←
            124 if alt => self.move_caret(word_right(&text, caret), shift),  // ⌥→
            123 => match self.selection() {                         // ←
                Some((a, _)) if !shift => self.move_caret(a, false),
                _ => self.move_caret(caret.saturating_sub(1), shift),
            },
            124 => match self.selection() {                         // →
                Some((_, b)) if !shift => self.move_caret(b, false),
                _ => self.move_caret((caret + 1).min(end), shift),
            },
            115 => self.move_caret(0, shift),                       // Home
            119 => self.move_caret(end, shift),                     // End
            0 if ctrl => self.move_caret(0, shift),                 // ⌃A
            14 if ctrl => self.move_caret(end, shift),              // ⌃E
            // ---- Deletion ----
            51 if cmd => self.splice(buf, 0, caret, "", EditKind::None), // ⌘⌫: to the line start
            51 if alt => {                                          // ⌥⌫: the word before the caret
                if !self.delete_selection(buf, EditKind::Delete) {
                    self.splice(buf, word_left(&text, caret), caret, "", EditKind::None);
                }
            }
            40 if ctrl => self.splice(buf, caret, end, "", EditKind::None), // ⌃K: to the line end
            51 => {                                                 // ⌫
                if !self.delete_selection(buf, EditKind::Delete) {
                    self.splice(buf, caret.saturating_sub(1), caret, "", EditKind::Delete);
                }
            }
            117 => {                                                // ⌦ (forward delete)
                if !self.delete_selection(buf, EditKind::Delete) {
                    self.splice(buf, caret, (caret + 1).min(end), "", EditKind::Delete);
                }
            }
            _ if cmd => {}
            _ => self.caret_insert(buf, event),
        }
    }

    /// Byte offset of char index `i` into `s` (clamped to the string end).
    fn char_byte(s: &str, i: usize) -> usize {
        s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
    }

    /// The current selection as a char range, or None when it is empty.
    fn selection(&self) -> Option<(usize, usize)> {
        let c = self.ivars().caret.get();
        self.ivars().sel.get().filter(|a| *a != c).map(|a| (a.min(c), a.max(c)))
    }

    /// Move the caret, either extending the selection (⇧ held) or dropping it.
    fn move_caret(&self, to: usize, extend: bool) {
        if extend {
            if self.ivars().sel.get().is_none() {
                self.ivars().sel.set(Some(self.ivars().caret.get()));
            }
        } else {
            self.ivars().sel.set(None);
        }
        self.ivars().caret.set(to);
        // A selection that has collapsed back onto the caret is no selection at all.
        if self.ivars().sel.get() == Some(to) {
            self.ivars().sel.set(None);
        }
        self.ivars().last_edit.set(EditKind::None);
    }

    fn select_all(&self, text: &str) {
        self.ivars().sel.set(Some(0));
        self.ivars().caret.set(text.chars().count());
        self.ivars().last_edit.set(EditKind::None);
    }

    /// Replace the char range [a,b) with `text` and put the caret after the inserted text.
    /// `kind` drives undo coalescing: a run of same-kind edits shares one undo step,
    /// `EditKind::None` always starts a new one.
    fn splice(&self, buf: &RefCell<String>, a: usize, b: usize, text: &str, kind: EditKind) {
        if a >= b && text.is_empty() {
            return;
        }
        self.push_undo(buf, kind);
        let mut s = buf.borrow_mut();
        let (from, to) = (Self::char_byte(&s, a), Self::char_byte(&s, b));
        s.replace_range(from..to, text);
        drop(s);
        self.ivars().caret.set(a + text.chars().count());
        self.ivars().sel.set(None);
    }

    /// Delete the selection if there is one, reporting whether anything was deleted.
    fn delete_selection(&self, buf: &RefCell<String>, kind: EditKind) -> bool {
        match self.selection() {
            Some((a, b)) => {
                self.splice(buf, a, b, "", kind);
                true
            }
            None => false,
        }
    }

    /// Record the pre-edit state for ⌘Z. Consecutive edits of the same kind (a typed run, a run of
    /// backspaces) reuse the step already on the stack so one ⌘Z undoes the whole run.
    fn push_undo(&self, buf: &RefCell<String>, kind: EditKind) {
        self.ivars().redo.borrow_mut().clear();
        if kind == EditKind::None || self.ivars().last_edit.get() != kind {
            let mut undo = self.ivars().undo.borrow_mut();
            undo.push((buf.borrow().clone(), self.ivars().caret.get()));
            if undo.len() > UNDO_DEPTH {
                undo.remove(0);
            }
        }
        self.ivars().last_edit.set(kind);
    }

    /// ⌘Z / ⇧⌘Z: swap the buffer with the top of the undo (or redo) stack.
    fn undo_redo(&self, buf: &RefCell<String>, undo: bool) {
        let (from, to) = if undo {
            (&self.ivars().undo, &self.ivars().redo)
        } else {
            (&self.ivars().redo, &self.ivars().undo)
        };
        let (text, caret) = match from.borrow_mut().pop() {
            Some(s) => s,
            None => return,
        };
        to.borrow_mut().push((buf.borrow().clone(), self.ivars().caret.get()));
        let n = text.chars().count();
        *buf.borrow_mut() = text;
        self.ivars().caret.set(caret.min(n));
        self.ivars().sel.set(None);
        self.ivars().last_edit.set(EditKind::None);
    }

    /// ⌘C / ⌘X: put the selected text on the general pasteboard (a no-op without a selection).
    fn copy_selection(&self, text: &str) {
        let (a, b) = match self.selection() {
            Some(r) => r,
            None => return,
        };
        let part = &text[Self::char_byte(text, a)..Self::char_byte(text, b)];
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        unsafe {
            pb.clearContents();
            pb.setString_forType(&NSString::from_str(part), NSPasteboardTypeString);
        }
    }

    /// ⌘V: insert the pasteboard text over the selection. These are single-line fields (and tab
    /// names are stored in a line-based config), so control characters are dropped.
    fn paste_clipboard(&self, buf: &RefCell<String>) {
        let s = match unsafe { NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString) } {
            Some(s) => s.to_string(),
            None => return,
        };
        let text: String = s.chars().filter(|c| config::is_typable(*c)).collect();
        if text.is_empty() {
            return;
        }
        let (a, b) = self.selection().unwrap_or_else(|| {
            let c = self.ivars().caret.get();
            (c, c)
        });
        self.splice(buf, a, b, &text, EditKind::None);
    }

    /// Insert the event's typable characters, replacing the selection.
    fn caret_insert(&self, buf: &RefCell<String>, event: &NSEvent) {
        let s = match unsafe { event.characters() } {
            Some(s) => s.to_string(),
            None => return,
        };
        let text: String = s.chars().filter(|c| config::is_typable(*c)).collect();
        if text.is_empty() {
            return;
        }
        let (a, b) = self.selection().unwrap_or_else(|| {
            let c = self.ivars().caret.get();
            (c, c)
        });
        self.splice(buf, a, b, &text, EditKind::Insert);
    }

    /// Whether this view currently holds the window's keyboard focus. The search box stays on
    /// screen after losing it, so "the search is on" and "the search has the keyboard" are two
    /// different questions now, and the focus marks answer this one.
    fn has_keyboard(&self) -> bool {
        let Some(window) = self.window() else { return false };
        let Some(fr) = window.firstResponder() else { return false };
        std::ptr::eq(&*fr as *const _ as *const u8, self as *const Self as *const u8)
    }

    /// Reset the shared input state (selection + undo history) when a box takes or loses focus.
    fn reset_input(&self) {
        self.ivars().sel.set(None);
        self.ivars().undo.borrow_mut().clear();
        self.ivars().redo.borrow_mut().clear();
        self.ivars().last_edit.set(EditKind::None);
    }

    fn select_first_match(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let snap = ctrl.snapshot();
        let query = self.ivars().query.borrow().clone();
        let first = self.build_rows(&snap, &query, self.scroll()).into_iter().find_map(|r| match r.kind {
            Press::Tab(id, _) => Some(id),
            _ => None,
        });
        if let Some(id) = first {
            // The box stays: Return picks a session out of the filter, it does not leave it. Focus
            // follows the selection to the terminal, so the box is left showing its query without
            // the focus ring — see `has_keyboard`.
            ctrl.select(id);
        }
    }

    /// Start an in-place rename (tab or group): the buffer is initialized to the current name, and the sidebar takes keyboard focus.
    fn start_edit(&self, what: Editing) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let init = match what {
            Editing::Tab(id) => ctrl.tab_title(id),
            Editing::Group(gi) => ctrl.group_name(gi),
        };
        self.set_searching(false);
        self.ivars().query.borrow_mut().clear();
        self.ivars().editing.set(Some(what));
        self.ivars().caret.set(init.chars().count()); // caret at end of the initial name
        *self.ivars().edit_buf.borrow_mut() = init;
        self.reset_input();
        ctrl.focus_sidebar();
        unsafe { self.setNeedsDisplay(true) };
    }

    fn commit_rename(&self) {
        if let (Some(what), Some(ctrl)) = (self.ivars().editing.get(), self.controller()) {
            let name = self.ivars().edit_buf.borrow().trim().to_string();
            if !name.is_empty() {
                match what {
                    Editing::Tab(id) => ctrl.rename_tab(id, name),
                    Editing::Group(gi) => ctrl.rename_group(gi, name),
                }
            }
        }
        self.cancel_rename();
    }

    /// Leave whichever input box is focused, on an event from outside the sidebar (e.g. it is being
    /// collapsed — an input the user can't see must not keep the keyboard). A no-op when neither box
    /// is active, so it never steals focus from the terminal.
    pub fn end_input(&self) {
        if self.ivars().editing.get().is_some() {
            self.cancel_rename();
        } else if self.ivars().searching.get() {
            // The search box survives a collapse — it closes only when the user closes it — but the
            // keyboard must not go with it: a card parked off the window edge holding first
            // responder would swallow everything typed at the terminal.
            if let Some(ctrl) = self.controller() {
                ctrl.focus_terminal();
            }
            unsafe { self.setNeedsDisplay(true) };
        }
    }

    fn cancel_rename(&self) {
        self.ivars().editing.set(None);
        self.ivars().edit_buf.borrow_mut().clear();
        self.reset_input();
        if let Some(ctrl) = self.controller() {
            ctrl.focus_terminal();
        }
        unsafe { self.setNeedsDisplay(true) };
    }
}

/// Char index of the word boundary before `at`: skip the separators immediately left of the caret,
/// then the word itself (⌥← and ⌥⌫).
fn word_left(text: &str, at: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut i = at.min(chars.len());
    while i > 0 && !chars[i - 1].is_alphanumeric() {
        i -= 1;
    }
    while i > 0 && chars[i - 1].is_alphanumeric() {
        i -= 1;
    }
    i
}

/// Char index of the word boundary after `at` (⌥→), mirroring `word_left`.
fn word_right(text: &str, at: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut i = at.min(chars.len());
    while i < chars.len() && !chars[i].is_alphanumeric() {
        i += 1;
    }
    while i < chars.len() && chars[i].is_alphanumeric() {
        i += 1;
    }
    i
}

/// Whether a character is typable text: excludes control characters and AppKit
/// function-key private-use code points (U+E000..U+F8FF; arrow keys / Home / End /
/// PageUp / forward-delete land here and must not leak into search/rename text).
/// sRGB color (with alpha).
fn rgba(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a) }
}

/// GCD trampoline for the search box's unroll (see `SidebarView::set_searching`). The context is
/// the view; the timer that carries it is cancelled from `step_search_anim` the moment the box
/// arrives, so it cannot outlive one animation.
extern "C" fn search_anim_tick(ctx: *mut c_void) {
    let view = unsafe { &*(ctx as *const SidebarView) };
    view.step_search_anim();
}

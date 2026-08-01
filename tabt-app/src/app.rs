//! AppController: runtime management of multi-tab / grouping.
//!
//! Owns all sessions (each tab = one PTY + one [`TermView`]); responsible for creating / switching /
//! moving / closing tabs, mounting the current active tab's view into the right-hand host container,
//! and persisting the group layout to ~/.tabt. It is a plain Rust struct (held in an `Rc`, exclusive to
//! the main thread); the sidebar and TermView call back into it via a raw pointer -- as long as it is
//! alive (main holds the `Rc` until app.run ends), the pointer stays valid.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::os::unix::io::RawFd;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, msg_send_id};
use objc2_app_kit::{
    NSAlert, NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSModalResponseOK, NSSavePanel,
    NSAnimationContext, NSAutoresizingMaskOptions, NSView, NSWindow, NSWindowButton,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use crate::card::{self, CARD_GAP, CARD_INSET};
use crate::config;
use crate::divider::{Divider, DIVIDER_W};
use crate::header::{HeaderView, HEADER_H};
use crate::placeholder::PlaceholderView;
use crate::pty;
use crate::settings;
use crate::settings_dialog::SettingsDialog;
use crate::sidebar::{SidebarView, MAX_SIDEBAR_W, MIN_SIDEBAR_W, SIDEBAR_W};
use crate::theme;
use crate::toggle::{StripButton, TOGGLE_W};
use crate::toolbar::Toolbar;
use crate::view::{self, TermView};

/// Duration of the sidebar collapse/expand slide. Matches the pace of the system's own sidebar
/// animations — long enough to read as motion, short enough not to sit in the way.
const SIDEBAR_ANIM: f64 = 0.22;

/// The narrowest the terminal pane may become, and through it the narrowest the window may be
/// dragged (see [`AppController::sync_window_min_width`]).
///
/// About forty columns at the default font — a width a wrapped `ls` or a git log still reads at, and
/// the point below which the pane stops being a terminal and starts being a sliver. It is a fixed
/// number of points rather than a number of columns, because the sidebar it has to be added to is
/// measured in points too and the window limit has to hold while the user is changing the font.
const MIN_TERM_W: f64 = 320.0;

/// Where the session name starts, measured from the terminal's own left edge.
const TITLE_INSET: f64 = 16.0;

/// Right edge of the toolbar's buttons (collapse + search), in window coordinates. AppKit places
/// the items itself — leading edge of the toolbar, just past the traffic lights — so this is
/// measured off a screenshot rather than derived, and it is what the session title has to clear
/// whenever the sidebar is not sitting between the two.
const TOOLBAR_ITEMS_RIGHT_X: f64 = 170.0;

/// Half-period of the cursor blink, in milliseconds: the phase flips on every tick, so the cursor
/// completes a cycle in twice this. Matches the pace of the system's own text carets.
const BLINK_MS: u64 = 530;

/// What a session is doing, as far as the sidebar is concerned.
///
/// Sampled on a timer rather than computed on demand: `Running` comes from a `tcgetpgrp` syscall,
/// and the sidebar redraws far too often — once per hovered row, once per keystroke that changes a
/// title — to afford one per tab per frame.
#[derive(Clone, Copy, PartialEq)]
pub enum SessionState {
    /// A foreground job other than the shell itself owns the terminal — a build, an editor, ssh.
    Running,
    /// The shell is sitting at its prompt.
    Idle,
    /// The shell exited and has not been restarted; the tab is a placeholder (see `end_tab_session`).
    Ended,
}

/// How often each session's state is sampled. Slow enough that a machine full of idle tabs costs
/// nothing measurable, fast enough that starting a build marks its tab before you look away.
const SAMPLE_MS: u64 = 800;

struct Tab {
    id: u64,
    /// The name the user gave this tab with ⌘R. Only meaningful when `pinned`; otherwise the
    /// displayed name is derived (see `Tab::display_title`).
    title: String,
    /// Whether `title` was chosen by the user and must not be overwritten by the shell.
    pinned: bool,
    /// Last title the shell reported via OSC 0/1/2, already sanitized; empty when it has reported
    /// none, which is the common case — a stock zsh on macOS never sets one.
    osc_title: String,
    dot: u8,      // tab color index (0 = default/auto; 1..=8 = a slot of the theme's palette, see sidebar::dot_colors)
    locked: bool, // locked tabs are protected from being closed by the user (⌘W / the tab menu)
    view: Retained<TermView>,
    master_fd: RawFd,
    shell_pid: libc::pid_t, // the shell's own pid/pgid; used to detect a foreground job (pty::has_foreground_job)
    reader: view::ReaderToken,
    spawn_cwd: String, // working directory at spawn time: cwd fallback when OSC 7 has not reported
    /// Last sampled state; see `sample_states`. Kept on the tab rather than recomputed per draw.
    state: SessionState,
    /// This session printed something you have not looked at. Set by the sampler for background
    /// tabs only, cleared by selecting the tab.
    activity: bool,
    /// This session rang the bell (BEL) and you have not looked at it since.
    bell: bool,
    /// `TermView::output_seq` as of the last sample, to spot new output without the PTY path
    /// having to notify anyone.
    last_seq: u64,
    // Whether this session has settled after start-up. See `sample_states`: everything a shell
    // prints on its way to its first prompt is baseline, not activity.
    primed: bool,
    /// The shell this session is actually running. Per tab, not read from Settings: changing the
    /// setting deliberately leaves running shells alone, so the global value names what the *next*
    /// tab will run and would misreport this one.
    shell: String,
}

impl Tab {
    /// This tab's working directory: the live OSC 7 report when the shell has made one, else the
    /// directory it was spawned in. Every caller wants that fallback — a shell that never reports
    /// would otherwise look like it has no cwd at all.
    fn cwd(&self) -> String {
        let live = self.view.cwd();
        if live.is_empty() {
            self.spawn_cwd.clone()
        } else {
            live
        }
    }

    /// The name to show for this tab.
    ///
    /// A ⌘R rename wins permanently; otherwise the shell's own OSC title does; otherwise the tab is
    /// named after where it is. The last rung is only reached by a tab with no cwd at all, which in
    /// practice means the spawn failed.
    ///
    /// The chain matters more than it looks: a stock macOS zsh reports no title, because
    /// `/etc/zshrc` only sources a title-setting hook for Apple_Terminal and this app deliberately
    /// identifies itself as TabT. So for most users the *directory* is the name, and OSC titles are
    /// what ssh, tmux, vim and the like contribute on top.
    fn display_title(&self) -> String {
        if self.pinned && !self.title.is_empty() {
            return self.title.clone();
        }
        if !self.osc_title.is_empty() {
            return self.osc_title.clone();
        }
        let cwd = self.cwd();
        if !cwd.is_empty() {
            return abbreviate_dir(&cwd);
        }
        "Terminal".to_string()
    }
}

/// A path with `$HOME` collapsed to `~`, the way every shell prompt writes it. Unlike
/// [`abbreviate_dir`] this keeps the whole path: the header has the width for it, and there the
/// point is to say *where* the session is, not merely to name it.
fn home_relative(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() || !path.starts_with(&home) {
        return path.to_string();
    }
    match &path[home.len()..] {
        "" => "~".to_string(),
        rest if rest.starts_with('/') => format!("~{}", rest),
        _ => path.to_string(), // a sibling like /Users/garyx, which is not inside $HOME at all
    }
}

/// The last component of a path, with `$HOME` itself shown as `~`. Used to name a tab after the
/// directory it sits in — the leaf is what identifies it; the rest is noise in a 200pt-wide row.
fn abbreviate_dir(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path == home {
        return "~".to_string();
    }
    match path.rsplit('/').find(|c| !c.is_empty()) {
        Some(leaf) => leaf.to_string(),
        None => "/".to_string(), // the root directory has no leaf
    }
}

struct Group {
    name: String,
    collapsed: bool, // when collapsed, the sidebar hides its tabs
    tabs: Vec<u64>,  // ordered tab ids
}

struct Model {
    ungrouped: Vec<u64>, // ids of tabs not belonging to any group (ordered, rendered at the top of the list)
    groups: Vec<Group>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_id: u64,
}

/// One tab, as the sidebar sees it. A struct rather than the tuple this used to be: the sidebar
/// needs to report what a session is actually doing, not just name it, and that is more fields than
/// a tuple can carry legibly.
pub struct TabSnap {
    pub id: u64,
    pub title: String,
    pub dot: u8,      // status-dot color index (0 = default/auto)
    pub locked: bool, // protected from user-initiated close
    pub state: SessionState,
    pub activity: bool, // unseen output
    pub bell: bool,     // unseen BEL
    /// Absolute working directory; "" only if the shell never started. Not drawn — the sidebar
    /// searches it, so a session can be found by where it is rather than only by what it is called.
    pub cwd: String,
}

/// Group snapshot used by the sidebar for drawing.
pub struct GroupSnap {
    pub name: String,
    pub collapsed: bool,
    pub tabs: Vec<TabSnap>,
}

/// Read-only snapshot used by the sidebar for drawing (does not expose internal details like Retained).
pub struct Snapshot {
    pub ungrouped: Vec<TabSnap>, // tabs belonging to no group, rendered at the top
    pub groups: Vec<GroupSnap>,
    pub active: Option<u64>,
    pub style: usize,
}

pub struct AppController {
    model: RefCell<Model>,
    window: Retained<NSWindow>,
    sidebar: Retained<SidebarView>,
    card: Retained<NSView>, // floating rounded panel the sidebar is mounted in (see card.rs)
    host: Retained<NSView>,
    // The card's own top-strip buttons (see toggle.rs): search, then collapse.
    search_btn: Retained<StripButton>,
    toggle_btn: Retained<StripButton>,
    divider: Retained<Divider>,
    // The *collapsed* state's sidebar button, as a native toolbar item — while the sidebar is
    // showing, `toggle_btn` above is the one on screen instead. `header` below is the theme-colored
    // band both sit on, and draws the session name (see `toolbar.rs` for why the title is not an
    // item of its own).
    toolbar: Toolbar,
    header: Retained<HeaderView>, // terminal-pane header bar (top of host)
    placeholder: Retained<PlaceholderView>, // empty-state view shown when there are no sessions
    style: Cell<usize>,        // index of the current color theme (into theme::names())
    collapsed: Cell<bool>,     // whether the sidebar is collapsed/hidden
    sidebar_w: Cell<f64>,      // current sidebar width (draggable)
    sidebar_right: Cell<bool>, // whether the sidebar is docked on the right
    // Last string handed to `setTitle:`, so `update_title` can skip a no-op call — it now runs on
    // every OSC title/cwd report, and each `setTitle:` relays out the title bar.
    last_window_title: RefCell<String>,
    // AppKit's own default x for the three traffic lights, captured the first time they are laid
    // out. `reposition_traffic_lights` shifts them right when the card is under them, and it can
    // run when AppKit has *not* reset the frames — offsetting the live x would then accumulate and
    // walk the buttons across the title bar, so they are always placed from this baseline.
    light_x0: Cell<Option<[f64; 3]>>,
    // Where the window was when it was last saved, read out of the config by `bootstrap` and put
    // down by `place_window`. None (an older config, or a first run) means "center it".
    saved_origin: Cell<Option<NSPoint>>,
    // The tab that was active before the current one, for ⌘~. Written by `select` only when the
    // selection actually moves, so repeatedly selecting the same tab cannot make it point at itself
    // and turn the shortcut into a no-op.
    prev_active: Cell<Option<u64>>,
    animating: Cell<bool>, // a collapse/expand is in flight: frame changes animate instead of snapping
    settings_dialog: RefCell<Option<Retained<SettingsDialog>>>, // lazily built settings panel
    blink_timer: RefCell<Option<view::TimerToken>>, // running only while the cursor blinks
    // Samples every session's running state. Unlike the blink timer this runs for the controller's
    // whole life: the blink is opt-in (Settings → Terminal) and off by default, so there would be
    // no timer at all to hang this off.
    state_timer: RefCell<Option<view::TimerToken>>,
    mtm: MainThreadMarker,
}

impl AppController {
    pub fn new(
        mtm: MainThreadMarker,
        window: Retained<NSWindow>,
        sidebar: Retained<SidebarView>,
        card: Retained<NSView>,
        host: Retained<NSView>,
        search_btn: Retained<StripButton>,
        toggle_btn: Retained<StripButton>,
        divider: Retained<Divider>,
    ) -> Rc<Self> {
        // The window's toolbar: the sidebar button, placed by AppKit at the leading edge of the
        // title-bar band. Attached before anything is laid out, since its band is what `band_h`
        // measures and every frame below the top strip starts under it.
        let toolbar = Toolbar::attach(mtm, &window);
        // The band it sits on: pinned to the top of host, full width. It paints the terminal's
        // background under the toolbar and draws the session title, so it spans the terminal rather
        // than the window — the title starts at the terminal's own left edge, and the sidebar's own
        // top strip is the card's, holding the traffic lights.
        let hb = host.bounds();
        let header = HeaderView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, hb.size.height - HEADER_H), NSSize::new(hb.size.width, HEADER_H)),
        );
        unsafe {
            header.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable | NSAutoresizingMaskOptions::NSViewMinYMargin,
            );
            host.addSubview(&header);
        }
        // Empty-state placeholder occupying the terminal area (below the header); mounted only when there are no sessions.
        let placeholder = PlaceholderView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(hb.size.width, (hb.size.height - HEADER_H).max(0.0))),
        );
        unsafe {
            placeholder.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
        }
        let c = Rc::new(AppController {
            model: RefCell::new(Model { ungrouped: Vec::new(), groups: Vec::new(), tabs: Vec::new(), active: None, next_id: 1 }),
            window,
            sidebar,
            card,
            host,
            search_btn,
            toggle_btn,
            divider,
            toolbar,
            header,
            placeholder,
            style: Cell::new(0),
            collapsed: Cell::new(false),
            sidebar_w: Cell::new(SIDEBAR_W),
            sidebar_right: Cell::new(false),
            last_window_title: RefCell::new(String::new()),
            light_x0: Cell::new(None),
            saved_origin: Cell::new(None),
            prev_active: Cell::new(None),
            animating: Cell::new(false),
            settings_dialog: RefCell::new(None),
            blink_timer: RefCell::new(None),
            state_timer: RefCell::new(None),
            mtm,
        });
        // The sidebar / strip buttons / divider get the controller's raw pointer (the controller lives in an Rc, so its address is stable).
        c.sidebar.set_controller(Rc::as_ptr(&c));
        c.search_btn.set_controller(Rc::as_ptr(&c));
        c.toggle_btn.set_controller(Rc::as_ptr(&c));
        c.divider.set_controller(Rc::as_ptr(&c));
        c
    }

    /// Drag to adjust the sidebar width (ignored when collapsed).
    ///
    /// Bounded by the window as well as by `MIN_SIDEBAR_W`..`MAX_SIDEBAR_W`: the same rule
    /// [`Self::sync_window_min_width`] enforces from the other side, since a divider dragged to the
    /// far edge squeezes the terminal exactly as a window dragged narrow does. The window's own
    /// minimum keeps the room here from falling below `MIN_SIDEBAR_W`, but the `max` guards it
    /// anyway — a config restored from before that minimum existed can start narrower, and
    /// `clamp` with a max below its min panics, which under `panic = "abort"` is the app.
    pub fn set_sidebar_width(&self, w: f64) {
        if self.collapsed.get() {
            return;
        }
        let room = self.content_width() - CARD_INSET - CARD_GAP - MIN_TERM_W;
        let max = MAX_SIDEBAR_W.min(room.max(MIN_SIDEBAR_W));
        self.sidebar_w.set(w.clamp(MIN_SIDEBAR_W, max));
        self.relayout();
    }

    /// Width of the window's content area — what the layout is measured in, and what the window's
    /// minimum is expressed in.
    fn content_width(&self) -> f64 {
        self.window.contentView().map(|c| c.bounds().size.width).unwrap_or(0.0)
    }

    /// Stop the window being resized narrower than the sidebar plus a usable terminal.
    ///
    /// Without a minimum the window shrinks until the card is all there is: the terminal pane goes
    /// to nothing, `dims()` floors it at one column, and the shell is left rendering into a strip.
    /// The limit has to be *recomputed*, not set once at startup, because half of it is the sidebar
    /// width — a divider drag and a collapse both move it, and both come through `relayout`.
    ///
    /// Only the width is constrained; the height keeps AppKit's own behavior (a `0` in an
    /// `NSSize` minimum means "no minimum", which is what this window has always had vertically).
    ///
    /// A window already narrower than the new limit is widened to it, which `setContentMinSize:`
    /// does not do on its own — it constrains the next resize and leaves the current frame alone.
    /// Two cases reach that: a `layout.conf` written before this limit existed, and expanding the
    /// sidebar back into a window that was narrowed while it was collapsed. The delta is applied to
    /// the *frame*, so whatever the border insets are they stay out of the arithmetic.
    fn sync_window_min_width(&self) {
        let min = if self.collapsed.get() {
            MIN_TERM_W
        } else {
            CARD_INSET + self.sidebar_w.get() + CARD_GAP + MIN_TERM_W
        };
        unsafe { self.window.setContentMinSize(NSSize::new(min, 0.0)) };
        let short = min - self.content_width();
        if short > 0.0 {
            let mut f = self.window.frame();
            f.size.width += short;
            self.window.setFrame_display(f, true);
        }
    }

    /// Divider drag: window coordinate x -> sidebar card width. The cursor tracks the middle of the
    /// gutter between card and terminal, so the card's far edge is half a gutter away from it, and
    /// its near edge is CARD_INSET in from the window edge.
    pub fn drag_sidebar_width(&self, window_x: f64) {
        let fw = self
            .window
            .contentView()
            .map(|c| c.bounds().size.width)
            .unwrap_or(0.0);
        let w = if self.sidebar_right.get() {
            fw - CARD_INSET - (window_x + CARD_GAP / 2.0)
        } else {
            window_x - CARD_GAP / 2.0 - CARD_INSET
        };
        self.set_sidebar_width(w);
    }

    /// Point the toolbar's sidebar button at the menu target, which is what implements
    /// `toggleSidebar:`. Called from `main` once that object exists — the toolbar is built with the
    /// window, well before it.
    pub fn set_toolbar_target(&self, target: &objc2::runtime::AnyObject) {
        self.toolbar.set_target(target);
    }

    /// Persist the current layout (called when a divider drag ends).
    pub fn save_layout(&self) {
        self.save();
    }

    /// Set the sidebar's left/right position and persist.
    pub fn set_sidebar_side(&self, right: bool) {
        if self.sidebar_right.get() == right {
            return;
        }
        self.sidebar_right.set(right);
        self.relayout();
        self.save();
    }

    pub fn sidebar_on_right(&self) -> bool {
        self.sidebar_right.get()
    }

    /// Centerline the top strip sits on, as a distance from the window's top edge — traffic lights,
    /// toolbar toggle and session title all share it, so the row reads as one line.
    ///
    /// The toolbar owns that row now, and it centers its items in the band, so this follows the
    /// band rather than leading it: the lights and the title are placed onto the line the system
    /// already put the toggle on. [`Self::band_h`]'s fallback is what makes the `unwrap`-free
    /// arithmetic safe before the window can report a band at all.
    fn titlebar_center_y(&self) -> f64 {
        self.band_h() / 2.0
    }

    /// x of the right edge of the traffic-light cluster, in window coordinates. AppKit lays the
    /// three buttons out to about 72pt from the window's left edge, and
    /// [`Self::reposition_traffic_lights`] then shifts the whole cluster right by CARD_INSET —
    /// anything that has to sit clear of them has to follow the same shift, so both come from here.
    fn lights_right_x(&self) -> f64 {
        72.0 + CARD_INSET
    }

    /// Move the traffic-light buttons onto [`Self::titlebar_center_y`], shifted right by the
    /// card's inset.
    ///
    /// The card's top-left corner is CARD_INSET in from the window's on both axes, and AppKit
    /// positions these buttons against the *window*. Left alone they end up hard against the
    /// card's rounded corner; translating by the same inset restores the standard corner spacing,
    /// measured from the edge the user actually sees.
    ///
    /// The shift is unconditional — the same offset whether the sidebar is showing or collapsed —
    /// so the buttons never move. Making it depend on the card being visible would look right in
    /// each state on its own, but every collapse would nudge them and every expand would nudge
    /// them back.
    ///
    /// macOS resets the buttons on resize, so this is re-applied from windowDidResize. The frame
    /// it reads back is therefore AppKit's default one — but not reliably so on every call, which
    /// is why the baseline x is captured once rather than offset in place each time.
    pub fn reposition_traffic_lights(&self) {
        let buttons = [
            NSWindowButton::NSWindowCloseButton,
            NSWindowButton::NSWindowMiniaturizeButton,
            NSWindowButton::NSWindowZoomButton,
        ];
        if self.light_x0.get().is_none() {
            let mut xs = [0.0f64; 3];
            for (i, b) in buttons.iter().enumerate() {
                match self.window.standardWindowButton(*b) {
                    Some(btn) => xs[i] = btn.frame().origin.x,
                    None => return, // not laid out yet — the next call will capture them
                }
            }
            self.light_x0.set(Some(xs));
        }
        let x0 = match self.light_x0.get() {
            Some(x) => x,
            None => return,
        };
        let dx = CARD_INSET;
        for (i, b) in buttons.iter().enumerate() {
            if let Some(btn) = self.window.standardWindowButton(*b) {
                // The buttons live in the (non-flipped) titlebar container that spans the full
                // window height, so its top edge is at `super_h`; measure the centerline down
                // from there.
                let super_h = unsafe { btn.superview() }.map(|s| s.frame().size.height).unwrap_or(0.0);
                if super_h <= 0.0 {
                    continue;
                }
                let bh = btn.frame().size.height;
                let mut o = btn.frame().origin;
                o.x = x0[i] + dx;
                o.y = super_h - self.titlebar_center_y() - bh / 2.0; // down = smaller y
                unsafe { btn.setFrameOrigin(o) };
            }
        }
    }

    /// Keep the AppKit-drawn window chrome in step with the theme.
    ///
    /// Two things here are drawn by AppKit, not by us, and both default to the *system* appearance
    /// — which is wrong whenever the theme's lightness disagrees with it (the usual case: a dark
    /// theme under a light system appearance).
    ///
    /// 1. The backdrop. Nothing normally shows it: the sidebar and the terminal host tile the whole
    ///    content view. But their seam lands wherever the sidebar width puts it, which is rarely a
    ///    whole device pixel, so both views antialias their edge against the backdrop and let a
    ///    fraction of it through as a hairline — a bright `windowBackgroundColor` line at the
    ///    default. Painting it with the theme background blends it away.
    /// 2. The window frame, which strokes a light highlight along its top edge. There is no API to
    ///    suppress that stroke; matching the window's appearance to the theme makes it dark instead.
    ///
    /// (Same reason the header/placeholder track the theme, and the settings dialog pins its own.)
    fn sync_window_chrome(&self) {
        let t = theme::current();
        // Translucency is per-fill: the window's own color carries the alpha and `setOpaque:` has
        // to agree with it, but nothing here touches `setAlphaValue`, which would fade the text too.
        self.window.setBackgroundColor(Some(&view::ns_color_bg(t.bg)));
        self.window.setOpaque(settings::opacity() >= 1.0);
        let name = unsafe {
            if t.is_dark() { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua }
        };
        if let Some(ap) = NSAppearance::appearanceNamed(name) {
            let _: () = unsafe { msg_send![&*self.window, setAppearance: &*ap] };
        }
        // The card's fill/border/shadow live on a layer, and the toolbar's icons are baked images,
        // so both hold concrete colors and have to be repainted here rather than re-read during a
        // `drawRect:`.
        card::apply_theme(&self.card);
        self.toolbar.apply_theme();
        // The settings panel may be open on the Toolbar pane, whose band preview is painted in the
        // theme's colors — it is a `drawRect:` view, but nothing else invalidates it from here.
        if let Some(d) = self.settings_dialog.borrow().as_ref() {
            d.theme_changed();
        }
    }

    /// Set `view`'s frame — animated while a sidebar collapse/expand is in flight, instant
    /// otherwise (a divider drag or a window resize must track the mouse frame for frame).
    fn set_frame_maybe_animated(&self, view: &NSView, frame: NSRect) {
        if !self.animating.get() {
            unsafe { view.setFrame(frame) };
            return;
        }
        unsafe {
            NSAnimationContext::beginGrouping();
            NSAnimationContext::currentContext().setDuration(SIDEBAR_ANIM);
            let anim: Retained<AnyObject> = msg_send_id![view, animator];
            let _: () = msg_send![&*anim, setFrame: frame];
            NSAnimationContext::endGrouping();
        }
    }

    fn set_card_frame(&self, frame: NSRect) {
        self.set_frame_maybe_animated(&self.card, frame);
    }

    /// Re-lay out the sidebar, divider, and terminal host per the current width / side / collapsed state,
    /// and set the autoresizing mask (on window resize: the sidebar keeps a fixed width against the edge, host fills the rest).
    fn relayout(&self) {
        use NSAutoresizingMaskOptions as M;
        // First, because it can widen the window: everything below measures against that width, and
        // the sidebar half of the limit is exactly what the callers of this method just changed.
        self.sync_window_min_width();
        let full = self
            .window
            .contentView()
            .map(|c| c.bounds())
            .unwrap_or_else(|| self.host.frame());
        let (fw, fh) = (full.size.width, full.size.height);
        let w = self.sidebar_w.get();
        let right = self.sidebar_right.get();
        // Everything below the toolbar band shares this height; the band itself spans the window.
        let mk = |x: f64, width: f64| NSRect::new(NSPoint::new(x, 0.0), NSSize::new(width, fh));
        // The card floats: inset from the window edges on its three outer sides, and separated from
        // the terminal by CARD_GAP. The terminal itself still runs to the window edge.
        let card = NSRect::new(
            NSPoint::new(if right { fw - CARD_INSET - w } else { CARD_INSET }, CARD_INSET),
            NSSize::new(w, (fh - 2.0 * CARD_INSET).max(0.0)),
        );
        // Collapsed, the card is parked past the window edge it docks to rather than hidden: an
        // off-screen frame is what `toggle_sidebar` slides it to and from, and the window clips it
        // there just as effectively as `setHidden:` would. Far enough out to take the card's shadow
        // with it — parked flush, the blur's tail stays behind as a smudge down the window edge.
        let off = w + CARD_INSET + card::SHADOW_REACH;
        let parked = NSRect::new(
            NSPoint::new(if right { fw + CARD_INSET + card::SHADOW_REACH } else { -off }, card.origin.y),
            card.size,
        );
        unsafe {
            if self.collapsed.get() {
                self.set_card_frame(parked);
                self.divider.setHidden(true);
                self.host.setFrame(mk(0.0, fw));
                self.host.setAutoresizingMask(M::NSViewWidthSizable | M::NSViewHeightSizable);
            } else {
                self.divider.setHidden(false);
                self.set_card_frame(card);
                self.host.setAutoresizingMask(M::NSViewWidthSizable | M::NSViewHeightSizable);
                // Seam = the middle of the gutter between the card and the terminal.
                let seam = if right { card.origin.x - CARD_GAP / 2.0 } else { card.origin.x + w + CARD_GAP / 2.0 };
                if right {
                    self.card.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMinXMargin);
                    self.host.setFrame(mk(0.0, seam - CARD_GAP / 2.0));
                    self.divider.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMinXMargin);
                } else {
                    self.card.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMaxXMargin);
                    self.host.setFrame(mk(seam + CARD_GAP / 2.0, fw - seam - CARD_GAP / 2.0));
                    self.divider.setAutoresizingMask(M::NSViewHeightSizable | M::NSViewMaxXMargin);
                }
                self.divider.setFrame(NSRect::new(
                    NSPoint::new(seam - DIVIDER_W / 2.0, 0.0),
                    NSSize::new(DIVIDER_W, fh),
                ));
            }
            // The band the toolbar's items sit on spans the terminal, and its height is the
            // system's — see `band_h`, which reads it back off the window rather than assume it.
            let hb = self.host.bounds();
            let band = self.band_h();
            self.header.setFrame(NSRect::new(
                NSPoint::new(0.0, hb.size.height - band),
                NSSize::new(hb.size.width, band),
            ));
            // Top strip, arranged the way the system's sidebar apps do it: traffic lights at the
            // sidebar's top-left, its collapse button at the far end of the same strip, and the
            // terminal's title at the terminal's own left edge.
            //
            // The two collapse buttons are deliberately separate. Expanded, the card carries its
            // own (`toggle.rs`) and it rides the card: parked with it, it slides off screen instead
            // of blinking away. Collapsed, there is no card to carry anything, so the toolbar's
            // button (`toolbar.rs`) takes over, immediately left of the title.
            let (tw, th) = (TOGGLE_W + 12.0, TOGGLE_W);
            let anchor = if self.collapsed.get() { parked } else { card };
            // Search sits immediately left of the collapse button, the pair anchored to the end of
            // the strip furthest from the traffic lights. Docked right, that end is the card's left
            // one, so the pair mirrors onto it — and their order mirrors with it, keeping collapse
            // outermost and search next to the list it filters.
            let (sx, tx) = if right {
                (anchor.origin.x + 8.0 + tw, anchor.origin.x + 8.0)
            } else {
                (anchor.origin.x + w - 2.0 * tw - 8.0, anchor.origin.x + w - tw - 8.0)
            };
            let cy = self.titlebar_center_y();
            // Snapped to whole points. The buttons' content is a hairline symbol, and `draw_symbol`
            // can only align it within the view's own coordinates — a frame at a half point moves
            // the whole grid with it and every stroke goes back to straddling two pixels.
            let y = (fh - cy - th / 2.0).round();
            for (btn, x) in [(&self.search_btn, sx), (&self.toggle_btn, tx)] {
                self.set_frame_maybe_animated(
                    btn,
                    NSRect::new(NSPoint::new(x.round(), y), NSSize::new(tw, th)),
                );
            }
            self.toolbar.set_shown(self.collapsed.get());
            let title_inset = if self.collapsed.get() {
                // The toolbar's button now sits between the lights and the title.
                TOOLBAR_ITEMS_RIGHT_X + 12.0
            } else if right {
                // Sidebar docked right: the terminal is on the left, so the traffic lights sit at
                // the window's top-left over the header — the title must clear them.
                self.lights_right_x() + 14.0
            } else {
                TITLE_INSET
            };
            self.header.set_left_inset(title_inset);
            self.header.set_center_y(cy);
            self.card.setNeedsDisplay(true);
            self.sidebar.setNeedsDisplay(true);
            self.search_btn.setNeedsDisplay(true);
            self.toggle_btn.setNeedsDisplay(true);
            self.header.setNeedsDisplay(true);
        }
        // The lights share the toolbar's centerline and shift with the card, so any layout change
        // can invalidate their position.
        self.sync_top_strip();
    }

    /// The window delegate's hook for every event that relays the title-bar band out: a resize, the
    /// window becoming key, its first exposure. AppKit puts the traffic lights back at its own
    /// coordinates on each of those, so they have to be re-placed after it.
    pub fn sync_top_strip(&self) {
        self.reposition_traffic_lights();
    }

    /// Put the window where it was left, or center it when there is nothing to restore.
    ///
    /// Called from `main` once, in place of the `center()` it used to do unconditionally. macOS's
    /// own window restoration is deliberately off (`setRestorable: false` — it would override the
    /// contentRect we just set), so without this the window is re-centered on every launch, and
    /// "centered" means *the screen that happens to be active*, which on a multi-display setup is
    /// not reliably the one it was on.
    ///
    /// A restored position is only trusted if the window lands on a screen that still exists:
    /// displays get unplugged, and a frame that intersects nothing would put the window somewhere
    /// the user cannot reach it. `NSWindow::screen` is the check — it answers None for a window on
    /// no screen at all.
    pub fn place_window(&self) {
        let Some(origin) = self.saved_origin.get() else {
            self.window.center();
            return;
        };
        unsafe { self.window.setFrameOrigin(origin) };
        if self.window.screen().is_none() {
            self.window.center();
        }
    }

    /// Height of the title-bar band, read back off the window: with a toolbar attached this is the
    /// system's, not ours, and everything below it (the terminal, the empty-state placeholder) has
    /// to start under it or the toolbar's items would sit over the first row of output.
    ///
    /// `contentLayoutRect` is the content view minus that band. It falls back to [`HEADER_H`] while
    /// the window has no content view yet, and is clamped because a full-screen window reports no
    /// band at all — the terminal would then run under the notch/menu bar on the way in.
    pub fn band_h(&self) -> f64 {
        let Some(cv) = self.window.contentView() else { return HEADER_H };
        let band = cv.bounds().size.height - unsafe { self.window.contentLayoutRect() }.size.height;
        if band.is_finite() && (8.0..=200.0).contains(&band) {
            band
        } else {
            HEADER_H
        }
    }

    /// The title bar always shows the current active tab's name; falls back to "TabT" when there is no active tab.
    fn update_title(&self) {
        let title = {
            let m = self.model.borrow();
            m.active
                .and_then(|a| m.tabs.iter().find(|t| t.id == a))
                .map(Tab::display_title)
                .unwrap_or_else(|| crate::branding::APP_NAME.to_string())
        };
        // Bail out when nothing changed. This is now on the path of every OSC report, so a shell
        // that retitles at each prompt — or a `cd`, which renames an underived tab — would
        // otherwise call `setTitle:` continuously, and each call relays out the title bar.
        if *self.last_window_title.borrow() == title {
            return;
        }
        *self.last_window_title.borrow_mut() = title.clone();
        self.window.setTitle(&NSString::from_str(&title));
        // `setTitle:` relays out the title bar and puts the traffic lights back where AppKit wants
        // them, synchronously and right here. Re-center them before returning: the correction then
        // lands in the same run loop turn, ahead of the window's flush, so nothing is ever drawn
        // with the buttons in the default spot. Deferring it — as this used to — showed the jump.
        self.reposition_traffic_lights();
        self.header.set_title(&title);
    }

    /// The header's dimmed second line: where the active session is and what it is running, or
    /// that it has ended.
    ///
    /// Deliberately not part of `update_title`. This changes on every `cd`, and `setTitle:` relays
    /// out the title bar and resets the traffic lights each time it is called — the header has to
    /// be updatable without paying that.
    fn update_header(&self) {
        let m = self.model.borrow();
        let meta = match m.active.and_then(|a| m.tabs.iter().find(|t| t.id == a)) {
            Some(t) if t.state == SessionState::Ended => "session ended".to_string(),
            Some(t) => {
                let shell = t.shell.rsplit('/').next().unwrap_or(&t.shell);
                format!("{} · {}", home_relative(&t.cwd()), shell)
            }
            None => String::new(),
        };
        drop(m);
        self.header.set_meta(&meta);
    }

    /// Load the layout from ~/.tabt and spawn a new shell for each tab.
    pub fn bootstrap(&self) {
        let layout = config::load();
        let cfg = &layout.settings;
        // Apply the color theme + font (both must be set before spawning tabs and computing cols/rows).
        let idx = theme::index_of(&cfg.style);
        self.style.set(idx);
        theme::set(theme::by_index(idx));
        self.sync_window_chrome();
        settings::set(&cfg.font_family, cfg.font_size);
        // The sidebar width/position must be set before spawning tabs and computing host dimensions.
        self.sidebar_w.set(cfg.sidebar_w.clamp(MIN_SIDEBAR_W, MAX_SIDEBAR_W));
        self.sidebar_right.set(cfg.sidebar_right);
        // Terminal/shell preferences: padding feeds `dims()` below, and scrollback/shell are read
        // by each `spawn_tab`, so all of it has to be in place before the tabs are restored.
        settings::set_cursor_shape(cfg.cursor_shape);
        settings::set_cursor_blink(cfg.cursor_blink);
        settings::set_scrollback(cfg.scrollback);
        settings::set_shell(&cfg.shell);
        settings::set_new_tab_dir(cfg.new_tab_dir);
        settings::set_pad(cfg.padding);
        settings::set_opacity(cfg.opacity);
        settings::set_toolbar_hidden(cfg.toolbar_hidden.clone());
        settings::set_toolbar_order(cfg.toolbar_order.clone());
        self.toolbar.rebuild();
        self.sync_blink_timer();
        // Restore the saved window size (clamped to a sane range) before laying out / spawning
        // tabs. The upper bound guards against a corrupted config producing an unusable
        // off-screen window; it's a generous cap, not a real display-size limit.
        if cfg.window_w > 0.0 && cfg.window_h > 0.0 {
            let sz = NSSize::new(cfg.window_w.clamp(480.0, 6000.0), cfg.window_h.clamp(320.0, 4000.0));
            self.window.setContentSize(sz);
        }
        // The position is applied by `place_window`, from `main`, after the menus are built — a
        // `center()` there would otherwise undo whatever this put down.
        self.saved_origin.set(match (cfg.window_x, cfg.window_y) {
            (Some(x), Some(y)) => Some(NSPoint::new(x, y)),
            _ => None,
        });
        self.relayout();
        // Spawn ungrouped tabs first (rendered at the top), then each group. A tab that fails to
        // spawn (e.g. the system is out of file descriptors) is silently skipped — restore
        // whatever we can rather than aborting the whole session restore.
        // `auto` says the stored title was derived and may be re-derived; its absence means the
        // user chose it. A config written before that key existed therefore restores pinned, which
        // is what keeps every rename made by an older version.
        for t in layout.ungrouped {
            let _ = self.spawn_tab(None, t.title, !t.auto, &t.cwd, t.dot, t.locked);
        }
        for (name, collapsed, tabs) in layout.groups {
            let gi = {
                let mut m = self.model.borrow_mut();
                m.groups.push(Group { name, collapsed, tabs: Vec::new() });
                m.groups.len() - 1
            };
            for t in tabs {
                let _ = self.spawn_tab(Some(gi), t.title, !t.auto, &t.cwd, t.dot, t.locked);
            }
        }
        let first = self.model.borrow().tabs.first().map(|t| t.id);
        match first {
            Some(id) => self.select(id),
            // Every saved tab failed to spawn (e.g. the system is out of file descriptors at
            // launch) — show the empty-state placeholder instead of a blank host view.
            None => self.show_placeholder(),
        }
        self.save(); // persist once, ensuring ~/.tabt exists and reflects the current layout
        self.refresh_sidebar();
        self.start_state_timer();
    }

    /// Current window content size (width, height), persisted so the next launch reopens at the same size.
    fn window_size(&self) -> (f64, f64) {
        let s = self
            .window
            .contentView()
            .map(|c| c.frame().size)
            .unwrap_or_else(|| self.window.frame().size);
        (s.width, s.height)
    }

    /// Compute cols/rows from the terminal-view area (host minus the header bar) and font metrics.
    /// Must match `TermView::on_resize`'s formula, otherwise a font change (which reflows via this
    /// instead of a view resize) would set a row count that doesn't fit the visible area.
    fn dims(&self) -> (usize, usize) {
        let b = self.host.bounds();
        let w = b.size.width - 2.0 * settings::pad();
        let h = b.size.height - self.band_h() - 2.0 * settings::pad();
        let cols = ((w / settings::cell_w()).floor() as i64).max(1) as usize;
        let rows = ((h / settings::line_h()).floor() as i64).max(1) as usize;
        (cols, rows)
    }

    /// Create a new tab (without switching to it): spawn a PTY + TermView + reader in `cwd`
    /// (for session restore, may be empty). When `group` is None it goes into the ungrouped list. Registered into the model.
    /// Returns `None` if the PTY/process itself couldn't be spawned (e.g. out of file
    /// descriptors) — the caller must skip creating this one tab without disturbing any others.
    fn spawn_tab(&self, group: Option<usize>, title: String, pinned: bool, cwd: &str, dot: u8, locked: bool) -> Option<u64> {
        let id = {
            let mut m = self.model.borrow_mut();
            let id = m.next_id;
            m.next_id += 1;
            id
        };
        let (cols, rows) = self.dims();
        let (fd, shell_pid, shell) = pty::spawn(cols as u16, rows as u16, cwd)?;
        let frame = self.host.bounds();
        let v = TermView::new(self.mtm, frame, fd, cols, rows);
        v.set_scrollback(settings::scrollback());
        v.attach(self as *const AppController as *const c_void, id, end_cb, restart_cb, toggle_cb, meta_cb);
        let reader = view::attach_reader(&v);

        let mut m = self.model.borrow_mut();
        m.tabs.push(Tab {
            id,
            title,
            pinned,
            osc_title: String::new(),
            dot,
            locked,
            view: v,
            master_fd: fd,
            shell_pid,
            reader,
            // Assume idle until the first sample: a tab that has only just spawned is at a prompt,
            // and guessing Running would flash every new tab.
            state: SessionState::Idle,
            activity: false,
            bell: false,
            last_seq: 0,
            primed: false,
            shell,
            // Record where the shell actually starts, not what was requested: `pty::spawn` falls
            // back to HOME on an empty cwd, and leaving that blank here would leave a fresh tab
            // with no directory to be named after or revealed in Finder until OSC 7 reports one —
            // which a stock zsh never does.
            spawn_cwd: if cwd.is_empty() { std::env::var("HOME").unwrap_or_default() } else { cwd.to_string() },
        });
        match group {
            Some(gi) if gi < m.groups.len() => m.groups[gi].tabs.push(id),
            _ => m.ungrouped.push(id),
        }
        Some(id)
    }

    pub fn select(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            if !m.tabs.iter().any(|t| t.id == id) {
                return;
            }
            if m.active != Some(id) {
                self.prev_active.set(m.active);
            }
            m.active = Some(id);
            // Looking at a session is what "seen" means, so its marks clear here. `last_seq` is
            // resynced at the same time: without that, output produced while it was in the
            // background would re-mark it on the very next sample.
            if let Some(t) = m.tabs.iter_mut().find(|t| t.id == id) {
                t.activity = false;
                t.bell = false;
                t.last_seq = t.view.output_seq();
                t.primed = true; // selecting is a reading too, so the sampler must not re-baseline
            }
            // A collapsed group hides its tabs, so the tab just made active would have no row in
            // the sidebar and keystrokes would go to a terminal nothing marks as selected. The rule
            // lives here rather than at the callers because every path that activates a tab — new
            // tab, sidebar click, restoring the persisted selection — goes through this one.
            if let Some(g) = m.groups.iter_mut().find(|g| g.tabs.contains(&id)) {
                g.collapsed = false;
            }
        }
        self.layout_active();
        self.refresh_sidebar();
        self.update_title();
        self.update_header();
    }

    /// ⌘~: back to the session you were in before this one, and pressing it again returns — because
    /// `select` records the outgoing tab as it goes, so the pair swaps each time.
    ///
    /// Does nothing when there is nothing to go back to, or when that tab has since been closed;
    /// silently, since the shortcut is a convenience and an alert for "you have only opened one
    /// session" would be worse than the no-op.
    pub fn select_recent_tab(&self) {
        let Some(prev) = self.prev_active.get() else { return };
        if self.model.borrow().tabs.iter().any(|t| t.id == prev) {
            self.select(prev);
        }
    }

    /// Click the already-selected tab → deselect it; any other tab → select it.
    pub fn toggle_select(&self, id: u64) {
        if self.model.borrow().active == Some(id) {
            self.deselect();
        } else {
            self.select(id);
        }
    }

    /// Detach the active tab from the terminal area, leaving the empty-state placeholder.
    /// The session keeps running (its PTY is untouched and its reader keeps feeding the grid);
    /// selecting the tab again re-mounts the view with the output it produced meanwhile.
    pub fn deselect(&self) {
        {
            let mut m = self.model.borrow_mut();
            if m.active.is_none() {
                return;
            }
            m.active = None;
            // layout_active() early-returns without an active tab, so unmount the views here.
            for t in &m.tabs {
                unsafe { t.view.removeFromSuperview() };
            }
        }
        self.show_placeholder();
        // Hand first responder back to the window: with no terminal mounted there is nothing to
        // type into, and a detached TermView must not keep receiving keyDown.
        self.window.makeFirstResponder(None);
        self.refresh_sidebar();
        self.update_title(); // also re-centers the traffic lights, which setTitle: resets
        self.update_header(); // no active session: the meta line empties too
    }

    /// Mount the active tab's view into the host (remove the others), and make it the keyboard first responder.
    fn layout_active(&self) {
        let m = self.model.borrow();
        let active = match m.active {
            Some(a) => a,
            None => return,
        };
        // Terminal fills the host below the title-bar band (host is non-flipped: y=0 is the bottom).
        let hb = self.host.bounds();
        let bounds = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(hb.size.width, (hb.size.height - self.band_h()).max(0.0)),
        );
        unsafe { self.placeholder.removeFromSuperview() }; // hide the empty-state view when a session is active
        for t in &m.tabs {
            unsafe { t.view.removeFromSuperview() };
        }
        if let Some(tab) = m.tabs.iter().find(|t| t.id == active) {
            unsafe {
                tab.view.setFrame(bounds); // triggers setFrameSize -> grid reflow + TIOCSWINSZ
                tab.view.setAutoresizingMask(
                    NSAutoresizingMaskOptions::NSViewWidthSizable
                        | NSAutoresizingMaskOptions::NSViewHeightSizable,
                );
                self.host.addSubview(&tab.view);
                self.window.makeFirstResponder(Some(&tab.view));
                tab.view.setNeedsDisplay(true);
            }
        }
    }

    /// Mount the empty-state placeholder in the terminal area (no sessions left).
    fn show_placeholder(&self) {
        let hb = self.host.bounds();
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(hb.size.width, (hb.size.height - self.band_h()).max(0.0)),
        );
        unsafe {
            self.placeholder.setFrame(frame);
            self.host.addSubview(&self.placeholder);
            self.placeholder.setNeedsDisplay(true);
        }
    }

    /// New tab. When a tab is selected, the new tab inherits its working directory and is inserted
    /// immediately after it within the same group/list. With no selection, it lands in the ungrouped
    /// list (rendered at the top) in the home directory; it can be dragged into a group when needed.
    pub fn add_tab_default(&self) {
        // Snapshot the selected tab's cwd + group under a single borrow, before spawning.
        let anchor = {
            let m = self.model.borrow();
            m.active.and_then(|a| {
                let cwd = m.tabs.iter().find(|t| t.id == a).map(Tab::cwd)?;
                // The new tab stays in the active tab's group even when that group is collapsed —
                // it is expanded below, so the tab is never created hidden.
                let group = m.groups.iter().position(|g| g.tabs.contains(&a));
                Some((a, group, cwd))
            })
        };
        let (group, cwd) = match &anchor {
            Some((_, g, cwd)) => (*g, self.new_tab_cwd(cwd.clone())),
            None => (None, String::new()),
        };
        match self.spawn_tab(group, String::new(), false, &cwd, 0, false) {
            Some(id) => {
                if let Some((active_id, _, _)) = anchor {
                    self.place_tab_after(group, id, active_id);
                }
                self.select(id); // expands the tab's group if it was collapsed
                self.save();
                self.refresh_sidebar();
            }
            None => self.alert_spawn_failed(),
        }
    }

    /// Create a new terminal inside group `gi` (from the group row's "New Terminal" menu item).
    /// Inherits the active tab's cwd when there is one, otherwise the default shell directory.
    /// `spawn_tab` appends the new tab to the group's end.
    pub fn add_tab_in_group(&self, gi: usize) {
        if gi >= self.model.borrow().groups.len() {
            return;
        }
        let cwd = {
            let m = self.model.borrow();
            m.active
                .and_then(|a| {
                    m.tabs.iter().find(|t| t.id == a).map(Tab::cwd)
                })
                .unwrap_or_default()
        };
        let cwd = self.new_tab_cwd(cwd);
        match self.spawn_tab(Some(gi), String::new(), false, &cwd, 0, false) {
            Some(id) => {
                self.select(id); // expands `gi` if it was collapsed, so the new row is visible
                self.save();
                self.refresh_sidebar();
            }
            None => self.alert_spawn_failed(),
        }
    }

    /// Move `id` to sit immediately after `anchor` within `group`'s ordered list (both must already be
    /// in that same list — `spawn_tab` appended `id` to its end). Keeps a new tab next to its origin.
    fn place_tab_after(&self, group: Option<usize>, id: u64, anchor: u64) {
        let mut m = self.model.borrow_mut();
        let list: &mut Vec<u64> = match group {
            Some(gi) if gi < m.groups.len() => &mut m.groups[gi].tabs,
            _ => &mut m.ungrouped,
        };
        if let Some(p) = list.iter().position(|&t| t == id) {
            list.remove(p);
        }
        let pos = list
            .iter()
            .position(|&t| t == anchor)
            .map(|p| p + 1)
            .unwrap_or(list.len());
        list.insert(pos, id);
    }

    /// Shown when a new tab's PTY/process couldn't be created (e.g. out of file descriptors).
    /// Existing tabs are unaffected; this only ever fails the one new tab.
    fn alert_spawn_failed(&self) {
        let alert = unsafe { NSAlert::new(self.mtm) };
        unsafe {
            alert.setMessageText(&NSString::from_str("Couldn’t open a new terminal"));
            // Two causes now: the configured shell (much the likelier one, and the user's to fix)
            // and the system refusing a new PTY. Name the shell when there is one to name.
            let shell = settings::shell();
            let text = if shell.is_empty() {
                "The system refused to create a new session (it may be low on resources). Your other tabs are unaffected.".to_string()
            } else {
                format!(
                    "“{shell}” could not be run — check Settings → Shell. Your other tabs are unaffected."
                )
            };
            alert.setInformativeText(&NSString::from_str(&text));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
    }

    pub fn add_group_default(&self) {
        let n = self.model.borrow().groups.len() + 1;
        self.model.borrow_mut().groups.push(Group { name: format!("Group {}", n), collapsed: false, tabs: Vec::new() });
        self.save();
        self.refresh_sidebar();
        // The new group is appended at the end of the list; scroll the sidebar to the bottom to make it visible.
        self.sidebar.scroll_to_bottom();
    }

    /// Drag to move a tab: move to `to_group`, inserting before tab `before` (None = append to the end).
    /// Supports both in-group reordering and cross-group moves.
    /// Drag to move a tab: target `to` is Some(group index) or None (ungrouped list), inserting before tab
    /// `before` (None = append). Supports same-group / cross-group / in-and-out of the ungrouped area.
    pub fn move_tab_to(&self, id: u64, to: Option<usize>, before: Option<u64>) {
        {
            let mut m = self.model.borrow_mut();
            if let Some(gi) = to {
                if gi >= m.groups.len() {
                    return;
                }
            }
            // First remove from the ungrouped list + all groups, then locate the insertion point by `before`.
            m.ungrouped.retain(|&t| t != id);
            for g in &mut m.groups {
                g.tabs.retain(|&t| t != id);
            }
            let list = match to {
                Some(gi) => &mut m.groups[gi].tabs,
                None => &mut m.ungrouped,
            };
            let pos = match before {
                Some(bid) => list.iter().position(|&t| t == bid).unwrap_or(list.len()),
                None => list.len(),
            };
            list.insert(pos, id);
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Drag to reorder groups: move group index `from` to insertion position `target` (0..=len, meaning
    /// "insert before the target-th element of the original array", so when target>from subtract 1 first to offset the removal).
    pub fn move_group(&self, from: usize, mut target: usize) {
        {
            let mut m = self.model.borrow_mut();
            if from >= m.groups.len() {
                return;
            }
            if target > from {
                target -= 1;
            }
            let g = m.groups.remove(from);
            let target = target.min(m.groups.len());
            m.groups.insert(target, g);
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Collapse/expand a group (the sidebar hides/shows its tabs).
    pub fn toggle_group_collapsed(&self, gi: usize) {
        {
            let mut m = self.model.borrow_mut();
            match m.groups.get_mut(gi) {
                Some(g) => g.collapsed = !g.collapsed,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Delete an entire group. If the group contains tabs, show a confirmation dialog first.
    pub fn delete_group(&self, gi: usize) {
        let (name, ids): (String, Vec<u64>) = match self.model.borrow().groups.get(gi) {
            Some(g) => (g.name.clone(), g.tabs.clone()),
            None => return,
        };
        if !ids.is_empty() && !self.confirm_delete_group(&name, ids.len()) {
            return;
        }
        // Remove the group entry first: otherwise, when closing the last tab triggers terminate->persist, the empty group
        // would be written back to the config and "revived" on the next launch.
        {
            let mut m = self.model.borrow_mut();
            if gi < m.groups.len() {
                m.groups.remove(gi);
            }
        }
        // Tear down each tab one by one (without persisting per tab). Deleting a group is an
        // explicit user action, so if it empties the app out entirely, stay open with the
        // empty-state placeholder rather than quitting (matches closing the last tab).
        let closed_active = ids.iter().any(|id| self.model.borrow().active == Some(*id));
        for id in &ids {
            self.teardown_tab(*id);
        }
        if self.model.borrow().tabs.is_empty() {
            self.went_empty();
            return;
        }
        if closed_active {
            // Bound to a local first: a temporary `Ref` in an `if let` scrutinee lives until the
            // end of the whole `if let`, body included, so borrowing inline here would still hold
            // the model when `select` takes it mutably — a BorrowMutError, and under
            // `panic = "abort"` that is the whole app going down.
            let next = self.model.borrow().tabs.first().map(|t| t.id);
            if let Some(a) = next {
                self.select(a);
            }
        }
        self.save(); // persist only once for the whole deletion
        self.refresh_sidebar();
    }

    /// Confirmation dialog before deleting a non-empty group. Returns true = confirm deletion.
    fn confirm_delete_group(&self, name: &str, count: usize) -> bool {
        confirm(
            self.mtm,
            &format!("Delete group “{}”?", name),
            &format!("This will close {} terminal(s) in this group. This cannot be undone.", count),
            "Delete",
        )
    }

    /// Remove from the model and release a tab's PTY/view/reader (no persist, no reselect, no terminate).
    /// Returns whether the tab actually existed. If the removed tab is active, clear `active` and leave the caller to reselect.
    fn teardown_tab(&self, id: u64) -> bool {
        // First pull the TabData out of the model (we are now on a new runloop tick, out of on_readable).
        let removed = {
            let mut m = self.model.borrow_mut();
            let idx = match m.tabs.iter().position(|t| t.id == id) {
                Some(i) => i,
                None => return false,
            };
            m.ungrouped.retain(|&t| t != id);
            for g in &mut m.groups {
                g.tabs.retain(|&t| t != id);
            }
            if m.active == Some(id) {
                m.active = None;
            }
            m.tabs.remove(idx)
        };
        view::cancel_reader(&removed.reader);
        unsafe {
            removed.view.removeFromSuperview();
            libc::close(removed.master_fd);
        }
        drop(removed); // Retained<TermView> is released here, safely
        true
    }

    /// The tab adjacent to `id` in visual order (ungrouped first, followed by each group): prefer the
    /// preceding one, falling back to the next when closing the very first tab. Used to move focus to a
    /// neighbor after closing the active tab.
    fn adjacent_tab(&self, id: u64) -> Option<u64> {
        let m = self.model.borrow();
        let mut order: Vec<u64> = m.ungrouped.clone();
        for g in &m.groups {
            order.extend(g.tabs.iter().copied());
        }
        let pos = order.iter().position(|&t| t == id)?;
        pos.checked_sub(1)
            .and_then(|p| order.get(p).copied())
            .or_else(|| order.get(pos + 1).copied())
    }

    /// The shell exited on its own (EOF/read error): tear down the now-dead reader/fd but keep the
    /// tab and its view — TermView shows a "session ended" placeholder (see `view::TermView::restart`)
    /// until the user restarts it (Enter) or closes the tab explicitly (⌘W).
    fn end_tab_session(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            let Some(t) = m.tabs.iter_mut().find(|t| t.id == id) else {
                return;
            };
            view::cancel_reader(&t.reader);
            unsafe { libc::close(t.master_fd) };
            t.master_fd = -1;
            t.shell_pid = -1; // tcgetpgrp(-1) fails, so has_foreground_job reports false
            t.state = SessionState::Ended;
            // The view keeps its own copy of both facts, and this is the one path that can reach
            // here without it already knowing: a shell that exits on its own comes through
            // `TermView::mark_ended`, which sets the flag before calling this, but `restart_active_tab`
            // hangs a live shell up and calls this directly. Idempotent, so the first case is a
            // no-op rather than a second status line (see `TermView::end_session`).
            t.view.end_session();
        }
        // Marked here rather than left to the next sample: a session that just died should not go
        // on looking alive for the best part of a second. The borrow above is scoped for the same
        // reason `sample_states` scopes its own — `refresh_sidebar` takes the model again.
        //
        // The header has to be told here too. Setting the state directly is exactly what stops the
        // sampler noticing a change, so leaving it to that would leave the meta line stale forever.
        self.refresh_sidebar();
        self.update_header();
    }

    /// Respawn a fresh shell into a tab whose previous one already ended (see `end_tab_session`),
    /// reusing its last known working directory. Fired when the user presses Enter on an ended tab.
    pub fn restart_tab(&self, id: u64) {
        let cwd = match self.model.borrow().tabs.iter().find(|t| t.id == id) {
            Some(t) => t.cwd(),
            None => return,
        };
        let (cols, rows) = self.dims();
        let (fd, shell_pid, shell) = match pty::spawn(cols as u16, rows as u16, &cwd) {
            Some(v) => v,
            None => return, // out of fds/process table etc.: leave the tab ended, nothing else to do
        };
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.master_fd = fd;
                    t.shell_pid = shell_pid;
                    t.shell = shell; // a restart re-resolves it, so the setting can take effect here
                    t.reader = view::attach_reader(&t.view);
                    // The directory the new shell was actually spawned in becomes the fallback the
                    // tab reports until OSC 7 says otherwise — which, with a stock zsh, is never
                    // (`/etc/zshrc` only sources that hook for Apple_Terminal). `TermView::restart`
                    // installs a fresh Grid one line below, wiping the cwd the old shell had
                    // reported, so without this the tab would go on naming the directory it was
                    // *first* opened in: the row and window title would rename themselves back to
                    // it, Reveal in Finder would open it, and the next save would persist it.
                    t.spawn_cwd = cwd.clone();
                    t.view.restart(fd, cols, rows);
                    t.state = SessionState::Idle; // a fresh shell comes up at its prompt
                }
                None => {
                    unsafe { libc::close(fd) }; // tab closed meanwhile: don't leak the new pty
                    return;
                }
            }
        }
        self.refresh_sidebar(); // clear the "ended" mark now, not on the next sample
        self.update_header();
    }

    /// Close a tab the user explicitly asked to close (⌘W, or "Close" in the tab's "⋯" menu).
    /// If it was the last tab, the app stays open with the empty-state placeholder instead of quitting.
    /// If the shell has a foreground job (not just an idle prompt), confirm first — closing tears
    /// down the PTY, which kills that job with no chance to save work.
    pub fn close_tab_user(&self, id: u64) {
        // A locked tab is protected from user-initiated close: silently ignore. The user must
        // unlock it first (via the tab menu). The shell exiting on its own still closes it.
        if self.is_tab_locked(id) {
            return;
        }
        if self.tab_has_foreground_job(id) && !confirm(self.mtm, "Close this tab?", RUNNING_JOB_WARNING, "Close") {
            return;
        }
        self.close_tab_impl(id);
    }

    /// ⌘Q (via the app delegate's `applicationShouldTerminate:`): if any open tab has a
    /// foreground job running, confirm once before quitting rather than silently killing every
    /// session. Returns whether termination should proceed.
    pub fn confirm_quit(&self) -> bool {
        if !self.model.borrow().tabs.iter().any(|t| pty::has_foreground_job(t.master_fd, t.shell_pid)) {
            return true;
        }
        confirm(
            self.mtm,
            &format!("Quit {}?", crate::branding::APP_NAME),
            "One or more terminals still have a process running. Quitting will terminate all of them.",
            "Quit",
        )
    }

    /// Whether the given tab's shell currently has a foreground job running (see `pty::has_foreground_job`).
    fn tab_has_foreground_job(&self, id: u64) -> bool {
        self.model
            .borrow()
            .tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| pty::has_foreground_job(t.master_fd, t.shell_pid))
            .unwrap_or(false)
    }

    /// The model just dropped to zero tabs from an explicit user action (closing the last tab,
    /// deleting the last group). Keep the app open with the empty-state placeholder rather than
    /// quitting — only a further close/delete with nothing at all left should quit.
    fn went_empty(&self) {
        self.show_placeholder();
        self.update_title();
        // Paired with the title, for the reason `deselect` pairs them: `teardown_tab` has already
        // cleared `active`, so the meta line resolves to empty — but only if someone asks it to.
        // Without this the last session's directory and shell stay under the placeholder, naming a
        // tab that is gone.
        self.update_header();
        self.refresh_sidebar();
        self.save();
    }

    fn close_tab_impl(&self, id: u64) {
        let was_active = self.model.borrow().active == Some(id);
        // The neighbor tab must be computed before teardown (at this point id is still in visual order).
        let neighbor = if was_active { self.adjacent_tab(id) } else { None };
        if !self.teardown_tab(id) {
            return;
        }
        // Last tab closed: stay open with the empty-state placeholder rather than quitting.
        if self.model.borrow().tabs.is_empty() {
            self.went_empty();
            return;
        }
        if was_active {
            let pick = neighbor
                .filter(|nid| self.model.borrow().tabs.iter().any(|t| t.id == *nid))
                .or_else(|| self.model.borrow().tabs.first().map(|t| t.id));
            if let Some(a) = pick {
                self.select(a);
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// The shell reported a new title or cwd for this tab (see `view::MetaFn`). Both feed the
    /// sidebar row, so either is worth a refresh; the callback only fires on an actual change.
    pub fn on_tab_meta(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            let Some(t) = m.tabs.iter_mut().find(|t| t.id == id) else {
                return;
            };
            // Sanitized here, at the boundary: past this point the title is stored, drawn, and
            // written to layout.conf, and it arrived from whatever is on the far end of the PTY.
            t.osc_title = config::sanitize_label(&t.view.title());
        }
        self.refresh_sidebar();
        self.update_title(); // the active tab's name may have just changed under the window title
        self.update_header(); // a plain `cd` reaches here and changes nothing else
    }

    /// Rename a tab (committed after double-click in-place editing in the sidebar).
    pub fn rename_tab(&self, id: u64, name: String) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.title = name;
                    t.pinned = true; // an explicit rename outranks whatever the shell reports
                }
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
        self.update_title(); // when collapsed, the renamed tab may be the active one
    }

    /// Toggle a tab's locked state. A locked tab is protected from user-initiated close (⌘W / the
    /// tab menu's Close); the shell exiting on its own still closes it (see `close_tab`).
    pub fn toggle_tab_lock(&self, id: u64) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t.locked = !t.locked,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    // Single-field lookups for the sidebar's menus and its rename box. They exist so those paths
    // don't build a whole `Snapshot` — which clones every title in the window — to read one bool.
    // `snapshot()` is for drawing, hit testing and dragging, where the whole model is wanted.

    /// Whether there is a session for a session action to act on. False with every tab closed (the
    /// empty-state placeholder), which is what the toolbar validates against: every one of those
    /// actions opens by resolving `active` and returning if there is none, so without this they
    /// would draw enabled and do nothing.
    pub fn has_active_session(&self) -> bool {
        self.model.borrow().active.is_some()
    }

    /// Whether the given tab is locked (protected from user-initiated close).
    pub fn is_tab_locked(&self, id: u64) -> bool {
        self.model.borrow().tabs.iter().find(|t| t.id == id).map(|t| t.locked).unwrap_or(false)
    }

    /// A tab's current status-dot color index (0 = default/auto).
    pub fn tab_dot(&self, id: u64) -> u8 {
        self.model.borrow().tabs.iter().find(|t| t.id == id).map(|t| t.dot).unwrap_or(0)
    }

    /// A tab's current title, as the rename box should seed itself with.
    pub fn tab_title(&self, id: u64) -> String {
        self.model.borrow().tabs.iter().find(|t| t.id == id).map(Tab::display_title).unwrap_or_default()
    }

    /// Whether a group is collapsed (its tabs hidden in the sidebar).
    pub fn group_collapsed(&self, gi: usize) -> bool {
        self.model.borrow().groups.get(gi).map(|g| g.collapsed).unwrap_or(false)
    }

    /// A group's current name, as the rename box should seed itself with.
    pub fn group_name(&self, gi: usize) -> String {
        self.model.borrow().groups.get(gi).map(|g| g.name.clone()).unwrap_or_default()
    }

    /// Set a tab's color (index into sidebar::dot_colors; 0 = default/auto).
    pub fn set_tab_dot(&self, id: u64, dot: u8) {
        {
            let mut m = self.model.borrow_mut();
            match m.tabs.iter_mut().find(|t| t.id == id) {
                Some(t) => t.dot = dot,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    /// Rename a group.
    pub fn rename_group(&self, gi: usize, name: String) {
        {
            let mut m = self.model.borrow_mut();
            match m.groups.get_mut(gi) {
                Some(g) => g.name = name,
                None => return,
            }
        }
        self.save();
        self.refresh_sidebar();
    }

    pub fn snapshot(&self) -> Snapshot {
        let m = self.model.borrow();
        let snap_of = |id: &u64| {
            m.tabs
                .iter()
                .find(|t| t.id == *id)
                .map(|t| TabSnap {
                    id: t.id,
                    title: t.display_title(),
                    dot: t.dot,
                    locked: t.locked,
                    state: t.state,
                    activity: t.activity,
                    bell: t.bell,
                    cwd: t.cwd(),
                })
        };
        let ungrouped = m.ungrouped.iter().filter_map(snap_of).collect();
        let groups = m
            .groups
            .iter()
            .map(|g| {
                let tabs = g.tabs.iter().filter_map(snap_of).collect();
                GroupSnap { name: g.name.clone(), collapsed: g.collapsed, tabs }
            })
            .collect();
        Snapshot { ungrouped, groups, active: m.active, style: self.style.get() }
    }

    /// Switch the global color theme: immediately redraw the current terminal and sidebar, and persist.
    pub fn set_style(&self, idx: usize) {
        if idx >= theme::count() || idx == self.style.get() {
            return;
        }
        self.style.set(idx);
        theme::set(theme::by_index(idx));
        self.sync_window_chrome();
        // Only the active tab is present; the other tabs will be redrawn with the new theme the next time they are switched to.
        if let Some(a) = self.model.borrow().active {
            if let Some(tab) = self.model.borrow().tabs.iter().find(|t| t.id == a) {
                unsafe { tab.view.setNeedsDisplay(true) };
            }
        }
        unsafe { self.header.setNeedsDisplay(true) }; // header bg + title track the terminal color
        unsafe { self.placeholder.setNeedsDisplay(true) }; // empty-state colors track the theme too
        self.refresh_sidebar();
        self.save();
    }

    /// Adjust the global font size (delta is usually ±1).
    pub fn change_font_size(&self, delta: f64) {
        settings::set(&settings::family(), settings::size() + delta);
        self.reflow_all();
        self.save();
    }

    /// Settings → Terminal → Cursor: block, bar or underline. Only the drawing changes, so a
    /// redraw of the active tab is the whole of it.
    pub fn set_cursor_shape(&self, idx: usize) {
        settings::set_cursor_shape(settings::CursorShape::from_index(idx));
        self.redraw_active();
        self.save();
    }

    /// Settings → Terminal → Blink: start or stop the blink timer.
    pub fn set_cursor_blink(&self, on: bool) {
        settings::set_cursor_blink(on);
        self.sync_blink_timer();
        self.redraw_active();
        self.save();
    }

    /// Create the blink timer when blinking is on and it isn't running, cancel it when it is off.
    ///
    /// One timer for the whole app, owned here rather than per tab: only the active view is
    /// mounted, so that is the only one whose cursor is on screen, and a per-tab timer would have
    /// to be torn down on every close. The token lives as long as the controller, which outlives
    /// the run loop, so there is no teardown path that can leave it firing on freed state.
    /// Start the session-state sampler. Runs for the controller's whole life, so nothing has to
    /// tear it down — and nothing can leave it firing against a dropped controller, which is the
    /// failure the blink timer's start/stop dance has to be careful about.
    fn start_state_timer(&self) {
        extern "C" fn tick(ctx: *mut c_void) {
            let c: &AppController = unsafe { &*(ctx as *const AppController) };
            c.sample_states();
        }
        let ctx = self as *const AppController as *mut c_void;
        *self.state_timer.borrow_mut() = Some(view::attach_timer(SAMPLE_MS, ctx, tick));
    }

    /// Re-read every session's state, and redraw the sidebar only if one of them moved.
    ///
    /// The "only if" is the whole design. An unconditional redraw here would repaint the entire
    /// sidebar once a second forever, which is worse for the battery than the wrong status dot this
    /// exists to fix.
    fn sample_states(&self) {
        let changed = {
            let mut m = self.model.borrow_mut();
            let active = m.active;
            let mut changed = false;
            for t in &mut m.tabs {
                // Unseen output and an unseen bell, both only for a tab you are not looking at —
                // marking the tab in front of you would be noise, and `select` clears them anyway.
                let seq = t.view.output_seq();
                let background = active != Some(t.id);
                // Drained every sample whether or not it is used: the flag latches in the grid, so
                // leaving it unread would ring the next time this tab happened to go background.
                if t.view.take_bell() && background && !t.bell {
                    t.bell = true;
                    changed = true;
                }
                // An ended session costs nothing to classify: `end_tab_session` closes the fd and
                // sets shell_pid to -1, so there is no syscall left to make.
                let now = if t.master_fd < 0 {
                    SessionState::Ended
                } else if pty::has_foreground_job(t.master_fd, t.shell_pid) {
                    SessionState::Running
                } else {
                    SessionState::Idle
                };
                if t.state != now {
                    t.state = now;
                    changed = true;
                }
                // A session's own start-up counts for nothing, or a restored layout would mark
                // every tab in it as having unseen output — the whole sidebar, on every launch,
                // until each row had been clicked. So a tab is unprimed until it has *settled*:
                // seen at a prompt, having printed nothing for a whole sample. Baselining on the
                // first sample instead (what this did) is not enough — that sample lands 800ms in,
                // typically before a forked shell has finished its rc files, so it baselines the
                // silence and the prompt itself arrives as "new output" one tick later. Until then
                // `last_seq` still tracks, so the start-up chatter is swallowed rather than
                // arriving in a lump the moment the tab primes.
                let quiet = seq == t.last_seq;
                t.last_seq = seq;
                if !t.primed {
                    t.primed = quiet && now != SessionState::Running;
                } else if !quiet && background && !t.activity {
                    t.activity = true;
                    changed = true;
                }
            }
            changed
        }; // the model borrow ends here — refresh_sidebar borrows it again, and under
           // `panic = "abort"` a RefCell clash is the whole process, not one bad tab.
        if changed {
            self.refresh_sidebar();
            self.update_header(); // the active session may have just started or finished a job
        }
    }

    fn sync_blink_timer(&self) {
        let running = self.blink_timer.borrow().is_some();
        match (settings::cursor_blink(), running) {
            (true, false) => {
                extern "C" fn tick(ctx: *mut c_void) {
                    let c: &AppController = unsafe { &*(ctx as *const AppController) };
                    settings::toggle_cursor_phase();
                    c.redraw_active();
                }
                let ctx = self as *const AppController as *mut c_void;
                *self.blink_timer.borrow_mut() = Some(view::attach_timer(BLINK_MS, ctx, tick));
            }
            (false, true) => {
                if let Some(t) = self.blink_timer.borrow_mut().take() {
                    view::cancel_timer(&t);
                }
            }
            _ => {}
        }
    }

    /// Settings → Appearance → Opacity: the background alpha. Every surface that paints the theme
    /// background has to be repainted, and the window itself stops being opaque.
    pub fn set_opacity(&self, v: f64) {
        settings::set_opacity(v);
        self.sync_window_chrome(); // window color + `setOpaque:` + the card's layer colors
        self.redraw_active();
        unsafe { self.header.setNeedsDisplay(true) };
        unsafe { self.placeholder.setNeedsDisplay(true) };
        self.refresh_sidebar();
        self.save();
    }

    /// Settings → Terminal → Scrollback: apply the new depth to every open tab, not just new ones.
    /// Lowering it drops history immediately (`Grid::set_history_max`), which can move a
    /// scrolled-back viewport, so the tabs are redrawn.
    pub fn set_scrollback(&self, lines: usize) {
        settings::set_scrollback(lines);
        let m = self.model.borrow();
        for tab in &m.tabs {
            tab.view.set_scrollback(lines);
            unsafe { tab.view.setNeedsDisplay(true) };
        }
        drop(m);
        self.save();
    }

    /// Settings → Shell → Shell: the program each *new* tab execs. Running shells are left alone —
    /// killing someone's session to apply a preference would be worse than the inconsistency.
    pub fn set_shell(&self, path: &str) {
        settings::set_shell(path);
        self.save();
    }

    /// Settings → Shell → New tab in: home, or the active tab's directory.
    pub fn set_new_tab_dir(&self, idx: usize) {
        settings::set_new_tab_dir(settings::NewTabDir::from_index(idx));
        self.save();
    }

    /// Settings → Appearance → Padding: the inset around the terminal text. It changes how many
    /// cells fit, so every grid reflows and every shell gets a new window size.
    pub fn set_padding(&self, v: f64) {
        settings::set_pad(v);
        self.reflow_all();
        self.save();
    }

    /// Redraw the active tab (the only one mounted; the rest repaint when switched to).
    fn redraw_active(&self) {
        let m = self.model.borrow();
        if let Some(tab) = m.active.and_then(|a| m.tabs.iter().find(|t| t.id == a)) {
            unsafe { tab.view.setNeedsDisplay(true) };
        }
    }

    /// Where a new tab should start: the active tab's directory, or empty for "let the shell pick"
    /// (which `pty::spawn` turns into `$HOME`). See Settings → Shell → New tab in.
    fn new_tab_cwd(&self, inherited: String) -> String {
        match settings::new_tab_dir() {
            settings::NewTabDir::Active => inherited,
            settings::NewTabDir::Home => String::new(),
        }
    }

    /// Set the global font size to an absolute value (used by the settings dialog).
    pub fn set_font_size(&self, size: f64) {
        settings::set(&settings::family(), size);
        self.reflow_all();
        self.save();
    }

    /// ⌘0: restore the default font size.
    pub fn reset_font_size(&self) {
        self.set_font_size(settings::DEFAULT_SIZE);
    }

    /// Switch the global font family (settings::FAMILIES index).
    pub fn set_font_family(&self, idx: usize) {
        if let Some(fam) = settings::FAMILIES.get(idx) {
            settings::set(fam, settings::size());
            self.reflow_all();
            self.save();
        }
    }

    /// After a font change: reflow each tab's grid per the new metrics, and notify each PTY.
    fn reflow_all(&self) {
        let (cols, rows) = self.dims();
        let m = self.model.borrow();
        for tab in &m.tabs {
            tab.view.resize_grid(cols, rows);
            let ws = libc::winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(tab.master_fd, libc::TIOCSWINSZ, &ws) };
            unsafe { tab.view.setNeedsDisplay(true) };
        }
    }

    fn refresh_sidebar(&self) {
        unsafe { self.sidebar.setNeedsDisplay(true) };
    }

    /// Hand keyboard focus back to the current active terminal (used when exiting sidebar search).
    pub fn focus_terminal(&self) {
        let m = self.model.borrow();
        if let Some(a) = m.active {
            if let Some(tab) = m.tabs.iter().find(|t| t.id == a) {
                self.window.makeFirstResponder(Some(&tab.view));
            }
        }
    }

    /// Give keyboard focus to the sidebar (used when entering search).
    pub fn focus_sidebar(&self) {
        self.window.makeFirstResponder(Some(&self.sidebar));
    }

    /// ⌘F: enter sidebar search (expand first if collapsed).
    pub fn focus_search(&self) {
        if self.collapsed.get() {
            self.toggle_sidebar();
        }
        self.sidebar.begin_search();
    }

    /// ⌃↩: open the sidebar's context menu for the hovered row, or the active tab (expand first if collapsed).
    pub fn open_sidebar_menu(&self) {
        if self.collapsed.get() {
            self.toggle_sidebar();
        }
        self.sidebar.open_context_menu();
    }

    /// ⌘R: rename the active session in place (expand the sidebar first if collapsed — the edit
    /// box lives in the row, and there is nothing to type into while the sidebar is off screen).
    pub fn rename_active_tab(&self) {
        if self.collapsed.get() {
            self.toggle_sidebar();
        }
        self.sidebar.begin_rename_active();
    }

    /// ⌘,: open the settings dialog (built lazily, then reused).
    pub fn open_settings(&self) {
        if self.settings_dialog.borrow().is_none() {
            let d = SettingsDialog::new(self.mtm);
            d.set_controller(self as *const AppController);
            *self.settings_dialog.borrow_mut() = Some(d);
        }
        let d = self.settings_dialog.borrow();
        d.as_ref().unwrap().show(self.mtm);
    }

    /// ⌘W: close the active tab but keep the app running even if it was the last one.
    /// Only when there are no tabs at all does ⌘W quit the app.
    pub fn close_active_tab(&self) {
        let a = self.model.borrow().active;
        match a {
            Some(a) => self.close_tab_user(a),
            // Nothing selected: quit only from the genuine empty state. With sessions still open
            // (the user just deselected the active tab) ⌘W has no tab to close — do nothing rather
            // than tear down the whole app.
            None if self.model.borrow().tabs.is_empty() => unsafe {
                NSApplication::sharedApplication(self.mtm).terminate(None)
            },
            None => {}
        }
    }

    /// Shell → Export Text: write the active session's whole buffer — scrollback and screen — to a
    /// file the user picks.
    ///
    /// The panel is run modally, which pumps its own run loop: every PTY reader and both GCD timers
    /// keep firing underneath it, so the text is taken *before* it opens rather than after. What is
    /// saved is then what was on screen when the user asked, not whatever the shell printed while
    /// they were picking a folder.
    pub fn export_active_text(&self) {
        let Some((text, name)) = ({
            let m = self.model.borrow();
            m.active
                .and_then(|a| m.tabs.iter().find(|t| t.id == a))
                .map(|t| (t.view.text(), config::sanitize_label(&t.display_title())))
        }) else {
            return;
        };
        let panel = unsafe { NSSavePanel::savePanel(self.mtm) };
        unsafe {
            let stem = if name.trim().is_empty() { "session".to_string() } else { name };
            panel.setNameFieldStringValue(&NSString::from_str(&format!("{stem}.txt")));
            if panel.runModal() != NSModalResponseOK {
                return;
            }
        }
        let Some(url) = (unsafe { panel.URL() }) else { return };
        let Some(path) = (unsafe { url.path() }) else { return };
        if let Err(e) = std::fs::write(path.to_string(), text) {
            self.alert_export_failed(&e.to_string());
        }
    }

    /// The export could not be written (a read-only volume, a full disk). Worth an alert rather than
    /// a silent no-op: the user asked for a file and would otherwise go looking for one.
    fn alert_export_failed(&self, why: &str) {
        let alert = unsafe { NSAlert::new(self.mtm) };
        unsafe {
            alert.setMessageText(&NSString::from_str("Could not export the session"));
            alert.setInformativeText(&NSString::from_str(why));
            alert.runModal();
        }
    }

    /// Settings → Toolbar: the arrangement the customizer's drag left behind.
    ///
    /// Both halves travel together and are the customizer's whole state: `order` is what the
    /// trailing group holds, in order, and `hidden` is what it does not — see `toolbar::layout` for
    /// why the second is stored as the removed set rather than derived from the first.
    pub fn set_toolbar_layout(&self, order: Vec<String>, hidden: Vec<String>) {
        settings::set_toolbar_order(order);
        settings::set_toolbar_hidden(hidden);
        self.toolbar.rebuild();
        self.save();
    }

    /// Bring up the system's screenshot palette — the one ⇧⌘5 shows.
    ///
    /// Opens Screenshot.app rather than synthesizing the keystroke: a synthetic ⇧⌘5 needs
    /// Accessibility permission this app has no other reason to ask for, and would be swallowed
    /// outright if the user has rebound the shortcut. Launching the app is what the shortcut does.
    pub fn open_screenshot_ui(&self) {
        let _ = std::process::Command::new("open")
            .args(["-b", "com.apple.screenshot.launcher"])
            .spawn();
    }

    /// Open the active tab's current directory in Finder.
    pub fn reveal_in_finder(&self) {
        if let Some(a) = self.model.borrow().active {
            self.reveal_in_finder_id(a);
        }
    }

    /// Open a specific tab's current directory in Finder (reported via OSC 7; falls back to the
    /// spawn-time directory when missing). Used by the per-tab context menu.
    pub fn reveal_in_finder_id(&self, id: u64) {
        let cwd = {
            let m = self.model.borrow();
            m.tabs
                .iter()
                .find(|t| t.id == id)
                .map(Tab::cwd)
                .unwrap_or_default()
        };
        if !cwd.is_empty() {
            let _ = std::process::Command::new("open").arg(&cwd).spawn();
        }
    }

    /// Type `cmd` into the active session and run it — what the toolbar's `claude` / `codex`
    /// buttons do. Deliberately the same as typing it: the shell resolves the name on `$PATH`, and
    /// if something is already running there the text lands in that program, exactly as it would
    /// from the keyboard.
    pub fn run_in_active(&self, cmd: &str) {
        let m = self.model.borrow();
        if let Some(a) = m.active {
            if let Some(tab) = m.tabs.iter().find(|t| t.id == a) {
                tab.view.send(format!("{cmd}\r").as_bytes());
            }
        }
    }

    /// Clear the active terminal — by asking the *shell* to, with a `^L`, not by blanking the grid.
    ///
    /// Wiping the grid directly (`\e[2J\e[H`, which is what this used to do) leaves the shell
    /// believing its prompt is still on screen: the line is gone, the cursor is at the top, and
    /// nothing comes back until the next Return. `^L` is what every terminal's clear is, and every
    /// program knows it — the shell repaints its prompt at the top of the cleared screen, and a
    /// full-screen application redraws itself instead, which is the right answer for both.
    pub fn clear_active(&self) {
        self.send_active(b"\x0c");
    }

    /// Discard whatever is typed at the prompt, as `⌃U` does — the toolbar's other eraser. Sent to
    /// the shell for the same reason `⌃L` is: the line belongs to the line editor, not to the grid,
    /// and blanking those cells would leave the shell still holding the text.
    pub fn clear_line_active(&self) {
        self.send_active(b"\x15");
    }

    /// Interrupt whatever is running, as `⌃C` does. Written to the shell rather than signalled from
    /// here for the same reason the two erasers are: the terminal driver turns that byte into
    /// SIGINT for the *foreground* process group, which is the one the user means and the one this
    /// side cannot name — a `kill` from here would have to guess at it.
    pub fn interrupt_active(&self) {
        self.send_active(b"\x03");
    }

    /// Restart the active session: a fresh shell in the same tab, in the same directory.
    ///
    /// [`Self::restart_tab`] alone is only correct for a session that has **already ended** — it
    /// overwrites `master_fd` and the reader token without closing either, so on a live tab it would
    /// leak the pty and orphan the shell. So a live one is taken down first, exactly as
    /// `end_tab_session` takes down a shell that exited on its own, after hanging up its process
    /// group. A foreground job is confirmed first, on the same terms closing the tab would be: the
    /// hangup kills it either way, and this is the user's last chance to say no.
    pub fn restart_active_tab(&self) {
        let Some(id) = self.model.borrow().active else { return };
        let live = {
            let m = self.model.borrow();
            m.tabs.iter().find(|t| t.id == id).map(|t| t.state != SessionState::Ended).unwrap_or(false)
        };
        if live {
            if self.tab_has_foreground_job(id)
                && !confirm(self.mtm, "Restart this session?", RUNNING_JOB_WARNING, "Restart")
            {
                return;
            }
            // The shell is its own session leader (`setsid` in `pty::spawn`), so the negated pid
            // names its whole process group — the job it is running goes with it. Read before the
            // teardown, which sets the pid to -1.
            let pid = self.model.borrow().tabs.iter().find(|t| t.id == id).map(|t| t.shell_pid);
            if let Some(pid) = pid.filter(|p| *p > 0) {
                unsafe { libc::kill(-pid, libc::SIGHUP) };
            }
            // Then exactly what a shell exiting on its own does — including the sidebar and header
            // refreshes it owes for setting the state directly. Hand-rolling the teardown here
            // would skip those, and `restart_tab` can still fail (no fds, no process table), which
            // would leave the tab ended behind a stale dot and a stale meta line.
            self.end_tab_session(id);
        }
        self.restart_tab(id);
    }

    /// Write bytes to the active session's shell as if typed.
    fn send_active(&self, bytes: &[u8]) {
        let m = self.model.borrow();
        if let Some(a) = m.active {
            if let Some(tab) = m.tabs.iter().find(|t| t.id == a) {
                tab.view.send(bytes);
            }
        }
    }

    /// Collapse/expand the sidebar (⌘B). When collapsed, the terminal fills the entire content area.
    pub fn toggle_sidebar(&self) {
        // Collapsing must not leave an invisible search/rename box holding the keyboard.
        self.sidebar.end_input();
        self.collapsed.set(!self.collapsed.get());
        // Slide the card in/out instead of swapping it in place. Only the card and the toggle
        // animate: the terminal host jumps straight to its final width, because animating it would
        // reflow the grid and TIOCSWINSZ the shell on every frame of the slide.
        self.animating.set(true);
        self.relayout();
        self.animating.set(false);
        self.update_title();
    }

    /// Persist once on app exit (save a snapshot of each tab's current content). See the app delegate in main.
    pub fn persist(&self) {
        self.save();
    }

    /// Persist the layout + session state (cwd + each tab's currently visible content).
    fn save(&self) {
        let m = self.model.borrow();
        // The *displayed* title is what gets written, pinned or not, so a restored session reads
        // right before any shell has reported anything; `auto` is what says whether the app may
        // replace it again.
        let tab_state = |id: &u64| {
            m.tabs.iter().find(|t| t.id == *id).map(|t| config::TabState {
                title: t.display_title(),
                cwd: t.cwd(),
                dot: t.dot,
                locked: t.locked,
                auto: !t.pinned,
            })
        };
        let ungrouped: Vec<config::SavedTab> = m.ungrouped.iter().filter_map(tab_state).collect();
        let groups: Vec<config::SavedGroup> = m
            .groups
            .iter()
            .map(|g| {
                let tabs = g.tabs.iter().filter_map(tab_state).collect();
                (g.name.clone(), g.collapsed, tabs)
            })
            .collect();
        drop(m);
        let (window_w, window_h) = self.window_size();
        // Read at save time rather than tracked as the window moves: `windowDidMove:` fires per
        // frame of a drag, and this rewrites the whole file. Every layout change saves, and so does
        // quitting, which is what a restored position actually has to survive.
        let origin = self.window.frame().origin;
        config::save(
            &config::Settings {
                style: theme::name_of(self.style.get()),
                font_family: settings::family(),
                font_size: settings::size(),
                sidebar_w: self.sidebar_w.get(),
                sidebar_right: self.sidebar_right.get(),
                window_w,
                window_h,
                window_x: Some(origin.x),
                window_y: Some(origin.y),
                cursor_shape: settings::cursor_shape(),
                cursor_blink: settings::cursor_blink(),
                scrollback: settings::scrollback(),
                shell: settings::shell(),
                new_tab_dir: settings::new_tab_dir(),
                padding: settings::pad(),
                opacity: settings::opacity(),
                toolbar_hidden: settings::toolbar_hidden(),
                toolbar_order: settings::toolbar_order(),
            },
            &ungrouped,
            &groups,
        );
    }
}

/// Shared wording for "closing this will kill a running process" confirmations.
const RUNNING_JOB_WARNING: &str = "A process is still running in this terminal. Closing it will terminate that process.";

/// A Close/Cancel-style confirmation alert. `affirmative` labels the destructive button (shown
/// first); returns true only if the user picked it.
fn confirm(mtm: MainThreadMarker, title: &str, info: &str, affirmative: &str) -> bool {
    let alert = unsafe { NSAlert::new(mtm) };
    unsafe {
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(info));
        alert.addButtonWithTitle(&NSString::from_str(affirmative));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.runModal() == 1000 // NSAlertFirstButtonReturn
    }
}

/// TermView calls back into the controller through this when the shell exits (see `view::EndFn`).
fn end_cb(ctx: *const c_void, id: u64) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.end_tab_session(id);
}

/// TermView calls back into the controller through this when the user presses Enter on an ended
/// tab (see `view::RestartFn`).
fn restart_cb(ctx: *const c_void, id: u64) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.restart_tab(id);
}

/// TermView calls back through this when the shell reported a new title or cwd (see `view::MetaFn`).
fn meta_cb(ctx: *const c_void, id: u64) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.on_tab_meta(id);
}

/// When TermView receives ⌘B it calls back into the controller to collapse the sidebar (see `view::CmdFn`).
fn toggle_cb(ctx: *const c_void) {
    let ctrl = unsafe { &*(ctx as *const AppController) };
    ctrl.toggle_sidebar();
}


//! Settings dialog: a small native panel with standard AppKit controls, split into tabs
//! (Theme / Appearance / Toolbar / Terminal / Shell).
//!
//! It replaces the old bottom-of-sidebar pop-up menu. The controls read/write through the
//! [`AppController`], so every change applies live and is persisted immediately. The panel
//! object is kept alive by the controller (it holds the `Retained<SettingsDialog>`); the
//! dialog holds only a raw pointer back to the controller, so there is no reference cycle.
//!
//! Rows are laid out by [`Rows`], a cursor that walks down a pane one `ROW_H` at a time: adding a
//! setting is one `rows.next()`, not a re-tune of a column of hand-computed constants. The panes
//! are sized to the tallest one (`MAX_ROWS`), so switching tabs never resizes the window.
//!
//! Two panes are not control rows at all but self-drawn views, for the same reason: a name in a
//! pop-up says nothing about a color scheme, and a checkbox says nothing about where a button sits.
//! Theme is `theme_grid.rs`, Toolbar is `toolbar_editor.rs` — and the second is what sets [`W`],
//! since it draws the window's own title bar at 1:1.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor, NSPopUpButton,
    NSScrollView, NSSegmentedControl,
    NSSegmentStyle, NSSlider, NSTabView, NSTabViewItem, NSTabViewType, NSTextAlignment, NSTextField, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::app::AppController;
use crate::settings;
use crate::theme_grid::ThemeGrid;
use crate::toolbar_editor::ToolbarEditor;

/// Window content width. The panes are the same, less `MARGIN` on each side. Sized for the Toolbar
/// pane, which draws the window's own title bar at 1:1 and therefore needs room for the whole run
/// of buttons *plus* the traffic lights and the title beside them; the theme grid (two preview
/// cards) and the control rows both want less and simply spread out.
const W: f64 = 640.0;
/// Gap between the window edge and the tab view.
const MARGIN: f64 = 12.0;
/// Vertical distance between two rows.
const ROW_H: f64 = 38.0;
/// Height of every pane's content area — one size for all of them, so switching tabs never
/// resizes the window. Set by the Toolbar pane's worst case: its two rows, plus a palette holding
/// every button (all of them dragged out), plus the Restore Defaults row. That pane is drawn into a
/// fixed frame and a palette taller than it is clipped rather than scrolled, so the size has to be
/// the worst case and not the usual one. The theme grid scrolls and the control panes hold less, so
/// both simply use what they need of it.
///
/// It grew when the palette split per group: the tiles still wrap into the same three rows, but the
/// two halves wrap *independently* and each carries a label, so the worst case gained two headings
/// and the gap between them. The previous value fit the old worst case to the point — it was exactly
/// `PANE_H - RESET_H` — so there was nothing left to absorb them, and the last tile row came out
/// from under the clip on top of the Restore Defaults button.
const PANE_H: f64 = 508.0;
/// Gap between the top of a pane and its first row.
const PANE_TOP: f64 = 20.0;
/// Room the Toolbar pane leaves at its bottom for the Restore Defaults button.
const RESET_H: f64 = 40.0;

/// The panes, in order — the segmented switcher's labels and the tab view's items.
const PANES: [&str; 5] = ["Theme", "Appearance", "Toolbar", "Terminal", "Shell"];
/// The switcher's own strip above the panes.
const SWITCHER_H: f64 = 34.0;
/// Sized for the widest label at the segment count — five panes at 340 squeezed
/// "Appearance" into an ellipsis.
const SWITCHER_W: f64 = 425.0;

/// A labelled row is a fixed block — label column, gap, control — and the block is *centered* in
/// the pane rather than pinned to a left margin. The panel's width is set by the Toolbar pane's 1:1
/// title bar, which is far wider than any control row needs; left-pinned rows in a pane that wide
/// leave a field of empty space on one side only, under a switcher that is itself centered.
const LABEL_W: f64 = 100.0;
const CTRL_W: f64 = 250.0;
/// Label-to-control gap, and the whole block's width.
const LABEL_GAP: f64 = 8.0;
const ROW_W: f64 = LABEL_W + LABEL_GAP + CTRL_W;
/// Where that block starts. Measured against the pane, which is the window less its margins and the
/// few points of chrome the tab view keeps for itself — close enough to centered that a point of
/// slop does not show, and it costs no plumbing through every row builder.
const LABEL_X: f64 = (W - 2.0 * MARGIN - ROW_W) / 2.0;
const CTRL_X: f64 = LABEL_X + LABEL_W + LABEL_GAP;
/// Width of the number shown beside a slider, at the right end of the control column.
const VALUE_W: f64 = 26.0;
/// Font-size range the slider covers. The same bounds the text field used to clamp to, so no
/// existing configuration falls outside what the control can represent.
const MIN_SIZE: f64 = 8.0;
const MAX_SIZE: f64 = 40.0;

/// A downward cursor over one pane's rows: `next()` yields the next row's baseline y. Panes are
/// non-flipped (y grows upward), so it counts down from the top.
struct Rows {
    y: f64,
}

impl Rows {
    /// Start at the top of the pane. Every pane's first row lands on the same line, so switching
    /// tabs moves the labels sideways and not up and down.
    fn new() -> Self {
        Rows { y: PANE_H - PANE_TOP }
    }

    fn next(&mut self) -> f64 {
        self.y -= ROW_H;
        self.y
    }
}

pub struct DialogIvars {
    controller: Cell<*const AppController>,
    window: RefCell<Option<Retained<NSWindow>>>,
    tabs: RefCell<Option<Retained<NSTabView>>>,
    theme_grid: RefCell<Option<Retained<ThemeGrid>>>,
    fam_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    size_slider: RefCell<Option<Retained<NSSlider>>>,
    size_value: RefCell<Option<Retained<NSTextField>>>,
    side_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    toolbar_editor: RefCell<Option<Retained<ToolbarEditor>>>,
    pad_slider: RefCell<Option<Retained<NSSlider>>>,
    pad_value: RefCell<Option<Retained<NSTextField>>>,
    cursor_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    scrollback_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    scrollback_items: RefCell<Vec<usize>>, // the pop-up's values, in item order
    shell_field: RefCell<Option<Retained<NSTextField>>>,
    new_tab_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    blink_pop: RefCell<Option<Retained<NSPopUpButton>>>,
    opacity_pop: RefCell<Option<Retained<NSPopUpButton>>>,
}

/// Opacity steps the pop-up offers, as percentages, opaque first.
const OPACITY_PERCENTS: [u32; 11] = [100, 95, 90, 85, 80, 75, 70, 65, 60, 55, 50];

/// Scrollback depths the pop-up offers. A hand-edited `layout.conf` may hold something else, in
/// which case that value is spliced in (see `scrollback_items`) rather than silently rounded.
const SCROLLBACK_PRESETS: [usize; 8] = [500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000];

declare_class!(
    pub struct SettingsDialog;

    unsafe impl ClassType for SettingsDialog {
        type Super = objc2::runtime::NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "SettingsDialog";
    }

    impl DeclaredClass for SettingsDialog {
        type Ivars = DialogIvars;
    }

    unsafe impl NSObjectProtocol for SettingsDialog {}

    unsafe impl NSWindowDelegate for SettingsDialog {
        /// Closing the panel tears down the field editor without committing it, so a value typed
        /// into one of the text fields and *not* followed by Return would otherwise be dropped on
        /// the floor — while every pop-up beside it applies the moment it changes. Read the fields
        /// out here instead of trusting AppKit's action to have fired.
        #[method(windowWillClose:)]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.commit_fields();
        }
    }

    unsafe impl SettingsDialog {
        /// The pane switcher. The tab view itself draws no tabs (see `show`), so this segmented
        /// control is the whole of the switching UI.
        /// Esc closes the panel. Wired to a hidden button rather than to a responder method: this
        /// is a plain window, so `cancelOperation:` only reaches whatever control has the keyboard
        /// (a text field swallows it), and a key equivalent is the one route AppKit checks before
        /// any of that. The close goes through `performClose:` so `windowWillClose:` still runs and
        /// the shell field is still committed.
        #[method(cancelPanel:)]
        fn cancel_panel(&self, _sender: Option<&AnyObject>) {
            if let Some(w) = self.ivars().window.borrow().as_ref() {
                unsafe { w.performClose(None) };
            }
        }

        /// Toolbar → Restore Defaults: every button back, in the built-in order.
        #[method(restoreToolbar:)]
        fn restore_toolbar(&self, _sender: &NSButton) {
            if let Some(e) = self.ivars().toolbar_editor.borrow().as_ref() {
                e.restore_defaults();
            }
        }

        #[method(panePicked:)]
        fn pane_picked(&self, sender: &NSSegmentedControl) {
            let idx = unsafe { sender.selectedSegment() };
            if let Some(t) = self.ivars().tabs.borrow().as_ref() {
                unsafe { t.selectTabViewItemAtIndex(idx) };
            }
        }

        #[method(fontFamilyChanged:)]
        fn font_family_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let Some(c) = self.controller() {
                c.set_font_family(idx);
            }
        }

        // Stepper arrows: push the new value into the text field, then apply.
        // Dragged the size slider. Continuous, so this fires per pixel of travel — but the value
        // it carries is rounded to a whole point and applied only when that lands on a different
        // one, because applying reflows every tab's grid and re-sends `TIOCSWINSZ`.
        #[method(sizeChanged:)]
        fn size_changed(&self, sender: &NSSlider) {
            let v = unsafe { sender.doubleValue() }.round();
            self.set_value_label(&self.ivars().size_value, v);
            if v != settings::size() {
                self.apply_size(v);
            }
        }

        #[method(sidebarChanged:)]
        fn sidebar_changed(&self, sender: &NSPopUpButton) {
            let on_right = unsafe { sender.indexOfSelectedItem() } == 1; // 0 = Left, 1 = Right
            if let Some(c) = self.controller() {
                c.set_sidebar_side(on_right);
            }
        }

        // Same shape as the size slider: rounded, and applied only on a real step (padding feeds
        // `dims()`, so every apply reflows the grid too).
        #[method(padChanged:)]
        fn pad_changed(&self, sender: &NSSlider) {
            let v = unsafe { sender.doubleValue() }.round();
            self.set_value_label(&self.ivars().pad_value, v);
            if v != settings::pad() {
                if let Some(c) = self.controller() {
                    c.set_padding(v);
                }
            }
        }

        #[method(cursorChanged:)]
        fn cursor_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let Some(c) = self.controller() {
                c.set_cursor_shape(idx);
            }
        }

        #[method(scrollbackChanged:)]
        fn scrollback_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            let lines = self.ivars().scrollback_items.borrow().get(idx).copied();
            if let (Some(c), Some(lines)) = (self.controller(), lines) {
                c.set_scrollback(lines);
            }
        }

        // Shell path edited (Enter): applies to tabs opened from now on.
        #[method(shellEdited:)]
        fn shell_edited(&self, sender: &NSTextField) {
            let path = unsafe { sender.stringValue() }.to_string();
            if let Some(c) = self.controller() {
                c.set_shell(&path);
            }
        }

        #[method(blinkChanged:)]
        fn blink_changed(&self, sender: &NSPopUpButton) {
            let on = unsafe { sender.indexOfSelectedItem() } == 1; // 0 = Off, 1 = On
            if let Some(c) = self.controller() {
                c.set_cursor_blink(on);
            }
        }

        #[method(opacityChanged:)]
        fn opacity_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let (Some(c), Some(p)) = (self.controller(), OPACITY_PERCENTS.get(idx)) {
                c.set_opacity(*p as f64 / 100.0);
            }
        }

        #[method(newTabDirChanged:)]
        fn new_tab_dir_changed(&self, sender: &NSPopUpButton) {
            let idx = unsafe { sender.indexOfSelectedItem() } as usize;
            if let Some(c) = self.controller() {
                c.set_new_tab_dir(idx);
            }
        }
    }
);

impl SettingsDialog {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(DialogIvars {
            controller: Cell::new(std::ptr::null()),
            window: RefCell::new(None),
            tabs: RefCell::new(None),
            theme_grid: RefCell::new(None),
            fam_pop: RefCell::new(None),
            size_slider: RefCell::new(None),
            size_value: RefCell::new(None),
            side_pop: RefCell::new(None),
            toolbar_editor: RefCell::new(None),
            pad_slider: RefCell::new(None),
            pad_value: RefCell::new(None),
            cursor_pop: RefCell::new(None),
            scrollback_pop: RefCell::new(None),
            scrollback_items: RefCell::new(Vec::new()),
            shell_field: RefCell::new(None),
            new_tab_pop: RefCell::new(None),
            blink_pop: RefCell::new(None),
            opacity_pop: RefCell::new(None),
        });
        unsafe { msg_send_id![super(this), init] }
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

    fn apply_size(&self, size: f64) {
        if let Some(c) = self.controller() {
            c.set_font_size(size);
        }
    }

    /// Build the panel (if needed) and bring it to front, seeded with the current settings.
    pub fn show(&self, mtm: MainThreadMarker) {
        // Already built: just refresh values and re-show.
        if self.ivars().window.borrow().is_some() {
            self.seed_values();
            if let Some(w) = self.ivars().window.borrow().as_ref() {
                w.center();
                w.makeKeyAndOrderFront(None);
            }
            let app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            return;
        }

        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        // Provisional height; the real one depends on how much chrome the tab view adds around a
        // PANE_H content area, which only the built tab view can answer (see below).
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, PANE_H + 80.0));
        let window: Retained<NSWindow> = unsafe {
            msg_send_id![
                mtm.alloc::<NSWindow>(),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::NSBackingStoreBuffered,
                defer: false,
            ]
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Settings"));
        // The dialog is the window's delegate purely to catch the close (see `windowWillClose:`).
        // The window is owned by the dialog, which the controller owns, so this is not a cycle
        // AppKit can trip over: `setDelegate:` does not retain.
        window.setDelegate(Some(ProtocolObject::from_ref(self)));

        let content = window.contentView().expect("content view");

        // Esc → close. A zero-size button parked off the panel's own layout: it is never seen and
        // never tabbed to, it exists only to carry the key equivalent.
        let esc: Retained<NSButton> = unsafe {
            msg_send_id![mtm.alloc::<NSButton>(), initWithFrame: NSRect::new(NSPoint::new(-100.0, -100.0), NSSize::new(1.0, 1.0))]
        };
        unsafe {
            esc.setTitle(&NSString::from_str(""));
            esc.setKeyEquivalent(&NSString::from_str("\u{1b}"));
            let _: () = msg_send![&esc, setTarget: self];
            esc.setAction(Some(sel!(cancelPanel:)));
            content.addSubview(&esc);
        }

        // ---- Panes: a borderless tab view driven by a segmented control ----
        // `NSNoTabsNoBorder` drops both the tab strip and the box AppKit draws around the content;
        // the pills below replace the strip, and the panel is left as one flat surface.
        let tab_w = W - 2.0 * MARGIN;
        let tabs: Retained<NSTabView> = unsafe {
            msg_send_id![mtm.alloc::<NSTabView>(), initWithFrame: NSRect::new(
                NSPoint::new(MARGIN, MARGIN), NSSize::new(tab_w, PANE_H))]
        };
        unsafe { tabs.setTabViewType(NSTabViewType::NSNoTabsNoBorder) };
        // Even borderless the content rect is inset a little, so measure the overhead rather than
        // hard-coding it, and grow the view until its *content* is exactly PANE_H.
        let inner = unsafe { tabs.contentRect() };
        let chrome_h = PANE_H - inner.size.height;
        let chrome_w = tab_w - inner.size.width;
        let tab_h = PANE_H + chrome_h;
        unsafe {
            tabs.setFrame(NSRect::new(NSPoint::new(MARGIN, MARGIN), NSSize::new(tab_w, tab_h)));
        }
        let pane_w = tab_w - chrome_w;
        window.setContentSize(NSSize::new(W, tab_h + SWITCHER_H + 2.0 * MARGIN));

        unsafe {
            add_pane(&tabs, PANES[0], self.build_theme(pane_w, mtm), mtm);
            add_pane(&tabs, PANES[1], self.build_appearance(pane_w, mtm), mtm);
            add_pane(&tabs, PANES[2], self.build_toolbar(pane_w, mtm), mtm);
            add_pane(&tabs, PANES[3], self.build_terminal(pane_w, mtm), mtm);
            add_pane(&tabs, PANES[4], self.build_shell(pane_w, mtm), mtm);
            content.addSubview(&tabs);
        }

        // The switcher, centered above the panes.
        let seg: Retained<NSSegmentedControl> = unsafe {
            msg_send_id![mtm.alloc::<NSSegmentedControl>(), initWithFrame: NSRect::new(
                NSPoint::new(0.0, 0.0), NSSize::new(SWITCHER_W, 24.0))]
        };
        unsafe {
            seg.setSegmentStyle(NSSegmentStyle::TexturedRounded);
            seg.setSegmentCount(PANES.len() as isize);
            for (i, title) in PANES.iter().enumerate() {
                seg.setLabel_forSegment(&NSString::from_str(title), i as isize);
                seg.setWidth_forSegment(SWITCHER_W / PANES.len() as f64, i as isize);
            }
            seg.setSelectedSegment(0);
            let _: () = msg_send![&seg, setTarget: self];
            seg.setAction(Some(sel!(panePicked:)));
            let y = tab_h + 2.0 * MARGIN - 6.0;
            seg.setFrame(NSRect::new(NSPoint::new((W - SWITCHER_W) / 2.0, y), NSSize::new(SWITCHER_W, 24.0)));
            content.addSubview(&seg);
        }
        *self.ivars().tabs.borrow_mut() = Some(tabs.clone());

        // No explicit Done button: the window's title-bar close button dismisses the panel.

        // No appearance is set on this window: unlike the terminal, whose chrome is the theme's,
        // the settings panel is a system panel and follows the system's light/dark setting. The
        // theme is previewed inside it (the Theme pane's cards), not applied to it.
        *self.ivars().window.borrow_mut() = Some(window.clone());
        self.seed_values();

        window.center();
        window.makeKeyAndOrderFront(None);
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    /// Appearance: theme, font family, font size, sidebar side, border.
    fn build_appearance(&self, pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
        let pane = new_pane(pane_w, mtm);
        let mut rows = Rows::new();

        let y = rows.next();
        add_label(&pane, "Font", y, mtm);
        let fam_pop = self.make_popup(settings::FAMILIES.iter().copied(), y - 3.0, sel!(fontFamilyChanged:), mtm);
        unsafe { pane.addSubview(&fam_pop) };

        // Font size: a slider over the range the app accepts, with the value beside it.
        let y = rows.next();
        add_label(&pane, "Size", y, mtm);
        let (slider, value) = self.make_slider_row(&pane, y, MIN_SIZE, MAX_SIZE, sel!(sizeChanged:), mtm);
        *self.ivars().size_slider.borrow_mut() = Some(slider);
        *self.ivars().size_value.borrow_mut() = Some(value);

        let y = rows.next();
        add_label(&pane, "Sidebar", y, mtm);
        let side_pop = self.make_popup(["Left", "Right"].into_iter(), y - 3.0, sel!(sidebarChanged:), mtm);
        unsafe { pane.addSubview(&side_pop) };

        // Padding: the inset around the terminal text, in points.
        let y = rows.next();
        add_label(&pane, "Padding", y, mtm);
        let (pad_slider, pad_value) = self.make_slider_row(&pane, y, 0.0, settings::MAX_PAD, sel!(padChanged:), mtm);
        *self.ivars().pad_slider.borrow_mut() = Some(pad_slider);
        *self.ivars().pad_value.borrow_mut() = Some(pad_value);

        // Opacity: the background alpha, in whole percent.
        let y = rows.next();
        add_label(&pane, "Opacity", y, mtm);
        let labels: Vec<String> = OPACITY_PERCENTS.iter().map(|p| format!("{p}%")).collect();
        let opacity_pop =
            self.make_popup(labels.iter().map(|s| s.as_str()), y - 3.0, sel!(opacityChanged:), mtm);
        unsafe { pane.addSubview(&opacity_pop) };
        *self.ivars().opacity_pop.borrow_mut() = Some(opacity_pop);

        // Remember the control references we need to re-seed later.
        *self.ivars().fam_pop.borrow_mut() = Some(fam_pop);
        *self.ivars().side_pop.borrow_mut() = Some(side_pop);
        pane
    }

    /// Theme: the preview grid, scrolling because `themes.conf` can hold any number of themes.
    fn build_theme(&self, pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
        let pane = new_pane(pane_w, mtm);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(pane_w, PANE_H));
        let scroll: Retained<NSScrollView> = unsafe { msg_send_id![mtm.alloc::<NSScrollView>(), initWithFrame: frame] };
        // The grid paints every card itself, so the scroll view must not paint a slab of system
        // background behind them — the pane's own color is what the gaps should show.
        unsafe {
            scroll.setDrawsBackground(false);
            scroll.setHasVerticalScroller(true);
        }
        // Size the grid to the *clip* view, not to the pane: a legacy (non-overlay) scroller eats
        // width, and a grid built at the pane's width would put its last column under it.
        let inner_w = unsafe { scroll.contentSize() }.width;
        let grid = ThemeGrid::new(mtm, inner_w);
        if let Some(c) = self.controller() {
            grid.set_controller(c as *const AppController);
        }
        unsafe {
            // Follow the clip view if the scroller comes and goes (it does, with "when scrolling").
            grid.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewWidthSizable);
            scroll.setDocumentView(Some(&grid));
            pane.addSubview(&scroll);
        }
        *self.ivars().theme_grid.borrow_mut() = Some(grid);
        pane
    }

    /// Terminal: cursor shape, scrollback depth.
    fn build_terminal(&self, pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
        let pane = new_pane(pane_w, mtm);
        let mut rows = Rows::new();

        let y = rows.next();
        add_label(&pane, "Cursor", y, mtm);
        let cursor_pop = self.make_popup(
            settings::CursorShape::ALL.iter().map(|c| c.label()),
            y - 3.0,
            sel!(cursorChanged:),
            mtm,
        );
        unsafe { pane.addSubview(&cursor_pop) };

        let y = rows.next();
        add_label(&pane, "Blink", y, mtm);
        let blink_pop = self.make_popup(["Off", "On"].into_iter(), y - 3.0, sel!(blinkChanged:), mtm);
        unsafe { pane.addSubview(&blink_pop) };
        *self.ivars().blink_pop.borrow_mut() = Some(blink_pop);

        let y = rows.next();
        add_label(&pane, "Scrollback", y, mtm);
        let items = scrollback_items(settings::scrollback());
        let labels: Vec<String> = items.iter().map(|n| format!("{n} lines")).collect();
        let scrollback_pop =
            self.make_popup(labels.iter().map(|s| s.as_str()), y - 3.0, sel!(scrollbackChanged:), mtm);
        unsafe { pane.addSubview(&scrollback_pop) };
        *self.ivars().scrollback_items.borrow_mut() = items;

        *self.ivars().cursor_pop.borrow_mut() = Some(cursor_pop);
        *self.ivars().scrollback_pop.borrow_mut() = Some(scrollback_pop);
        pane
    }

    /// Shell: which shell to run, and where a new tab starts.
    /// Toolbar: the customizer — a full-size picture of the band with the buttons in it, and the
    /// ones that are out below (`toolbar_editor.rs`).
    ///
    /// The editor is self-drawn and top-down, so it is mounted flush with the top of the pane and
    /// sized by its own `height()`; the only standard control here is the Restore Defaults button
    /// under it, on the pane's own bottom edge.
    fn build_toolbar(&self, pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
        let pane = new_pane(pane_w, mtm);
        let editor = ToolbarEditor::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(pane_w, PANE_H)));
        if let Some(c) = self.controller() {
            editor.set_controller(c as *const AppController);
        }
        // The controller answers the band's height, so the editor can only size itself once it has
        // one — and a non-flipped pane places a subview by its bottom edge, so the frame is set
        // from the measured height rather than left at the pane's.
        let h = editor.height(pane_w).min(PANE_H - RESET_H);
        unsafe {
            editor.setFrame(NSRect::new(NSPoint::new(0.0, PANE_H - h), NSSize::new(pane_w, h)));
            pane.addSubview(&editor);
        }

        let reset = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Restore Defaults"),
                Some(self),
                Some(sel!(restoreToolbar:)),
                mtm,
            )
        };
        unsafe {
            // Aligned with the editor's own left margin, not with the control rows: this pane has
            // none, and the button belongs under the band's leading edge.
            reset.setFrame(NSRect::new(NSPoint::new(crate::toolbar_editor::PAD_X - 6.0, 8.0), NSSize::new(150.0, 24.0)));
            pane.addSubview(&reset);
        }
        *self.ivars().toolbar_editor.borrow_mut() = Some(editor);
        pane
    }

    fn build_shell(&self, pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
        let pane = new_pane(pane_w, mtm);
        let mut rows = Rows::new();

        let y = rows.next();
        add_label(&pane, "Shell", y, mtm);
        let field: Retained<NSTextField> = unsafe {
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: NSRect::new(
                NSPoint::new(CTRL_X, y - 2.0), NSSize::new(CTRL_W, 22.0))]
        };
        unsafe {
            field.setEditable(true);
            field.setBezeled(true);
            // Empty means "whatever $SHELL says", which is what the placeholder has to convey.
            let _: () = msg_send![&field, setPlaceholderString: &*NSString::from_str("$SHELL")];
            let _: () = msg_send![&field, setTarget: self];
            field.setAction(Some(sel!(shellEdited:)));
            pane.addSubview(&field);
        }
        commit_on_end_editing(&field);
        *self.ivars().shell_field.borrow_mut() = Some(field);

        let y = rows.next();
        add_label(&pane, "New tab in", y, mtm);
        let new_tab_pop = self.make_popup(
            settings::NewTabDir::ALL.iter().map(|d| d.label()),
            y - 3.0,
            sel!(newTabDirChanged:),
            mtm,
        );
        unsafe { pane.addSubview(&new_tab_pop) };
        *self.ivars().new_tab_pop.borrow_mut() = Some(new_tab_pop);
        pane
    }

    /// An editable number field plus its stepper, both wired to this dialog, added to `pane`.
    /// A slider plus the number it is on: `[====|=======] 13`.
    ///
    /// The slider is continuous (the terminal follows the drag, which is the point of using one
    /// here) and unsnapped: tick marks for a 30-step range draw as a comb under the control, so the
    /// rounding lives in the action instead.
    fn make_slider_row(
        &self,
        pane: &NSView,
        y: f64,
        min: f64,
        max: f64,
        action: objc2::runtime::Sel,
        mtm: MainThreadMarker,
    ) -> (Retained<NSSlider>, Retained<NSTextField>) {
        let slider: Retained<NSSlider> = unsafe {
            msg_send_id![mtm.alloc::<NSSlider>(), initWithFrame: NSRect::new(
                NSPoint::new(CTRL_X, y - 2.0), NSSize::new(CTRL_W - VALUE_W - 8.0, 20.0))]
        };
        let value = unsafe { NSTextField::labelWithString(&NSString::from_str(""), mtm) };
        unsafe {
            slider.setMinValue(min);
            slider.setMaxValue(max);
            slider.setContinuous(true);
            let _: () = msg_send![&slider, setTarget: self];
            slider.setAction(Some(action));
            value.setFrame(NSRect::new(
                NSPoint::new(CTRL_X + CTRL_W - VALUE_W, y - 1.0),
                NSSize::new(VALUE_W, 18.0),
            ));
            value.setAlignment(NSTextAlignment::Right);
            pane.addSubview(&slider);
            pane.addSubview(&value);
        }
        (slider, value)
    }

    /// Write a slider's current number into the label beside it.
    fn set_value_label(&self, label: &RefCell<Option<Retained<NSTextField>>>, v: f64) {
        if let Some(l) = label.borrow().as_ref() {
            unsafe { l.setStringValue(&NSString::from_str(&format!("{}", v as i64))) };
        }
    }

    /// Build a pop-up button filled with `titles`, wired to `action`, positioned at `y`.
    fn make_popup<'a>(
        &self,
        titles: impl Iterator<Item = &'a str>,
        y: f64,
        action: objc2::runtime::Sel,
        mtm: MainThreadMarker,
    ) -> Retained<NSPopUpButton> {
        let frame = NSRect::new(NSPoint::new(CTRL_X, y), NSSize::new(CTRL_W, 26.0));
        let pop: Retained<NSPopUpButton> =
            unsafe { msg_send_id![mtm.alloc::<NSPopUpButton>(), initWithFrame: frame, pullsDown: false] };
        for t in titles {
            unsafe { pop.addItemWithTitle(&NSString::from_str(t)) };
        }
        unsafe {
            let _: () = msg_send![&pop, setTarget: self];
            pop.setAction(Some(action));
        }
        pop
    }

    /// Apply whatever is currently typed in the text fields, skipping values that already match.
    ///
    /// Only the shell is left to commit: size and padding are sliders now, and a slider has no
    /// uncommitted state — it applies as it moves.
    fn commit_fields(&self) {
        let store = self.ivars();
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        if let Some(f) = store.shell_field.borrow().as_ref() {
            let path = unsafe { f.stringValue() }.to_string();
            if path.trim() != settings::shell() {
                ctrl.set_shell(&path);
            }
        }
    }

    /// The theme changed under an open panel. Only the Toolbar pane's band preview cares: it is
    /// painted in the theme's colors, and unlike the terminal's own views nothing invalidates it
    /// when the theme moves.
    pub fn theme_changed(&self) {
        if let Some(e) = self.ivars().toolbar_editor.borrow().as_ref() {
            unsafe { e.setNeedsDisplay(true) };
        }
    }

    /// Re-read the current settings into the controls.
    fn seed_values(&self) {
        let ctrl = match self.controller() {
            Some(c) => c,
            None => return,
        };
        let store = self.ivars();
        if let Some(g) = store.theme_grid.borrow().as_ref() {
            g.set_selected(ctrl.snapshot().style);
        }
        if let Some(pop) = store.fam_pop.borrow().as_ref() {
            let cur = settings::family();
            if let Some(i) = settings::FAMILIES.iter().position(|f| *f == cur) {
                unsafe { pop.selectItemAtIndex(i as isize) };
            }
        }
        let size = settings::size();
        if let Some(s) = store.size_slider.borrow().as_ref() {
            unsafe { s.setDoubleValue(size) };
        }
        self.set_value_label(&store.size_value, size);
        if let Some(p) = store.side_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(if ctrl.sidebar_on_right() { 1 } else { 0 }) };
        }
        if let Some(e) = store.toolbar_editor.borrow().as_ref() {
            e.reload();
        }
        let pad = settings::pad();
        if let Some(s) = store.pad_slider.borrow().as_ref() {
            unsafe { s.setDoubleValue(pad) };
        }
        self.set_value_label(&store.pad_value, pad);
        if let Some(p) = store.cursor_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(settings::cursor_shape().index() as isize) };
        }
        if let Some(p) = store.scrollback_pop.borrow().as_ref() {
            // The list was built to contain the current value, so a miss can only mean the setting
            // changed behind the dialog's back; leaving the selection alone is the honest response.
            if let Some(i) = store.scrollback_items.borrow().iter().position(|n| *n == settings::scrollback()) {
                unsafe { p.selectItemAtIndex(i as isize) };
            }
        }
        if let Some(f) = store.shell_field.borrow().as_ref() {
            unsafe { f.setStringValue(&NSString::from_str(&settings::shell())) };
        }
        if let Some(p) = store.new_tab_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(settings::new_tab_dir().index() as isize) };
        }
        if let Some(p) = store.blink_pop.borrow().as_ref() {
            unsafe { p.selectItemAtIndex(if settings::cursor_blink() { 1 } else { 0 }) };
        }
        if let Some(p) = store.opacity_pop.borrow().as_ref() {
            // Percentages are what the pop-up offers, so round to the nearest one rather than
            // requiring an exact float match on a value that came back through a config file.
            let pct = (settings::opacity() * 100.0).round() as u32;
            if let Some(i) = OPACITY_PERCENTS.iter().position(|p| *p == pct) {
                unsafe { p.selectItemAtIndex(i as isize) };
            }
        }
    }
}

/// The scrollback pop-up's values: the presets, plus `current` if a hand-edited config put it
/// somewhere between them (so the pop-up can show what is actually in force).
fn scrollback_items(current: usize) -> Vec<usize> {
    let mut items = SCROLLBACK_PRESETS.to_vec();
    if !items.contains(&current) {
        items.push(current);
        items.sort_unstable();
    }
    items
}

/// Make an editable field commit when editing *ends*, not only on Return.
///
/// Without this a field's action fires on Return alone, so typing a value and then closing the
/// panel — or just clicking another control — silently throws it away, while every pop-up next to
/// it applies instantly. Closing the window ends editing (`NSWindow` calls `endEditingFor:`), so
/// this is what makes the obvious gesture work. The flag lives on the cell, not the field.
fn commit_on_end_editing(field: &NSTextField) {
    unsafe {
        let cell: Retained<AnyObject> = msg_send_id![field, cell];
        let _: () = msg_send![&cell, setSendsActionOnEndEditing: true];
    }
}

/// An empty pane view, sized to the tab view's content area.
fn new_pane(pane_w: f64, mtm: MainThreadMarker) -> Retained<NSView> {
    unsafe {
        msg_send_id![mtm.alloc::<NSView>(), initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0), NSSize::new(pane_w, PANE_H))]
    }
}

/// Append one titled tab holding `pane`.
unsafe fn add_pane(tabs: &NSTabView, title: &str, pane: Retained<NSView>, mtm: MainThreadMarker) {
    let item: Retained<NSTabViewItem> = msg_send_id![mtm.alloc::<NSTabViewItem>(), initWithIdentifier: std::ptr::null::<objc2::runtime::AnyObject>()];
    item.setLabel(&NSString::from_str(title));
    item.setView(Some(&pane));
    tabs.addTabViewItem(&item);
}

/// A non-editable, borderless label added to `pane` at `y`.
fn add_label(pane: &NSView, text: &str, y: f64, mtm: MainThreadMarker) {
    let label = unsafe { NSTextField::labelWithString(&NSString::from_str(text), mtm) };
    unsafe {
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        label.setFrame(NSRect::new(NSPoint::new(LABEL_X, y - 1.0), NSSize::new(LABEL_W, 18.0)));
        pane.addSubview(&label);
    }
}

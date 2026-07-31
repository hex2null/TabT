//! Menu bar + shortcuts.
//!
//! App-level actions (new/close tab, collapse sidebar, reveal in Finder, clear screen) are received by a small
//! ObjC object [`MenuTarget`] and forwarded to [`AppController`]. Copy/paste/select-all go through the
//! standard `copy:`/`paste:`/`selectAll:`; with target=nil they travel the responder chain down to the first responder
//! (i.e. `TermView`). About/Quit go through NSApplication's built-in selectors.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSApplicationTerminateReply, NSEventModifierFlags, NSMenu, NSMenuItem,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSString};

use crate::app::AppController;

pub struct MenuTargetIvars {
    controller: Cell<*const AppController>,
}

declare_class!(
    pub struct MenuTarget;

    unsafe impl ClassType for MenuTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "MenuTarget";
    }

    impl DeclaredClass for MenuTarget {
        type Ivars = MenuTargetIvars;
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl MenuTarget {
        // NSMenuDelegate. Recent macOS decorates menu items whose action is a standard selector
        // (Copy, Paste, Select All, About, Quit …) with an SF Symbol, and injects its own
        // AutoFill / Dictation / Emoji items into any menu titled "Edit" — all of them carrying
        // icons. Our menus are short and keyboard-driven, so only some items get one and the icon
        // column lands half-filled and ragged.
        //
        // The images are applied when the menu is about to be shown, so clearing them at build
        // time does not stick; this hook runs late enough to win, and runs again on every display.
        #[method(menuNeedsUpdate:)]
        fn menu_needs_update(&self, menu: &NSMenu) {
            unsafe {
                for i in 0..menu.numberOfItems() {
                    menu.itemAtIndex(i).map(|item| item.setImage(None));
                }
            }
        }

        #[method(newTerminal:)]
        fn new_terminal(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.add_tab_default());
        }
        #[method(newGroup:)]
        fn new_group(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.add_group_default());
        }
        #[method(closeTab:)]
        fn close_tab(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.close_active_tab());
        }
        #[method(toggleSidebar:)]
        fn toggle_sidebar(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.toggle_sidebar());
        }

        // The toolbar's sidebar button (see `toolbar.rs`) targets this object. Its one action is
        // always available — there is no state in which collapsing or expanding is meaningless —
        // so validation is a plain yes rather than the default per-selector check.
        #[method(validateToolbarItem:)]
        fn validate_toolbar_item(&self, _item: Option<&AnyObject>) -> bool {
            true
        }
        #[method(renameSession:)]
        fn rename_session(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.rename_active_tab());
        }
        #[method(lastSession:)]
        fn last_session(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.select_recent_tab());
        }
        #[method(takeScreenshot:)]
        fn take_screenshot(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.open_screenshot_ui());
        }
        #[method(clearLine:)]
        fn clear_line(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.clear_line_active());
        }
        #[method(goHome:)]
        fn go_home(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("cd ~"));
        }
        #[method(runClaude:)]
        fn run_claude(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("claude"));
        }
        #[method(runCodex:)]
        fn run_codex(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("codex"));
        }
        #[method(runGemini:)]
        fn run_gemini(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("gemini"));
        }
        #[method(runAider:)]
        fn run_aider(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("aider"));
        }
        #[method(runCursor:)]
        fn run_cursor(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.run_in_active("cursor-agent"));
        }
        #[method(interruptSession:)]
        fn interrupt_session(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.interrupt_active());
        }
        #[method(restartSession:)]
        fn restart_session(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.restart_active_tab());
        }
        #[method(exportText:)]
        fn export_text(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.export_active_text());
        }
        #[method(revealInFinder:)]
        fn reveal_in_finder(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.reveal_in_finder());
        }
        #[method(clearScreen:)]
        fn clear_screen(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.clear_active());
        }
        #[method(increaseFontSize:)]
        fn increase_font(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.change_font_size(1.0));
        }
        #[method(decreaseFontSize:)]
        fn decrease_font(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.change_font_size(-1.0));
        }
        #[method(resetFontSize:)]
        fn reset_font(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.reset_font_size());
        }
        #[method(findSession:)]
        fn find_session(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.focus_search());
        }
        #[method(openSettings:)]
        fn open_settings(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.open_settings());
        }
        #[method(sidebarMenu:)]
        fn sidebar_menu(&self, _s: Option<&AnyObject>) {
            self.with(|c| c.open_sidebar_menu());
        }

        // As the app delegate: before quitting, flush layout + session state to disk (cwd + content snapshot of each tab).
        #[method(applicationWillTerminate:)]
        fn app_will_terminate(&self, _n: Option<&AnyObject>) {
            self.with(|c| c.persist());
        }

        // As the app delegate: ⌘Q / Quit — confirm first if a terminal has a foreground job running.
        #[method(applicationShouldTerminate:)]
        fn app_should_terminate(&self, _s: Option<&AnyObject>) -> NSApplicationTerminateReply {
            let p = self.ivars().controller.get();
            let proceed = p.is_null() || unsafe { &*p }.confirm_quit();
            if proceed { NSApplicationTerminateReply::NSTerminateNow } else { NSApplicationTerminateReply::NSTerminateCancel }
        }

        // As the window delegate: re-center the traffic lights after macOS relays them out on resize.
        #[method(windowDidResize:)]
        fn window_did_resize(&self, _n: Option<&AnyObject>) {
            self.with(|c| c.sync_top_strip());
        }

        // Cold start: the one-shot call in main runs before AppKit finalizes the button layout, which
        // then resets them. Re-apply once the window is shown/keyed so they land centered on first launch.
        #[method(windowDidBecomeKey:)]
        fn window_did_become_key(&self, _n: Option<&AnyObject>) {
            self.with(|c| c.sync_top_strip());
        }

        #[method(windowDidExpose:)]
        fn window_did_expose(&self, _n: Option<&AnyObject>) {
            self.with(|c| c.sync_top_strip());
        }
    }
);

impl MenuTarget {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(MenuTargetIvars { controller: Cell::new(std::ptr::null()) });
        unsafe { msg_send_id![super(this), init] }
    }

    pub fn set_controller(&self, c: *const AppController) {
        self.ivars().controller.set(c);
    }

    fn with(&self, f: impl FnOnce(&AppController)) {
        let p = self.ivars().controller.get();
        if !p.is_null() {
            f(unsafe { &*p });
        }
    }
}

/// Adds one item to the menu. `target=None` → travels the responder chain; `shift=true` → shortcut adds ⇧.
fn add(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &str,
    action: Option<Sel>,
    target: Option<&MenuTarget>,
    key: &str,
    shift: bool,
) {
    let mods = shift.then(|| {
        NSEventModifierFlags::NSEventModifierFlagCommand
            | NSEventModifierFlags::NSEventModifierFlagShift
    });
    add_mods(mtm, menu, title, action, target, key, mods);
}

/// Like [`add`], but with an explicit modifier mask (`None` keeps AppKit's ⌘ default) — needed for
/// the few shortcuts that aren't ⌘-based, e.g. ⌃↩.
fn add_mods(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &str,
    action: Option<Sel>,
    target: Option<&MenuTarget>,
    key: &str,
    mods: Option<NSEventModifierFlags>,
) {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    if let Some(t) = target {
        unsafe {
            let _: () = msg_send![&item, setTarget: t];
        }
    }
    if let Some(m) = mods {
        item.setKeyEquivalentModifierMask(m);
    }
    // Recent macOS decorates items whose action is a standard selector (About, Quit, Copy, Paste,
    // Select All, …) with an SF Symbol of its own choosing. Ours is a keyboard-driven terminal
    // menu where only some items would get one, so the column of icons lands half-filled and
    // ragged; clearing it keeps every menu a plain list of titles.
    unsafe { item.setImage(None) };
    menu.addItem(&item);
}

/// Creates a top-level submenu and attaches it to the menu bar, returning the submenu so more items can be added.
fn submenu(mtm: MainThreadMarker, menubar: &NSMenu, title: &str, target: &MenuTarget) -> Retained<NSMenu> {
    let item = NSMenuItem::new(mtm);
    menubar.addItem(&item);
    let menu = NSMenu::new(mtm);
    unsafe { menu.setTitle(&NSString::from_str(title)) };
    // See `menuNeedsUpdate:` — the delegate strips the system's automatic item icons.
    unsafe {
        let _: () = msg_send![&menu, setDelegate: target];
    }
    item.setSubmenu(Some(&menu));
    menu
}

/// Builds the complete menu bar. `target` must stay alive while the app runs (menu items hold a weak reference to target).
pub fn build_menu(mtm: MainThreadMarker, app: &NSApplication, target: &MenuTarget) {
    let menubar = NSMenu::new(mtm);

    // ---- App menu (the title is shown by the system as the app name) ----
    let name = crate::branding::APP_NAME;
    let app_menu = submenu(mtm, &menubar, name, target);
    add(mtm, &app_menu, &format!("About {name}"), Some(sel!(orderFrontStandardAboutPanel:)), None, "", false);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(mtm, &app_menu, "Settings…", Some(sel!(openSettings:)), Some(target), ",", false);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add(mtm, &app_menu, &format!("Quit {name}"), Some(sel!(terminate:)), None, "q", false);

    // ---- Shell ----
    let shell = submenu(mtm, &menubar, "Shell", target);
    add(mtm, &shell, "New Terminal", Some(sel!(newTerminal:)), Some(target), "t", false);
    add(mtm, &shell, "New Group", Some(sel!(newGroup:)), Some(target), "n", true);
    shell.addItem(&NSMenuItem::separatorItem(mtm));
    add(mtm, &shell, "Rename Session", Some(sel!(renameSession:)), Some(target), "r", false);
    // A fresh shell in the same tab and the same directory. Reachable from the keyboard as Enter on
    // an *ended* session, but not otherwise, so the toolbar's button needs a menu item beside it.
    add(mtm, &shell, "Restart Session", Some(sel!(restartSession:)), Some(target), "", false);
    add(mtm, &shell, "Reveal in Finder", Some(sel!(revealInFinder:)), Some(target), "r", true);
    // ⇧⌘S, the system's own "save a copy of this" shortcut.
    add(mtm, &shell, "Export Text…", Some(sel!(exportText:)), Some(target), "s", true);
    shell.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘~ (that is, ⇧⌘` — the same physical key): flip back to the session you came from.
    add(mtm, &shell, "Last Session", Some(sel!(lastSession:)), Some(target), "`", true);
    add(mtm, &shell, "Close Tab", Some(sel!(closeTab:)), Some(target), "w", false);

    // ---- Edit (target=nil → first responder TermView) ----
    let edit = submenu(mtm, &menubar, "Edit", target);
    add(mtm, &edit, "Copy", Some(sel!(copy:)), None, "c", false);
    add(mtm, &edit, "Paste", Some(sel!(paste:)), None, "v", false);
    add(mtm, &edit, "Select All", Some(sel!(selectAll:)), None, "a", false);
    edit.addItem(&NSMenuItem::separatorItem(mtm));
    add(mtm, &edit, "Find", Some(sel!(findSession:)), Some(target), "f", false);

    // ---- View (color/font and other settings have been merged into the sidebar's bottom "Settings"; only zoom shortcuts remain here) ----
    let view = submenu(mtm, &menubar, "View", target);
    add(mtm, &view, "Toggle Sidebar", Some(sel!(toggleSidebar:)), Some(target), "b", false);
    // ⌃↩ — opens the context menu of the hovered sidebar row, else the active tab's.
    add_mods(
        mtm,
        &view,
        "Sidebar Context Menu",
        Some(sel!(sidebarMenu:)),
        Some(target),
        "\r",
        Some(NSEventModifierFlags::NSEventModifierFlagControl),
    );
    add(mtm, &view, "Clear", Some(sel!(clearScreen:)), Some(target), "k", false);
    view.addItem(&NSMenuItem::separatorItem(mtm));
    // ⌘= (no Shift) to zoom in, ⌘- to zoom out — the bare-key form, no need to reach for Shift.
    add(mtm, &view, "Increase Font Size", Some(sel!(increaseFontSize:)), Some(target), "=", false);
    add(mtm, &view, "Decrease Font Size", Some(sel!(decreaseFontSize:)), Some(target), "-", false);
    add(mtm, &view, "Actual Size", Some(sel!(resetFontSize:)), Some(target), "0", false); // ⌘0

    app.setMainMenu(Some(&menubar));
}

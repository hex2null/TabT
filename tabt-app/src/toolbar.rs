//! The window's `NSToolbar`: the sidebar toggle, as a standard toolbar button.
//!
//! The window's title bar is transparent and full-size, so the toolbar contributes no background of
//! its own — the band is still painted by [`crate::header::HeaderView`] in the theme's color, and
//! the toolbar puts two items on top of it: ordinary image `NSToolbarItem`s carrying the system's
//! `sidebar.left` and `magnifyingglass` symbols, bordered, so they draw and highlight exactly like
//! the show/hide-sidebar button in Finder or Mail. Their actions are `toggleSidebar:` and
//! `findSession:` on `MenuTarget` — the same selectors the View menu's items send, so the two
//! routes cannot drift apart. They are the collapsed state's stand-ins for the pair the sidebar
//! card carries on its own top strip (`toggle.rs`).
//!
//! **The items are only in the toolbar while the sidebar is collapsed** ([`Toolbar::set_shown`]).
//! Expanded, the sidebar carries its own pair at the card's top-right (`toggle.rs`) and these would
//! be a second copy of them a few points away. Showing and hiding means removing and re-inserting
//! the items: `NSToolbarItem` has no setter for `isVisible`, and this happens once per collapse,
//! not per frame.
//!
//! It is deliberately *not* `NSToolbarToggleSidebarItemIdentifier`. That reserved item only works
//! in a window whose content view controller is an `NSSplitViewController` with a sidebar item;
//! with this app's floating card AppKit finds no target for it, and ships it permanently disabled —
//! greyed out and unclickable, with `validateToolbarItem:` never even asked.
//!
//! The item sits where the toolbar puts it: at the leading edge, just right of the traffic lights —
//! over the sidebar card when the sidebar is showing, and immediately left of the session title
//! when it is collapsed.
//!
//! **The session title is deliberately not an item here.** It has to start at the *terminal's* left
//! edge, which moves with the sidebar, and AppKit measures a custom toolbar item view exactly once,
//! when the item is inserted: `minSize`/`maxSize` changes, `invalidateIntrinsicContentSize` and
//! `setNeedsLayout` up the item's superview chain all leave the item where it was first placed. A
//! title item could therefore be positioned, but never re-positioned. `HeaderView` spans exactly
//! the terminal pane and gets that alignment for free. (Sizing a custom item view is also
//! `setMinSize`/`setMaxSize` or nothing — with constraints or an `intrinsicContentSize` alone the
//! item reports itself visible, its frame looks right, and `drawRect:` is simply never called.)

// The `NSToolbarDelegate` methods have to carry objc2's names for the selectors they implement.
#![allow(non_snake_case)]

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::runtime::Sel;
use objc2::{declare_class, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSCompositingOperation, NSFontWeightRegular, NSImage, NSImageSymbolConfiguration,
    NSImageSymbolScale, NSMenu, NSMenuItem, NSMenuToolbarItem, NSRectFillUsingOperation, NSToolbar,
    NSToolbarDelegate, NSToolbarDisplayMode, NSToolbarItem, NSToolbarItemIdentifier, NSWindow,
    NSWindowToolbarStyle,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSBundle, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::settings;
use crate::theme;
use crate::view::ns_color;

/// How far the icons are blended from the theme's foreground toward its background. The system's
/// own tint is a full-strength label color, which on a themed band reads as louder than everything
/// around it — these are always-available actions, not the content.
pub const ICON_DIM: f64 = 0.45;
/// Point size of the icons. The system's default for a toolbar item is bigger, and next to the
/// card's own strip buttons — which draw a 17pt glyph box at `draw_symbol`'s 0.92 fill — it reads
/// as a different control. This is that same glyph, to the point.
const ICON_PT: f64 = 17.0 * 0.92;

const TOGGLE_ID: &str = "dev.local.tabt.toolbar.toggle";
const SEARCH_ID: &str = "dev.local.tabt.toolbar.search";
const COPY_ID: &str = "dev.local.tabt.toolbar.copy";
const PASTE_ID: &str = "dev.local.tabt.toolbar.paste";
const CLEAR_LINE_ID: &str = "dev.local.tabt.toolbar.clearline";
const CLEAR_ID: &str = "dev.local.tabt.toolbar.clear";
const SHOT_ID: &str = "dev.local.tabt.toolbar.screenshot";
const SHARE_ID: &str = "dev.local.tabt.toolbar.share";
const HOME_ID: &str = "dev.local.tabt.toolbar.home";
const CLAUDE_ID: &str = "dev.local.tabt.toolbar.claude";
const CODEX_ID: &str = "dev.local.tabt.toolbar.codex";
/// AppKit's own item, which eats whatever width is left — it is what pins the session actions to
/// the trailing end while the leading pair stays by the traffic lights.
const FLEX_ID: &str = "NSToolbarFlexibleSpaceItem";
/// The buttons Settings → Toolbar can turn off, in the order they appear. The short key is what
/// `layout.conf` stores; the label is what the checkbox says.
pub const CUSTOMIZABLE: [(&str, &str, &str); 9] = [
    ("claude", CLAUDE_ID, "Claude"),
    ("codex", CODEX_ID, "Codex"),
    ("home", HOME_ID, "Home (cd ~)"),
    ("copy", COPY_ID, "Copy"),
    ("paste", PASTE_ID, "Paste"),
    ("clearline", CLEAR_LINE_ID, "Clear Line"),
    ("clear", CLEAR_ID, "Clear Screen"),
    ("screenshot", SHOT_ID, "Screenshot"),
    ("share", SHARE_ID, "Share"),
];

/// AppKit's fixed-width space. Recent macOS draws a run of adjacent items in one capsule, so this
/// is what splits that run: the two launchers get a capsule of their own, apart from the actions
/// that operate on the session already in front of you.
const SPACE_ID: &str = "NSToolbarSpaceItem";

pub struct DelegateIvars {
    items: Vec<(String, Retained<NSToolbarItem>)>,
}

declare_class!(
    /// Supplies the item list. The toolbar is not user-customizable, so the "allowed" and "default"
    /// sets are the same one identifier.
    pub struct ToolbarDelegate;

    unsafe impl ClassType for ToolbarDelegate {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "TabTToolbarDelegate";
    }

    impl DeclaredClass for ToolbarDelegate {
        type Ivars = DelegateIvars;
    }

    unsafe impl NSObjectProtocol for ToolbarDelegate {}

    unsafe impl NSToolbarDelegate for ToolbarDelegate {
        #[method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:)]
        unsafe fn toolbar_itemForItemIdentifier_willBeInsertedIntoToolbar(
            &self,
            _toolbar: &NSToolbar,
            item_identifier: &NSToolbarItemIdentifier,
            _flag: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            // Built once and handed back on every request, so the controller can keep pointing the
            // same items at the menu target after the fact. Anything not ours — the flexible space —
            // is AppKit's to build.
            let id = item_identifier.to_string();
            self.ivars().items.iter().find(|(k, _)| *k == id).map(|(_, it)| it.clone())
        }

        #[method_id(toolbarDefaultItemIdentifiers:)]
        unsafe fn toolbarDefaultItemIdentifiers(
            &self,
            _toolbar: &NSToolbar,
        ) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            Self::identifier_array(true)
        }

        #[method_id(toolbarAllowedItemIdentifiers:)]
        unsafe fn toolbarAllowedItemIdentifiers(
            &self,
            _toolbar: &NSToolbar,
        ) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            Self::identifier_array(true)
        }
    }
);

impl ToolbarDelegate {
    /// The item list for a given sidebar state. The session actions are always there, pushed to the
    /// trailing end by the flexible space; the sidebar pair only joins them while the sidebar is
    /// collapsed and its own strip is off screen (see the module docs).
    ///
    /// Search first, then collapse — the same order and the same two icons the card's own strip
    /// carries when the sidebar is showing (`toggle.rs`), so the pair does not appear to swap places
    /// as the sidebar goes away and comes back.
    fn identifiers(with_sidebar_pair: bool) -> Vec<&'static str> {
        let mut ids = Vec::new();
        if with_sidebar_pair {
            ids.extend_from_slice(&[SEARCH_ID, TOGGLE_ID]);
        }
        ids.push(FLEX_ID);
        // Two capsules: the launchers, then the actions on the session already in front of you.
        // Whatever the user has switched off in Settings → Toolbar drops out here, and a group that
        // ends up empty takes its separator with it — a lone space would leave a gap with nothing
        // on one side of it.
        let on = |k: &str| settings::toolbar_shows(k);
        let launchers: Vec<&'static str> = CUSTOMIZABLE[0..2].iter().filter(|(k, ..)| on(k)).map(|(_, id, _)| *id).collect();
        let actions: Vec<&'static str> = CUSTOMIZABLE[2..].iter().filter(|(k, ..)| on(k)).map(|(_, id, _)| *id).collect();
        ids.extend_from_slice(&launchers);
        if !launchers.is_empty() && !actions.is_empty() {
            ids.push(SPACE_ID);
        }
        ids.extend_from_slice(&actions);
        ids
    }

    fn identifier_array(with_sidebar_pair: bool) -> Retained<NSArray<NSToolbarItemIdentifier>> {
        NSArray::from_vec(Self::identifiers(with_sidebar_pair).into_iter().map(NSString::from_str).collect())
    }
}

/// One of the two buttons: a system symbol in a bordered toolbar item, which is what gives it the
/// standard size, hover highlight and pressed state.
///
/// The action is the selector the matching View-menu item already sends, so the menu and the button
/// cannot drift. The target is set once the menu target exists (see [`Toolbar::set_target`]); until
/// then the item validates against the responder chain and simply stays disabled.
fn item(mtm: MainThreadMarker, id: &str, symbol: &str, label: &str, tip: &str, action: Sel) -> Retained<NSToolbarItem> {
    unsafe {
        let item = NSToolbarItem::initWithItemIdentifier(mtm.alloc(), &NSString::from_str(id));
        let label = NSString::from_str(label);
        set_symbol(&item, symbol, &label);
        item.setLabel(&label);
        item.setPaletteLabel(&label);
        item.setToolTip(Some(&NSString::from_str(tip)));
        item.setBordered(true);
        item.setAction(Some(action));
        item
    }
}

/// Put `symbol` on `item`, tinted from the current theme.
///
/// A template image would be re-tinted by AppKit in its own control color, so the symbol is baked
/// with a hierarchical color configuration and handed over as a plain image instead — the same
/// mechanism `view::draw_symbol` uses for the self-drawn icons, so the toolbar and the card's own
/// strip end up the same tone. It has to be re-applied on every theme change ([`Toolbar::apply_theme`]).
fn set_symbol(item: &NSToolbarItem, symbol: &str, label: &NSString) {
    let tone = { let t = theme::current(); theme::mix(t.fg, t.bg, ICON_DIM) };
    unsafe {
        // A bundled artwork of that name wins — that is how `claude` and `openai` get their own
        // marks — and everything else is a system symbol. Both end up tinted to the same tone at
        // the same size, so a brand glyph sits in the row as one more icon rather than as a logo.
        let colored = match bundled_image(symbol) {
            Some(img) => tinted(&img, tone),
            None => {
                let Some(img) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str(symbol),
                    Some(label),
                ) else {
                    return;
                };
                let color_cfg = NSImageSymbolConfiguration::configurationWithHierarchicalColor(&ns_color(tone));
                let size_cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight_scale(
                    ICON_PT,
                    NSFontWeightRegular,
                    NSImageSymbolScale::Medium,
                );
                let cfg = size_cfg.configurationByApplyingConfiguration(&color_cfg);
                img.imageWithSymbolConfiguration(&cfg).unwrap_or(img)
            }
        };
        colored.setTemplate(false);
        item.setImage(Some(&colored));
    }
}

/// A PNG shipped in `Contents/Resources`, or None — an unbundled `cargo build` has no resources at
/// all, and the caller falls back to a system symbol there (see `set_symbol`).
unsafe fn bundled_image(name: &str) -> Option<Retained<NSImage>> {
    let path = NSBundle::mainBundle()
        .pathForResource_ofType(Some(&NSString::from_str(name)), Some(&NSString::from_str("png")))?;
    NSImage::initWithContentsOfFile(NSImage::alloc(), &path)
}

/// Redraw `img` in one flat color at the icon size, keeping its alpha.
///
/// The artwork is a black glyph on transparency, so this is the same result the symbols get from a
/// hierarchical color configuration: `sourceAtop` paints the color over everything the glyph
/// covers and leaves the transparent parts alone. Marking it a template instead would hand the
/// tinting to AppKit, which would use its own control color rather than the theme's.
#[allow(deprecated)] // lockFocus/unlockFocus: a 16pt icon rebuilt on a theme change, not per frame
unsafe fn tinted(img: &NSImage, tone: (f64, f64, f64)) -> Retained<NSImage> {
    let size = NSSize::new(ICON_PT, ICON_PT);
    let out = NSImage::initWithSize(NSImage::alloc(), size);
    let r = NSRect::new(NSPoint::new(0.0, 0.0), size);
    out.lockFocus();
    img.drawInRect_fromRect_operation_fraction(r, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)), NSCompositingOperation::SourceOver, 1.0);
    ns_color(tone).set();
    NSRectFillUsingOperation(r, NSCompositingOperation::SourceAtop);
    out.unlockFocus();
    out
}

/// Symbol for the share item, kept out of `spec` because that table is keyed by action and this
/// item has a menu instead — but `apply_theme` still has to know what to re-tint.
const SHARE_SYMBOL: &str = "square.and.arrow.up";

/// The share item: one button that drops a menu rather than firing an action, holding the two ways
/// a session leaves the app — its text to a file, its directory to Finder.
///
/// An `NSMenuToolbarItem` rather than a plain item that pops a menu by hand: the class exists for
/// exactly this, and it draws the small chevron that says a click opens something.
fn share_item(mtm: MainThreadMarker) -> (Retained<NSToolbarItem>, Retained<NSMenu>) {
    unsafe {
        let item = NSMenuToolbarItem::initWithItemIdentifier(mtm.alloc(), &NSString::from_str(SHARE_ID));
        let label = NSString::from_str("Share");
        item.setLabel(&label);
        item.setPaletteLabel(&label);
        item.setToolTip(Some(&NSString::from_str("Export or reveal this session")));
        item.setBordered(true);
        // No chevron: the row is a set of plain icon buttons, and the one arrow next to the share
        // symbol reads as part of the glyph rather than as an affordance.
        item.setShowsIndicator(false);
        // The menu's items carry no target here; `Toolbar::set_target` points them at the menu
        // target with everything else, once `main` has built it.
        let menu = NSMenu::new(mtm);
        for (title, action) in [
            ("Export Text…", sel!(exportText:)),
            ("Reveal in Finder", sel!(revealInFinder:)),
        ] {
            let mi = NSMenuItem::new(mtm);
            mi.setTitle(&NSString::from_str(title));
            mi.setAction(Some(action));
            menu.addItem(&mi);
        }
        item.setMenu(&menu);
        let up: Retained<NSToolbarItem> = Retained::into_super(item);
        set_symbol(&up, SHARE_SYMBOL, &label);
        (up, menu)
    }
}

/// Owns the toolbar and its delegate. Plain Rust, held by the controller: AppKit retains the
/// toolbar through the window, but nothing retains the delegate for us.
pub struct Toolbar {
    toolbar: Retained<NSToolbar>,
    _delegate: Retained<ToolbarDelegate>,
    /// Every item this toolbar owns with the symbol it draws — kept so `set_target` can point them
    /// all at the menu target once it exists, and so `apply_theme` can re-tint them.
    items: Vec<(Retained<NSToolbarItem>, &'static str)>,
    /// The share button's drop-down. Held because its items need the menu target pointing into
    /// them, and that object does not exist until after the toolbar is built.
    share_menu: Retained<NSMenu>,
    /// Whether the sidebar pair is currently in the toolbar (i.e. the sidebar is collapsed).
    shown: Cell<bool>,
}

impl Toolbar {
    /// Build the toolbar and attach it to `window`.
    pub fn attach(mtm: MainThreadMarker, window: &NSWindow) -> Self {
        // The two sidebar buttons target the menu target explicitly (see `set_target`); the four
        // session actions are the terminal's own, so their actions travel the responder chain to
        // the mounted `TermView` exactly as the Edit menu's items do — `copy:`/`paste:` are its
        // methods, and clear/reveal are the controller's, reached through the same menu target.
        let spec: [(&'static str, &'static str, &str, &str, Sel); 10] = [
            (TOGGLE_ID, "sidebar.left", "Sidebar", "Show Sidebar (⌘B)", sel!(toggleSidebar:)),
            (SEARCH_ID, "magnifyingglass", "Search", "Search Sessions (⌘F)", sel!(findSession:)),
            (HOME_ID, "house", "Home", "cd ~", sel!(goHome:)),
            (COPY_ID, "doc.on.doc", "Copy", "Copy (⌘C)", sel!(copy:)),
            (PASTE_ID, "doc.on.clipboard", "Paste", "Paste (⌘V)", sel!(paste:)),
            (CLEAR_LINE_ID, "delete.left", "Clear Line", "Clear Line (⌃U)", sel!(clearLine:)),
            (CLEAR_ID, "eraser", "Clear", "Clear Screen (⌃L)", sel!(clearScreen:)),
            (SHOT_ID, "camera.viewfinder", "Screenshot", "Screenshot (⇧⌘5)", sel!(takeScreenshot:)),
            // The two launchers type their command into the session and press Return, which is all
            // "run claude here" means — the shell resolves it on $PATH exactly as the user would.
            (CLAUDE_ID, "claude", "Claude", "Run claude in this session", sel!(runClaude:)),
            (CODEX_ID, "openai", "Codex", "Run codex in this session", sel!(runCodex:)),
        ];
        let mut built: Vec<(&str, Retained<NSToolbarItem>)> = spec
            .iter()
            .map(|(id, sym, label, tip, action)| (*id, item(mtm, id, sym, label, tip, *action)))
            .collect();
        let (share, share_menu) = share_item(mtm);
        built.push((SHARE_ID, share));
        let symbols = spec.iter().map(|(_, sym, ..)| *sym).chain(std::iter::once(SHARE_SYMBOL));
        let items: Vec<(Retained<NSToolbarItem>, &'static str)> =
            built.iter().zip(symbols).map(|((_, it), sym)| (it.clone(), sym)).collect();
        let delegate: Retained<ToolbarDelegate> = {
            let this = mtm.alloc();
            let this = this.set_ivars(DelegateIvars {
                items: built.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            });
            unsafe { msg_send_id![super(this), init] }
        };
        let toolbar = unsafe {
            let tb = NSToolbar::initWithIdentifier(mtm.alloc(), &NSString::from_str("dev.local.tabt.toolbar"));
            tb.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            tb.setAllowsUserCustomization(false);
            tb.setAllowsExtensionItems(false);
            tb.setDisplayMode(NSToolbarDisplayMode::IconOnly);
            // The band is the terminal's own background; a baseline separator would draw a system
            // hairline across it that no theme asked for.
            tb.setShowsBaselineSeparator(false);
            tb
        };
        unsafe {
            // Unified: one band, the item on the same row as the traffic lights. The window's title
            // is hidden and its title bar transparent, so none of the system's own chrome shows —
            // only the item.
            window.setToolbarStyle(NSWindowToolbarStyle::Unified);
            window.setToolbar(Some(&toolbar));
        }
        Toolbar { toolbar, _delegate: delegate, items, share_menu, shown: Cell::new(true) }
    }

    /// Re-tint every icon after a theme change. Unlike the `drawRect:` views in the content view,
    /// a toolbar item holds a concrete image and cannot re-read the theme on its own.
    pub fn apply_theme(&self) {
        for (it, symbol) in &self.items {
            let label = unsafe { it.label() };
            set_symbol(it, symbol, &label);
        }
    }

    /// Put the button in the toolbar, or take it out again. See the module docs: expanded, the
    /// card's own button is the one on screen, and two of them would sit a few points apart.
    pub fn set_shown(&self, shown: bool) {
        if self.shown.get() == shown {
            return;
        }
        self.shown.set(shown);
        self.rebuild();
    }

    /// Re-lay the toolbar from the current state and settings. Removing and re-inserting is the
    /// whole mechanism: `NSToolbarItem` has no setter for `isVisible`, and this runs once per
    /// collapse or settings change, not per frame.
    pub fn rebuild(&self) {
        unsafe {
            while self.toolbar.items().count() > 0 {
                self.toolbar.removeItemAtIndex(0);
            }
            for (i, id) in ToolbarDelegate::identifiers(self.shown.get()).into_iter().enumerate() {
                self.toolbar.insertItemWithItemIdentifier_atIndex(&NSString::from_str(id), i as isize);
            }
        }
    }

    /// Point the toggle at the object that implements `toggleSidebar:`. Explicit rather than left to
    /// the responder chain: `MenuTarget` is only the window's delegate, and an item whose target
    /// cannot be resolved is drawn disabled.
    /// Point the app's own actions at the object that implements them. Explicit rather than left to
    /// the responder chain: `MenuTarget` is only the window's delegate, and an item whose target
    /// cannot be resolved is drawn disabled.
    ///
    /// `copy:` and `paste:` are deliberately left targetless — those belong to whichever `TermView`
    /// has the keyboard, exactly as the Edit menu's items do, and a fixed target would send them to
    /// an object that does not implement them.
    pub fn set_target(&self, target: &AnyObject) {
        for (it, _) in &self.items {
            let action = unsafe { it.action() };
            if action == Some(sel!(copy:)) || action == Some(sel!(paste:)) {
                continue;
            }
            unsafe { it.setTarget(Some(target)) };
        }
        // The share item has no action of its own; its menu's items carry them.
        unsafe {
            for i in 0..self.share_menu.numberOfItems() {
                if let Some(mi) = self.share_menu.itemAtIndex(i) {
                    mi.setTarget(Some(target));
                }
            }
        }
    }
}

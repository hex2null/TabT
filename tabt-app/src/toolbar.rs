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
    NSImageSymbolScale, NSRectFillUsingOperation, NSToolbar,
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
const HOME_ID: &str = "dev.local.tabt.toolbar.home";
const CLAUDE_ID: &str = "dev.local.tabt.toolbar.claude";
const CODEX_ID: &str = "dev.local.tabt.toolbar.codex";
const GEMINI_ID: &str = "dev.local.tabt.toolbar.gemini";
const AIDER_ID: &str = "dev.local.tabt.toolbar.aider";
const CURSOR_ID: &str = "dev.local.tabt.toolbar.cursor";
const INTERRUPT_ID: &str = "dev.local.tabt.toolbar.interrupt";
const RESTART_ID: &str = "dev.local.tabt.toolbar.restart";
const FONT_UP_ID: &str = "dev.local.tabt.toolbar.fontup";
const FONT_DOWN_ID: &str = "dev.local.tabt.toolbar.fontdown";
const EXPORT_ID: &str = "dev.local.tabt.toolbar.export";
const REVEAL_ID: &str = "dev.local.tabt.toolbar.reveal";
/// AppKit's own item, which eats whatever width is left — it is what pins the session actions to
/// the trailing end while the leading pair stays by the traffic lights.
const FLEX_ID: &str = "NSToolbarFlexibleSpaceItem";
/// AppKit's fixed-width space. Recent macOS draws a run of adjacent items in one capsule, so this
/// is what splits that run into the two capsules: the AI launchers, then the actions that operate on
/// the session already in front of you.
const SPACE_ID: &str = "NSToolbarSpaceItem";
/// How the stored order writes that split: everything before this token is the AI row, everything
/// after it the Common row (see [`rows`]).
///
/// A token in the list rather than an entry with a tile of its own, because the customizer draws the
/// two groups as **two rows** and the break between them is the row break — there is nothing left for
/// a draggable "Space" button to mean. Configs written when it *was* one read back unchanged, which
/// is the whole reason the divider kept this key.
pub const SPACE_KEY: &str = "space";

/// Which half of the toolbar a button sits in: the AI launchers, or the actions that operate on the
/// session already in front of you. One capsule each, in this order.
///
/// A button's group is **the row the user put it in**, not a fixed property of the button: the
/// customizer draws one editable row per group and dragging across the gap moves a button between
/// them. [`Entry::group`] is only where a button starts — the row it joins the first time it is
/// placed, and the row an older config's arrangement is read back into when it recorded no divider.
#[derive(Clone, Copy, PartialEq)]
pub enum Group {
    Ai,
    Common,
}

impl Group {
    /// The heading the customizer shows over this group's row.
    pub fn label(self) -> &'static str {
        match self {
            Group::Ai => "AI",
            Group::Common => "Common",
        }
    }

    /// The groups, in the order the toolbar holds them.
    pub const ALL: [Group; 2] = [Group::Ai, Group::Common];
}

/// One entry the customizer can place: the short key `layout.conf` stores, the toolbar identifier,
/// the label the customizer shows under it, the symbol it draws, its tooltip, and which half of the
/// bar it belongs to.
pub struct Entry {
    pub key: &'static str,
    id: &'static str,
    pub label: &'static str,
    pub symbol: &'static str,
    tip: &'static str,
    /// Where the button starts: the row it joins when nothing has placed it yet. Not where it stays
    /// — see [`Group`].
    pub group: Group,
    /// Whether a config that has never mentioned this key gets the button.
    ///
    /// The **one deliberate exception** to the rule `toolbar_hidden` exists for — that a button a
    /// later version adds shows up rather than staying invisible. A version that adds seven at once
    /// would otherwise rearrange a bar the user had arranged by hand, so the newer ones start in the
    /// customizer's palette instead, one drag from the toolbar and impossible to miss there.
    default_on: bool,
}

/// Everything the trailing group can hold, in the order it holds it out of the box — which is also
/// the order a button added by a later version is appended in (see [`layout`]).
///
/// One table, so the customizer's tile and the toolbar's item cannot describe the same button
/// differently; the action is the one thing that cannot live in a `const` and is looked up by key
/// in [`action_for`].
pub const CUSTOMIZABLE: [Entry; 17] = [
    // ---- AI: each types its command into the session and presses Return ----
    Entry { key: "claude", id: CLAUDE_ID, label: "Claude", symbol: "claude", tip: "Run claude in this session", group: Group::Ai, default_on: true },
    Entry { key: "codex", id: CODEX_ID, label: "Codex", symbol: "openai", tip: "Run codex in this session", group: Group::Ai, default_on: true },
    Entry { key: "gemini", id: GEMINI_ID, label: "Gemini", symbol: "sparkles", tip: "Run gemini in this session", group: Group::Ai, default_on: false },
    Entry { key: "aider", id: AIDER_ID, label: "Aider", symbol: "wand.and.stars", tip: "Run aider in this session", group: Group::Ai, default_on: false },
    Entry { key: "cursor", id: CURSOR_ID, label: "Cursor", symbol: "cursorarrow.rays", tip: "Run cursor-agent in this session", group: Group::Ai, default_on: false },
    // ---- Common: the session in front of you ----
    Entry { key: "home", id: HOME_ID, label: "Home", symbol: "house", tip: "cd ~", group: Group::Common, default_on: true },
    // Copy and paste start in the palette: ⌘C/⌘V are muscle memory and a selection is made with the
    // mouse anyway, so the pair spent two of the row's slots restating the shortcut every other app
    // has. Still one drag away for anyone who wants them.
    Entry { key: "copy", id: COPY_ID, label: "Copy", symbol: "doc.on.doc", tip: "Copy (⌘C)", group: Group::Common, default_on: false },
    Entry { key: "paste", id: PASTE_ID, label: "Paste", symbol: "doc.on.clipboard", tip: "Paste (⌘V)", group: Group::Common, default_on: false },
    Entry { key: "clearline", id: CLEAR_LINE_ID, label: "Clear Line", symbol: "delete.left", tip: "Clear Line (⌃U)", group: Group::Common, default_on: true },
    Entry { key: "clear", id: CLEAR_ID, label: "Clear", symbol: "eraser", tip: "Clear Screen (⌃L)", group: Group::Common, default_on: true },
    Entry { key: "screenshot", id: SHOT_ID, label: "Screenshot", symbol: "camera.viewfinder", tip: "Screenshot (⇧⌘5)", group: Group::Common, default_on: true },
    Entry { key: "interrupt", id: INTERRUPT_ID, label: "Interrupt", symbol: "stop.circle", tip: "Interrupt (⌃C)", group: Group::Common, default_on: false },
    Entry { key: "restart", id: RESTART_ID, label: "Restart", symbol: "arrow.clockwise", tip: "Restart this session", group: Group::Common, default_on: false },
    Entry { key: "fontup", id: FONT_UP_ID, label: "Bigger", symbol: "plus.magnifyingglass", tip: "Increase Font Size (⌘=)", group: Group::Common, default_on: false },
    Entry { key: "fontdown", id: FONT_DOWN_ID, label: "Smaller", symbol: "minus.magnifyingglass", tip: "Decrease Font Size (⌘−)", group: Group::Common, default_on: false },
    // The two ways a session leaves the app. They replaced a single "share" button that dropped a
    // menu holding both: a menu is a click plus a read to reach either one, and at two items it was
    // a submenu standing in for two buttons. They inherit its place in the default bar, so the
    // toolbar out of the box can still do everything it could.
    Entry { key: "export", id: EXPORT_ID, label: "Export", symbol: "arrow.down.doc", tip: "Export Text… (⇧⌘S)", group: Group::Common, default_on: true },
    Entry { key: "reveal", id: REVEAL_ID, label: "Reveal", symbol: "folder", tip: "Reveal in Finder (⇧⌘R)", group: Group::Common, default_on: true },
];

/// The entry with that key, if this build has one — a stored layout may name a button an older or
/// newer version had.
pub fn entry(key: &str) -> Option<&'static Entry> {
    CUSTOMIZABLE.iter().find(|e| e.key == key)
}

/// The two rows the toolbar holds, in order: `(ai, common)`. The customizer edits exactly these, one
/// row each, and this is what resolves them out of the stored config.
///
/// Two stored keys feed it and neither is redundant. `toolbar_hidden` is the visibility, and it is
/// the *hidden* set so a button a later version adds appears rather than staying invisible;
/// `toolbar_order` is the arrangement — one flat list with [`SPACE_KEY`] marking the row break, so
/// the two rows and the toolbar's own item list are the same sequence written once.
///
/// A stored list with **no** divider is one written before the rows existed (or hand-edited): its
/// keys are read back into the row each button's table entry names, keeping their relative order, so
/// an arrangement never collapses into one row on upgrade.
///
/// A shown button the list does not mention — exactly the one a later version added — is appended to
/// its table row. That is the only place `default_on` applies: a button the user dragged in comes
/// back from the stored order regardless of it, so the newer buttons wait in the customizer's
/// palette and moving one into a row is permanent the moment it lands.
pub fn rows() -> (Vec<&'static str>, Vec<&'static str>) {
    let stored = settings::toolbar_order();
    let split = stored.iter().position(|k| k == SPACE_KEY);
    let mut out: [Vec<&'static str>; 2] = [Vec::new(), Vec::new()];
    let mut mentioned: Vec<&'static str> = Vec::new();
    for (i, k) in stored.iter().enumerate() {
        let Some(e) = entry(k) else { continue };
        mentioned.push(e.key);
        if !settings::toolbar_shows(e.key) {
            continue;
        }
        // Deduplicated rather than trusted: `layout.conf` is hand-editable, and a repeated key
        // would insert the *same* `NSToolbarItem` at two indices, which AppKit does not support.
        if out[0].contains(&e.key) || out[1].contains(&e.key) {
            continue;
        }
        let row = match split {
            Some(at) => usize::from(i > at),
            None => usize::from(e.group == Group::Common),
        };
        out[row].push(e.key);
    }
    for e in CUSTOMIZABLE.iter() {
        if e.default_on && settings::toolbar_shows(e.key) && !mentioned.contains(&e.key) {
            out[usize::from(e.group == Group::Common)].push(e.key);
        }
    }
    let [ai, common] = out;
    (ai, common)
}

/// The rows as one stored list: `ai`, the divider, `common`. What the customizer writes back and
/// what [`ToolbarDelegate::identifiers`] reads — the divider is kept even when a row is empty, so an
/// emptied row still says which side of the break the rest is on.
pub fn flatten(ai: &[&'static str], common: &[&'static str]) -> Vec<&'static str> {
    let mut keys = ai.to_vec();
    keys.push(SPACE_KEY);
    keys.extend_from_slice(common);
    keys
}

/// The bar a config that says nothing gets: the `default_on` keys in the table's order, split into
/// their table rows. Also what the customizer's Restore Defaults goes back to — "every entry" would
/// drag in the buttons the table deliberately keeps in the palette.
pub fn defaults() -> Vec<&'static str> {
    let of = |g: Group| -> Vec<&'static str> {
        CUSTOMIZABLE.iter().filter(|e| e.default_on && e.group == g).map(|e| e.key).collect()
    };
    flatten(&of(Group::Ai), &of(Group::Common))
}

/// The keys in neither row — what the customizer offers to drag back in.
pub fn removed() -> Vec<&'static str> {
    let (ai, common) = rows();
    CUSTOMIZABLE
        .iter()
        .map(|e| e.key)
        .filter(|k| !ai.contains(k) && !common.contains(k))
        .collect()
}

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
        // The user's two rows (Settings → Toolbar), one capsule each. The divider between them is
        // dropped when either row is empty: leading, it is a gap against the flexible space that
        // eats the row anyway; trailing, a gap with nothing on one side of it.
        let (ai, common) = rows();
        ids.extend(ai.iter().filter_map(|k| entry(k)).map(|e| e.id));
        if !ai.is_empty() && !common.is_empty() {
            ids.push(SPACE_ID);
        }
        ids.extend(common.iter().filter_map(|k| entry(k)).map(|e| e.id));
        ids
    }

    fn identifier_array(with_sidebar_pair: bool) -> Retained<NSArray<NSToolbarItemIdentifier>> {
        NSArray::from_vec(Self::identifiers(with_sidebar_pair).into_iter().map(NSString::from_str).collect())
    }
}

/// What each customizable button sends. The one part of [`CUSTOMIZABLE`] that cannot live in the
/// table itself, since a `Sel` is not a `const`.
///
/// `copy:`/`paste:` are the terminal's own, so they travel the responder chain to whichever
/// `TermView` has the keyboard exactly as the Edit menu's items do (see [`Toolbar::set_target`]).
/// The launchers type their command into the session and press Return, which is all "run claude
/// here" means — the shell resolves it on $PATH exactly as the user would. The font pair sends the
/// selectors the ⌘=/⌘− menu items already send, so the button and the shortcut stay one code path.
fn action_for(key: &str) -> Option<Sel> {
    Some(match key {
        "claude" => sel!(runClaude:),
        "codex" => sel!(runCodex:),
        "gemini" => sel!(runGemini:),
        "aider" => sel!(runAider:),
        "cursor" => sel!(runCursor:),
        "home" => sel!(goHome:),
        "copy" => sel!(copy:),
        "paste" => sel!(paste:),
        "clearline" => sel!(clearLine:),
        "clear" => sel!(clearScreen:),
        "screenshot" => sel!(takeScreenshot:),
        "interrupt" => sel!(interruptSession:),
        "restart" => sel!(restartSession:),
        "fontup" => sel!(increaseFontSize:),
        "fontdown" => sel!(decreaseFontSize:),
        // The same selectors the share menu's items send, so the button and the menu route cannot
        // drift — exactly as the font pair shares ⌘=/⌘−'s.
        "export" => sel!(exportText:),
        "reveal" => sel!(revealInFinder:),
        // A table row added without a case here gets no action rather than a wrong one: the item
        // then validates as disabled, which is visible, where a plausible-looking default would
        // quietly fire the neighbouring button's selector.
        _ => return None,
    })
}

/// One of the two buttons: a system symbol in a bordered toolbar item, which is what gives it the
/// standard size, hover highlight and pressed state.
///
/// The action is the selector the matching View-menu item already sends, so the menu and the button
/// cannot drift. The target is set once the menu target exists (see [`Toolbar::set_target`]); until
/// then the item validates against the responder chain and simply stays disabled.
fn item(mtm: MainThreadMarker, id: &str, symbol: &str, label: &str, tip: &str, action: Option<Sel>) -> Retained<NSToolbarItem> {
    unsafe {
        let item = NSToolbarItem::initWithItemIdentifier(mtm.alloc(), &NSString::from_str(id));
        let label = NSString::from_str(label);
        set_symbol(&item, symbol, &label);
        item.setLabel(&label);
        item.setPaletteLabel(&label);
        item.setToolTip(Some(&NSString::from_str(tip)));
        item.setBordered(true);
        item.setAction(action);
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
    let tone = icon_tone();
    let Some(colored) = icon_image(symbol, tone, Some(label)) else { return };
    unsafe {
        colored.setTemplate(false);
        item.setImage(Some(&colored));
    }
}

/// The tone every one of these icons is drawn in: the theme's foreground, dimmed toward its
/// background. Shared with the customizer's preview, which has to land on the same gray.
pub fn icon_tone() -> theme::Rgb {
    let t = theme::current();
    theme::mix(t.fg, t.bg, ICON_DIM)
}

/// One toolbar icon at [`ICON_PT`], tinted to `tone`.
///
/// A bundled artwork of that name wins — that is how `claude` and `openai` get their own marks —
/// and everything else is a system symbol. Both end up tinted to the same tone at the same size, so
/// a brand glyph sits in the row as one more icon rather than as a logo. The customizer draws its
/// preview through this too: `view::draw_symbol` knows only the system's symbols, and would render
/// the two brand marks as nothing at all.
pub fn icon_image(symbol: &str, tone: theme::Rgb, label: Option<&NSString>) -> Option<Retained<NSImage>> {
    unsafe {
        if let Some(img) = bundled_image(symbol) {
            return Some(tinted(&img, tone));
        }
        let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            label,
        )?;
        let color_cfg = NSImageSymbolConfiguration::configurationWithHierarchicalColor(&ns_color(tone));
        let size_cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight_scale(
            ICON_PT,
            NSFontWeightRegular,
            NSImageSymbolScale::Medium,
        );
        let cfg = size_cfg.configurationByApplyingConfiguration(&color_cfg);
        Some(img.imageWithSymbolConfiguration(&cfg).unwrap_or(img))
    }
}

/// Draw one toolbar icon centered in `r` — the customizer's preview, at the size and tone the real
/// band uses.
///
/// Centered at the image's own size and snapped to whole points, for the reason
/// `view::draw_symbol` documents: these are hairline glyphs, and an origin off the pixel grid
/// renders them as a soft smudge.
pub fn draw_icon(symbol: &str, r: NSRect, tone: theme::Rgb) {
    let Some(img) = icon_image(symbol, tone, None) else { return };
    unsafe {
        let sz = img.size();
        img.drawInRect(NSRect::new(
            NSPoint::new(
                (r.origin.x + (r.size.width - sz.width) / 2.0).round(),
                (r.origin.y + (r.size.height - sz.height) / 2.0).round(),
            ),
            NSSize::new(sz.width.round(), sz.height.round()),
        ));
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

/// Owns the toolbar and its delegate. Plain Rust, held by the controller: AppKit retains the
/// toolbar through the window, but nothing retains the delegate for us.
pub struct Toolbar {
    toolbar: Retained<NSToolbar>,
    _delegate: Retained<ToolbarDelegate>,
    /// Every item this toolbar owns with the symbol it draws — kept so `set_target` can point them
    /// all at the menu target once it exists, and so `apply_theme` can re-tint them.
    items: Vec<(Retained<NSToolbarItem>, &'static str)>,
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
        // The sidebar pair, which the customizer does not touch — they are the collapsed state's
        // stand-ins for the card's own strip, not session actions.
        let pair: [(&'static str, &'static str, &str, &str, Sel); 2] = [
            (TOGGLE_ID, "sidebar.left", "Sidebar", "Show Sidebar (⌘B)", sel!(toggleSidebar:)),
            (SEARCH_ID, "magnifyingglass", "Search", "Search Sessions (⌘F)", sel!(findSession:)),
        ];
        let mut built: Vec<(&str, &'static str, Retained<NSToolbarItem>)> = pair
            .iter()
            .map(|(id, sym, label, tip, action)| (*id, *sym, item(mtm, id, sym, label, tip, Some(*action))))
            .collect();
        // …then everything the rows can hold, from the one table that describes it: every entry is
        // a plain bordered image item, since the divider is AppKit's own and nothing here carries a
        // menu any more.
        for e in CUSTOMIZABLE.iter() {
            built.push((e.id, e.symbol, item(mtm, e.id, e.symbol, e.label, e.tip, action_for(e.key))));
        }
        let items: Vec<(Retained<NSToolbarItem>, &'static str)> =
            built.iter().map(|(_, sym, it)| (it.clone(), *sym)).collect();
        let delegate: Retained<ToolbarDelegate> = {
            let this = mtm.alloc();
            let this = this.set_ivars(DelegateIvars {
                items: built.iter().map(|(k, _, v)| (k.to_string(), v.clone())).collect(),
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
        Toolbar { toolbar, _delegate: delegate, items, shown: Cell::new(true) }
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
    }
}

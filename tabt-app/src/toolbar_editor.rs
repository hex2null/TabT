//! Settings → Toolbar: the customizer, a picture of the toolbar you can rearrange by hand.
//!
//! It replaces a column of nine checkboxes, which named the buttons and showed none of them — the
//! same trade the theme grid (`theme_grid.rs`) replaced the theme pop-up over, and the same answer:
//! draw the thing being chosen. The band at the top of the pane is the window's own title bar
//! rendered at full size — the theme's background, the traffic lights where AppKit puts them, the
//! icons at [`toolbar::ICON_PT`] in the tone [`toolbar::icon_tone`] gives them — so what the pane
//! shows and what the window shows are the same row of buttons, not an illustration of it.
//!
//! Below it are the buttons currently *out* of the toolbar, under one heading per
//! [`toolbar::Group`] — AI and Common. Those groups are the table's, and they are soft: they order
//! the palette and nothing else, so a drag can still put any button anywhere in the band. A group
//! with nothing left in it takes its heading with it.
//!
//! Dragging moves a button either way: into the band to add or reorder it, out of the band to
//! remove it. The **Space** is one more draggable entry rather than a fixed rule, which is what
//! makes the capsules the user's: recent macOS draws a run of adjacent items as one capsule, so
//! where the space lands is where the row splits.
//!
//! Two things about the drag are deliberate:
//!
//! - **A press that never moves does nothing.** The easy bug here is a mouse-up landing outside the
//!   band reading as "remove", so a stray click on a button would delete it.
//! - **The arrangement is committed once, on release.** `Toolbar::rebuild` removes and re-inserts
//!   every item; doing that per frame of a drag is the same mistake as applying a font size per
//!   pixel of the slider's travel.
//!
//! The band's own metrics — how wide a bordered item is, how far the run sits from the window's
//! trailing edge — are not queryable from AppKit and are measured off a screenshot, exactly as
//! `TOGGLE_RIGHT_X` and the card's `WINDOW_RADIUS` are.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSEvent, NSFont, NSFontWeightMedium, NSFontWeightRegular, NSGraphicsContext,
    NSStringDrawing, NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSString};

use crate::app::AppController;
use crate::header::HEADER_H;
use crate::theme;
use crate::toolbar;
use crate::view::{make_attrs, ns_color, rect, round_fill, round_stroke};

/// Margin between the pane's edge and everything in it. Also where the dialog puts the pane's
/// one standard control, the Restore Defaults button.
pub const PAD_X: f64 = 14.0;
/// Top of the band preview, under the caption line.
const BAND_Y: f64 = 30.0;
/// Corner radius of the band preview. It stands in for the top of the window, whose own corners are
/// rounder, but a 26pt radius on a 52pt-tall strip is a lozenge — this is the window's *look*, at
/// the size a preview can carry.
const BAND_RADIUS: f64 = 8.0;

/// One bordered toolbar item's footprint. Measured off a live window's own item views — AppKit
/// exposes no metric for it, and an item's frame is only readable once it has been inserted into a
/// toolbar that has been laid out. Items abut: the run's rhythm is the item width alone.
const ITEM_W: f64 = 44.0;
/// `NSToolbarSpaceItem`'s width, measured the same way — a hairline of a thing, which is why the
/// editor widens only its *hit* area (see [`GRAB_SLACK`]) and not its footprint.
const SPACE_W: f64 = 8.0;
/// Distance from the window's trailing edge to the last item.
const TRAIL_INSET: f64 = 4.0;
/// The capsule recent macOS draws behind a run of adjacent items: how far it is inset into the band,
/// and how tall it therefore is. Measured off the same window.
const PLATTER_INSET: f64 = 8.0;
/// How far either side of the space's 8pt footprint still counts as a press on it.
const GRAB_SLACK: f64 = 8.0;
/// The plate under the item riding the pointer during a drag — the capsule's own height.
const PLATE_H: f64 = 36.0;
/// The glyph box inside an item — the same 17pt box the card's strip buttons draw into, which is
/// what keeps the two sets of icons the same size where they share a row.
const ICON_BOX: f64 = 17.0;

/// Room the lights and the session title keep for themselves at the band's leading edge — where
/// the button run is clipped (see [`ToolbarEditor::run_zone`]). Measured from the lights' own
/// geometry plus enough for a short title, not from the title's actual width, which changes with
/// the session and would move the clip under the user.
const TITLE_ZONE_W: f64 = LIGHT_X + 3.0 * (LIGHT_D + LIGHT_GAP) + 14.0 + 62.0;

/// The traffic lights: diameter, spacing, and where the first one's left edge sits.
const LIGHT_D: f64 = 12.0;
const LIGHT_GAP: f64 = 8.0;
const LIGHT_X: f64 = 13.0;
/// The three lights, in order. Fixed sRGB rather than theme-derived: they are the system's buttons
/// and look the same on every theme, which is exactly why they anchor the preview.
const LIGHTS: [(f64, f64, f64); 3] =
    [(1.0, 0.373, 0.341), (0.996, 0.737, 0.18), (0.157, 0.784, 0.251)];

/// A palette tile: the icon over its name.
const TILE_W: f64 = 76.0;
const TILE_H: f64 = 52.0;
const TILE_GAP: f64 = 10.0;
/// Gap between the band and the palette's first heading, and the height a heading occupies.
const PALETTE_TOP: f64 = 22.0;
const HEADING_H: f64 = 22.0;
/// Extra room between one group's last tile row and the next group's heading.
const SECTION_GAP: f64 = 8.0;

/// How far the pointer must travel before a press becomes a drag. Below this the gesture is a
/// click, and a click here does nothing at all (see the module docs).
const DRAG_SLOP: f64 = 3.0;

/// Where a press landed. `Copy`, so the whole thing stays in a `Cell`.
#[derive(Clone, Copy, PartialEq)]
enum Press {
    None,
    /// Index into the band's list.
    Band(usize),
    /// Index into the palette's list.
    Palette(usize),
}

/// No drop position — the pointer is outside the band, which for a band item means "remove".
const NO_DROP: isize = -1;
/// A [`Slot`] that is a heading rather than a tile.
const NO_INDEX: usize = usize::MAX;

/// One placed thing in the palette: a group's heading, or one button's tile. Headings carry the
/// group they name; tiles carry their position in the flat palette list, which is what a press on
/// one reports (`Press::Palette`).
struct Slot {
    index: usize,
    group: Option<toolbar::Group>,
    rect: NSRect,
}

pub struct EditorIvars {
    controller: Cell<*const AppController>,
    /// The arrangement being edited. Seeded from [`toolbar::layout`] and pushed back to the
    /// controller on mouse-up, so a drag in flight never touches the real toolbar.
    band: RefCell<Vec<&'static str>>,
    /// The buttons currently out of the toolbar, in the table's own order.
    palette: RefCell<Vec<&'static str>>,
    press: Cell<Press>,
    start: Cell<NSPoint>,
    cur: Cell<NSPoint>,
    dragging: Cell<bool>,
    drop_at: Cell<isize>,
}

declare_class!(
    pub struct ToolbarEditor;

    // SAFETY: plain NSView subclass, main-thread only like every view here, no Drop.
    unsafe impl ClassType for ToolbarEditor {
        type Super = NSView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ToolbarEditor";
    }

    impl DeclaredClass for ToolbarEditor {
        type Ivars = EditorIvars;
    }

    unsafe impl NSObjectProtocol for ToolbarEditor {}

    unsafe impl ToolbarEditor {
        // Top-down, like the rest of the app's self-drawn views: the band is the first thing you
        // see and sits at the top.
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
            let p = self.local_point(event);
            self.ivars().start.set(p);
            self.ivars().cur.set(p);
            self.ivars().dragging.set(false);
            self.ivars().drop_at.set(NO_DROP);
            self.ivars().press.set(self.hit(p));
        }

        #[method(mouseDragged:)]
        fn mouse_dragged(&self, event: &NSEvent) {
            if self.ivars().press.get() == Press::None {
                return;
            }
            let p = self.local_point(event);
            self.ivars().cur.set(p);
            if !self.ivars().dragging.get() {
                let s = self.ivars().start.get();
                if (p.x - s.x).abs() < DRAG_SLOP && (p.y - s.y).abs() < DRAG_SLOP {
                    return;
                }
                self.ivars().dragging.set(true);
            }
            self.ivars().drop_at.set(self.drop_index(p));
            unsafe { self.setNeedsDisplay(true) };
        }

        #[method(mouseUp:)]
        fn mouse_up(&self, _event: &NSEvent) {
            self.finish_drag();
        }
    }
);

impl ToolbarEditor {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc();
        let this = this.set_ivars(EditorIvars {
            controller: Cell::new(std::ptr::null()),
            band: RefCell::new(Vec::new()),
            palette: RefCell::new(Vec::new()),
            press: Cell::new(Press::None),
            start: Cell::new(NSPoint::new(0.0, 0.0)),
            cur: Cell::new(NSPoint::new(0.0, 0.0)),
            dragging: Cell::new(false),
            drop_at: Cell::new(NO_DROP),
        });
        let this: Retained<Self> = unsafe { msg_send_id![super(this), initWithFrame: frame] };
        this.reload();
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

    /// Re-read the arrangement from the settings — what the dialog calls when it re-seeds, and what
    /// "Restore Defaults" ends on.
    pub fn reload(&self) {
        *self.ivars().band.borrow_mut() = toolbar::layout();
        *self.ivars().palette.borrow_mut() = toolbar::removed();
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Back to the bar a fresh config produces — [`toolbar::defaults`], not every entry: the table
    /// deliberately keeps the newer buttons in the palette, and "restore" must not drag them in.
    pub fn restore_defaults(&self) {
        if let Some(c) = self.controller() {
            let order: Vec<String> = toolbar::defaults().iter().map(|k| k.to_string()).collect();
            let hidden: Vec<String> = toolbar::CUSTOMIZABLE
                .iter()
                .filter(|e| !order.iter().any(|k| k == e.key))
                .map(|e| e.key.to_string())
                .collect();
            c.set_toolbar_layout(order, hidden);
        }
        self.reload();
    }

    /// How tall the pane's content is, so the dialog can place what goes under it. Measured against
    /// the *worst* case — every button removed — because the pane is a fixed size and a palette
    /// taller than its frame is clipped, not scrolled.
    pub fn height(&self, width: f64) -> f64 {
        let all: Vec<&'static str> = toolbar::CUSTOMIZABLE.iter().map(|e| e.key).collect();
        let slots = self.palette_slots(&all, width);
        let bottom = slots.iter().map(|s| s.rect.origin.y + s.rect.size.height).fold(0.0, f64::max);
        bottom.max(self.palette_top()) + TILE_GAP
    }

    // ---- geometry ----

    /// The band's height, read back off the window so the preview is the real one: with a toolbar
    /// attached that height is the system's, not ours.
    fn band_h(&self) -> f64 {
        self.controller().map(|c| c.band_h()).unwrap_or(HEADER_H)
    }

    fn band_rect(&self) -> NSRect {
        let w = self.frame().size.width;
        rect(PAD_X, BAND_Y, (w - 2.0 * PAD_X).max(1.0), self.band_h())
    }

    /// The part of the band the button run may occupy: everything right of the traffic lights and
    /// the session title. The run is drawn and hit-tested against this, so a run too long for the
    /// preview's narrower window is cut at the title rather than drawn over it.
    fn run_zone(&self, band: NSRect) -> NSRect {
        let left = band.origin.x + TITLE_ZONE_W;
        rect(left, band.origin.y, (band.origin.x + band.size.width - left).max(0.0), band.size.height)
    }

    /// How many palette tiles fit on a row.
    fn cols(&self, width: f64) -> usize {
        (((width - 2.0 * PAD_X + TILE_GAP) / (TILE_W + TILE_GAP)).floor() as usize).max(1)
    }

    /// Where the palette's first heading sits.
    fn palette_top(&self) -> f64 {
        BAND_Y + self.band_h() + PALETTE_TOP
    }

    /// Lay the palette out: one heading per group that has tiles, then that group's tiles in rows.
    ///
    /// The single source of the palette's geometry — `render`, `hit` and `height` all read this, so
    /// a section break cannot land in one of them and not the others. `index` is the position in the
    /// flat palette list, which is what `Press::Palette` carries.
    fn palette_slots(&self, palette: &[&'static str], width: f64) -> Vec<Slot> {
        let cols = self.cols(width);
        let mut slots = Vec::new();
        let mut y = self.palette_top();
        for g in toolbar::Group::ALL {
            let of_group: Vec<usize> = palette
                .iter()
                .enumerate()
                .filter(|(_, k)| toolbar::entry(k).map(|e| e.group) == Some(g))
                .map(|(i, _)| i)
                .collect();
            // A group with nothing left in it takes its heading with it, exactly as an empty run
            // takes its space in the band.
            if of_group.is_empty() {
                continue;
            }
            slots.push(Slot { index: NO_INDEX, group: Some(g), rect: rect(PAD_X, y, width, HEADING_H) });
            y += HEADING_H;
            let rows = of_group.len().div_ceil(cols);
            for (n, index) in of_group.into_iter().enumerate() {
                let (col, row) = (n % cols, n / cols);
                slots.push(Slot {
                    index,
                    group: None,
                    rect: rect(
                        PAD_X + col as f64 * (TILE_W + TILE_GAP),
                        y + row as f64 * (TILE_H + TILE_GAP),
                        TILE_W,
                        TILE_H,
                    ),
                });
            }
            y += rows as f64 * (TILE_H + TILE_GAP) + SECTION_GAP;
        }
        slots
    }

    /// The palette laid out for what is actually in it right now.
    fn slots_now(&self) -> Vec<Slot> {
        let palette = self.ivars().palette.borrow().clone();
        self.palette_slots(&palette, self.frame().size.width)
    }

    /// Where each of `keys` sits inside the band: one right-aligned run, exactly as the flexible
    /// space leaves it in the real toolbar.
    fn item_rects(&self, keys: &[&'static str], band: NSRect) -> Vec<NSRect> {
        let total: f64 = keys.iter().map(|k| item_w(k)).sum();
        let mut x = band.origin.x + band.size.width - TRAIL_INSET - total;
        keys.iter()
            .map(|k| {
                // An item viewer is the full height of the band, which is what the capsule and the
                // hit testing are both measured against.
                let r = rect(x, band.origin.y, item_w(k), band.size.height);
                x += item_w(k);
                r
            })
            .collect()
    }

    /// The band's list without the item being dragged out of it — what the run will look like once
    /// the drop lands, and therefore what the insertion point has to be measured against.
    fn preview_keys(&self) -> Vec<&'static str> {
        let mut keys = self.ivars().band.borrow().clone();
        if let Press::Band(i) = self.ivars().press.get() {
            if self.ivars().dragging.get() && i < keys.len() {
                keys.remove(i);
            }
        }
        keys
    }

    fn hit(&self, p: NSPoint) -> Press {
        let band = self.band_rect();
        if contains(band, p) {
            // Only the run's own zone: an item clipped away behind the title is not on screen, and
            // a press there would grab something the user cannot see.
            if !contains(self.run_zone(band), p) {
                return Press::None;
            }
            let keys = self.ivars().band.borrow().clone();
            for (i, (k, r)) in keys.iter().zip(self.item_rects(&keys, band)).enumerate() {
                // The space is 8pt wide for real, which is a picture worth keeping and a target
                // worth widening: only its hit area grows.
                let slack = if *k == toolbar::SPACE_KEY { GRAB_SLACK } else { 0.0 };
                if p.x >= r.origin.x - slack && p.x <= r.origin.x + r.size.width + slack {
                    return Press::Band(i);
                }
            }
            return Press::None;
        }
        for s in self.slots_now() {
            if s.index != NO_INDEX && contains(s.rect, p) {
                return Press::Palette(s.index);
            }
        }
        Press::None
    }

    /// Where the dragged item would land: an index into [`Self::preview_keys`], or [`NO_DROP`] when
    /// the pointer is not over the band.
    fn drop_index(&self, p: NSPoint) -> isize {
        let band = self.band_rect();
        // The whole band, deliberately — not the run's clipped zone. A release over the title or
        // the traffic lights is still a release *inside the toolbar*, and the count below already
        // yields 0 there, which is "move it to the front". Narrowing this to the zone would make a
        // drop the user can see land inside the bar mean "remove".
        if !contains(band, p) {
            return NO_DROP;
        }
        let keys = self.preview_keys();
        let rects = self.item_rects(&keys, band);
        rects.iter().filter(|r| p.x > r.origin.x + r.size.width / 2.0).count() as isize
    }

    /// Apply the drag and push the result through to the toolbar. The one place the arrangement is
    /// committed, and only ever from a mouse-up.
    fn finish_drag(&self) {
        let press = self.ivars().press.get();
        let dragging = self.ivars().dragging.get();
        let drop_at = self.ivars().drop_at.get();
        self.ivars().press.set(Press::None);
        self.ivars().dragging.set(false);
        self.ivars().drop_at.set(NO_DROP);
        if !dragging {
            // A press that never moved. Deliberately nothing: a stray click must not delete a
            // button, and there is no second meaning for it here.
            unsafe { self.setNeedsDisplay(true) };
            return;
        }
        {
            let mut band = self.ivars().band.borrow_mut();
            let mut palette = self.ivars().palette.borrow_mut();
            match press {
                Press::Band(i) if i < band.len() => {
                    let key = band.remove(i);
                    if drop_at == NO_DROP {
                        // Dragged out of the band: removed, and offered back in the palette at the
                        // place the table would put it, not at the end of a growing pile.
                        insert_by_table(&mut palette, key);
                    } else {
                        let at = (drop_at as usize).min(band.len());
                        band.insert(at, key);
                    }
                }
                Press::Palette(i) if i < palette.len() && drop_at != NO_DROP => {
                    let key = palette.remove(i);
                    let at = (drop_at as usize).min(band.len());
                    band.insert(at, key);
                }
                // Dropped a palette tile outside the band, or a press on nothing: no change.
                _ => {}
            }
        }
        let order: Vec<String> = self.ivars().band.borrow().iter().map(|k| k.to_string()).collect();
        let mut hidden: Vec<String> = self.ivars().palette.borrow().iter().map(|k| k.to_string()).collect();
        // A key this build does not know is one a *newer* version hid, and it is not the
        // customizer's to drop: the editor only ever saw the buttons in this table, so anything
        // else is carried through untouched rather than rewritten out by the first drag.
        hidden.extend(
            crate::settings::toolbar_hidden().into_iter().filter(|k| toolbar::entry(k).is_none()),
        );
        if let Some(c) = self.controller() {
            c.set_toolbar_layout(order, hidden);
        }
        unsafe { self.setNeedsDisplay(true) };
    }

    fn local_point(&self, event: &NSEvent) -> NSPoint {
        let win = unsafe { event.locationInWindow() };
        self.convertPoint_fromView(win, None)
    }

    // ---- drawing ----

    fn render(&self) {
        let t = theme::current();
        let tone = toolbar::icon_tone();
        let band = self.band_rect();
        let dragging = self.ivars().dragging.get();

        let caption = unsafe { NSFont::systemFontOfSize_weight(11.0, NSFontWeightRegular) };
        let heading = unsafe { NSFont::systemFontOfSize_weight(11.0, NSFontWeightMedium) };
        let tile_font = unsafe { NSFont::systemFontOfSize_weight(10.0, NSFontWeightRegular) };
        let secondary = unsafe { NSColor::secondaryLabelColor() };

        draw_text(
            "Drag an icon to arrange the toolbar; drag it out of the bar to remove it.",
            NSPoint::new(PAD_X, 8.0),
            &caption,
            &secondary,
        );

        // ---- The band: the window's own title bar, at full size ----
        // `ns_color`, not `ns_color_bg`: the opacity setting belongs to a window over a desktop,
        // and inside this panel it would show the panel's own background through the preview.
        round_fill(band, BAND_RADIUS, &ns_color(t.bg));
        round_stroke(inset(band, 0.5), BAND_RADIUS, 1.0, &ns_color(t.card_border()));

        let ly = band.origin.y + (band.size.height - LIGHT_D) / 2.0;
        for (i, c) in LIGHTS.iter().enumerate() {
            let x = band.origin.x + LIGHT_X + i as f64 * (LIGHT_D + LIGHT_GAP);
            round_fill(rect(x, ly, LIGHT_D, LIGHT_D), LIGHT_D / 2.0, &ns_color(*c));
        }
        // A session title where the header draws one, so the preview reads as the window's top
        // strip rather than as a floating row of icons.
        let title_x = band.origin.x + LIGHT_X + 3.0 * (LIGHT_D + LIGHT_GAP) + 14.0;
        let title = unsafe { NSFont::systemFontOfSize_weight(12.0, NSFontWeightMedium) };
        let attrs = make_attrs(&title, Some(&ns_color(theme::mix(t.fg, t.bg, 0.15))));
        let s = NSString::from_str("Session");
        let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
        unsafe {
            s.drawAtPoint_withAttributes(
                NSPoint::new(title_x, band.origin.y + (band.size.height - sz.height) / 2.0),
                Some(&attrs),
            )
        };

        // ---- The buttons, right-aligned as the flexible space leaves them ----
        let keys = self.preview_keys();
        let rects = self.item_rects(&keys, band);
        // Clipped to the room left of the title, because the preview band is a *narrower* window
        // than the real one: at 1:1 a long enough run outgrows it and would otherwise paint over
        // the title and the traffic lights, which reads as a bug rather than as "your window is
        // wider than this". The real toolbar has the whole window and only overflows into AppKit's
        // own overflow menu. Everything is still laid out at full size — only the pixels are cut.
        let zone = self.run_zone(band);
        save_state();
        clip(zone);
        for (from, to) in runs(&keys) {
            // The capsule recent macOS draws behind each run of adjacent items — which is what a
            // space in the middle of the run is *for*, and so the one thing the preview cannot
            // leave out.
            let (a, b) = (rects[from], rects[to]);
            let plate = rect(
                a.origin.x,
                band.origin.y + PLATTER_INSET,
                b.origin.x + b.size.width - a.origin.x,
                band.size.height - 2.0 * PLATTER_INSET,
            );
            let h = plate.size.height;
            round_fill(plate, h / 2.0, &ns_color(theme::mix(t.fg, t.bg, 0.88)));
        }
        for (k, r) in keys.iter().zip(rects.iter()) {
            self.draw_entry(k, *r, tone);
        }
        restore_state();
        // …and say so at the cut, rather than leaving a run that looks arbitrarily sliced. In the
        // band's own dimmed tone, not a system label color: it sits inside the preview.
        if rects.first().map(|r| r.origin.x < zone.origin.x).unwrap_or(false) {
            let attrs = make_attrs(&caption, Some(&ns_color(theme::mix(t.fg, t.bg, 0.55))));
            let s = NSString::from_str("…");
            let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
            unsafe {
                s.drawAtPoint_withAttributes(
                    NSPoint::new(
                        zone.origin.x - sz.width - 4.0,
                        band.origin.y + (band.size.height - sz.height) / 2.0,
                    ),
                    Some(&attrs),
                )
            };
        }

        // The insertion caret, in the system accent color — the one mark here that must read the
        // same on every theme, so it is deliberately not theme-derived.
        if dragging {
            let drop_at = self.ivars().drop_at.get();
            if drop_at != NO_DROP {
                let i = (drop_at as usize).min(rects.len());
                let x = if rects.is_empty() {
                    band.origin.x + band.size.width - TRAIL_INSET
                } else if i == 0 {
                    rects[0].origin.x
                } else {
                    let r = rects[i - 1];
                    r.origin.x + r.size.width
                };
                // Clamped into the run's zone: the caret is drawn after the clip is released, so
                // an overflowing run would otherwise put it over the traffic lights.
                let x = x.max(zone.origin.x);
                let caret = rect(
                    x.round() - 1.0,
                    band.origin.y + PLATTER_INSET - 2.0,
                    2.0,
                    band.size.height - 2.0 * (PLATTER_INSET - 2.0),
                );
                round_fill(caret, 1.0, unsafe { &NSColor::controlAccentColor() });
            }
        }

        // ---- The palette: what is out of the toolbar, under one heading per group ----
        let palette = self.ivars().palette.borrow().clone();
        let skip = match (dragging, self.ivars().press.get()) {
            // The tile being dragged is under the pointer instead of in its slot.
            (true, Press::Palette(i)) => Some(i),
            _ => None,
        };
        for s in self.slots_now() {
            if let Some(g) = s.group {
                draw_text(g.label(), NSPoint::new(s.rect.origin.x, s.rect.origin.y), &heading, &secondary);
                continue;
            }
            if Some(s.index) == skip {
                continue;
            }
            let Some(k) = palette.get(s.index) else { continue };
            let r = s.rect;
            let icon = rect(
                r.origin.x + (TILE_W - ICON_BOX) / 2.0,
                r.origin.y + 8.0,
                ICON_BOX,
                ICON_BOX,
            );
            self.draw_entry(k, icon, tone);
            if let Some(e) = toolbar::entry(k) {
                draw_centered(e.label, rect(r.origin.x, r.origin.y + 30.0, TILE_W, 14.0), &tile_font, &secondary);
            }
        }
        if palette.is_empty() {
            draw_text(
                "Every button is in the toolbar.",
                NSPoint::new(PAD_X, self.palette_top() + 6.0),
                &tile_font,
                &unsafe { NSColor::tertiaryLabelColor() },
            );
        }

        // ---- The item under the pointer, drawn last so it rides over everything ----
        if dragging {
            let key = match self.ivars().press.get() {
                Press::Band(i) => self.ivars().band.borrow().get(i).copied(),
                Press::Palette(i) => palette.get(i).copied(),
                Press::None => None,
            };
            if let Some(k) = key {
                let p = self.ivars().cur.get();
                let r = rect(p.x - ITEM_W / 2.0, p.y - PLATE_H / 2.0, ITEM_W, PLATE_H);
                // A faint plate under it, so the icon stays legible over the palette's own
                // background as well as over the themed band.
                let plate = unsafe { NSColor::controlAccentColor().colorWithAlphaComponent(0.16) };
                round_fill(r, PLATE_H / 2.0, &plate);
                self.draw_entry(k, r, tone);
            }
        }
    }

    /// One entry inside `r`: its icon, or — for the space, which is invisible by definition — the
    /// sliver that gives the user something to grab. That sliver is the editor's own affordance and
    /// has no counterpart in the real band, which is the one place this preview is not literal.
    fn draw_entry(&self, key: &str, r: NSRect, tone: theme::Rgb) {
        if key == toolbar::SPACE_KEY {
            let faint = theme::mix(tone, theme::current().bg, 0.5);
            // Proportional, because this is drawn into two very different boxes: the band's 52pt
            // item and the palette's 17pt icon square. A fixed inset that suits the first leaves
            // nothing at all of the second.
            let h = (r.size.height * 0.35).max(8.0);
            let bar = rect(
                (r.origin.x + r.size.width / 2.0 - 1.0).round(),
                (r.origin.y + (r.size.height - h) / 2.0).round(),
                2.0,
                h,
            );
            round_fill(bar, 1.0, &ns_color(faint));
            return;
        }
        let Some(e) = toolbar::entry(key) else { return };
        let box_r = rect(
            r.origin.x + (r.size.width - ICON_BOX) / 2.0,
            r.origin.y + (r.size.height - ICON_BOX) / 2.0,
            ICON_BOX,
            ICON_BOX,
        );
        toolbar::draw_icon(e.symbol, box_r, tone);
    }
}

/// Put `key` back into the palette where [`toolbar::CUSTOMIZABLE`] would have it, so removing three
/// buttons leaves them in the table's order rather than in the order they were dragged out.
fn insert_by_table(palette: &mut Vec<&'static str>, key: &'static str) {
    let rank = |k: &str| toolbar::CUSTOMIZABLE.iter().position(|e| e.key == k).unwrap_or(usize::MAX);
    let at = palette.iter().position(|k| rank(k) > rank(key)).unwrap_or(palette.len());
    palette.insert(at, key);
}

/// One entry's footprint in the band.
fn item_w(key: &str) -> f64 {
    if key == toolbar::SPACE_KEY {
        SPACE_W
    } else {
        ITEM_W
    }
}

/// The runs of adjacent buttons in `keys`, as inclusive index pairs — one capsule each. A space is
/// what ends a run, which is the whole reason it is placeable.
fn runs(keys: &[&'static str]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if *k == toolbar::SPACE_KEY {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 + 1 == i => last.1 = i,
            _ => out.push((i, i)),
        }
    }
    out
}

/// Push/pop the drawing state around a clip, on the context `drawRect:` is running in — there is no
/// `NSView` API for a scoped clip, and the state has to be restored or the clip leaks into whatever
/// AppKit draws next in the same pass.
fn save_state() {
    if let Some(ctx) = unsafe { NSGraphicsContext::currentContext() } {
        unsafe { ctx.saveGraphicsState() };
    }
}

fn restore_state() {
    if let Some(ctx) = unsafe { NSGraphicsContext::currentContext() } {
        unsafe { ctx.restoreGraphicsState() };
    }
}

/// Clip everything drawn after this (until [`restore_state`]) to `r`.
fn clip(r: NSRect) {
    unsafe { NSBezierPath::bezierPathWithRect(r).addClip() };
}

fn contains(r: NSRect, p: NSPoint) -> bool {
    p.x >= r.origin.x
        && p.x <= r.origin.x + r.size.width
        && p.y >= r.origin.y
        && p.y <= r.origin.y + r.size.height
}

/// `r` shrunk by `d` on every side — a stroke is centered on its path, so a border drawn on the
/// rect's own bounds would be half outside it.
fn inset(r: NSRect, d: f64) -> NSRect {
    rect(r.origin.x + d, r.origin.y + d, r.size.width - 2.0 * d, r.size.height - 2.0 * d)
}

/// A line of the panel's own text — system label colors, unlike the band's contents, which are the
/// theme's. Same split the theme cards keep: the preview is themed, the chrome around it is not.
fn draw_text(text: &str, at: NSPoint, font: &Retained<NSFont>, color: &Retained<NSColor>) {
    let attrs = make_attrs(font, Some(color));
    unsafe { NSString::from_str(text).drawAtPoint_withAttributes(at, Some(&attrs)) };
}

/// The same, centered in `r` — a tile's label under its icon.
fn draw_centered(text: &str, r: NSRect, font: &Retained<NSFont>, color: &Retained<NSColor>) {
    let attrs = make_attrs(font, Some(color));
    let s = NSString::from_str(text);
    let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
    let x = r.origin.x + (r.size.width - sz.width) / 2.0;
    unsafe { s.drawAtPoint_withAttributes(NSPoint::new(x.round(), r.origin.y), Some(&attrs)) };
}

//! Settings → Toolbar: the customizer, a picture of the toolbar you can rearrange by hand.
//!
//! It replaces a column of checkboxes, which named the buttons and showed none of them — the same
//! trade the theme grid (`theme_grid.rs`) replaced the theme pop-up over, and the same answer: draw
//! the thing being chosen. Each button is drawn exactly as the window draws it — the theme's
//! background behind it, the icon at [`toolbar::ICON_PT`] in the tone [`toolbar::icon_tone`] gives
//! it, the 44pt item box and the capsule measured off a live window's own item views — so the pane
//! and the title bar show the same row of buttons rather than an illustration of one.
//!
//! **One editable row per group** ([`toolbar::Group`]): AI, then Common. The row a button sits in
//! *is* its group, so dragging one across the gap moves it between them, and the toolbar is those
//! two rows with a space between — one capsule each. That is also why there is no "Space" tile: the
//! break between the capsules is the break between the rows, and a button that could be dragged
//! either side of it would be saying the same thing twice.
//!
//! It is deliberately **not** a picture of the whole title bar. The traffic lights and the session
//! title used to be drawn here for realism, and they cost the row most of its width for nothing an
//! editor needs; the strip is the part being edited, at full size, and nothing else.
//!
//! Below the rows are the buttons currently *out* of the toolbar. Dragging moves one either way:
//! into a row to add it, out of both to remove it. Two things about that drag are deliberate:
//!
//! - **A press that never moves does nothing.** The easy bug here is a mouse-up landing outside a
//!   row reading as "remove", so a stray click on a button would delete it.
//! - **The arrangement is committed once, on release.** `Toolbar::rebuild` removes and re-inserts
//!   every item; doing that per frame of a drag is the same mistake as applying a font size per
//!   pixel of the slider's travel.
//!
//! The item metrics are measured, not documented — AppKit answers none of them, and an item's frame
//! is only readable once it has been inserted into a laid-out toolbar. That is the same footing
//! `TOGGLE_RIGHT_X` and the card's `WINDOW_RADIUS` stand on.

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

/// Margin between the pane's edge and everything in it. Also where the dialog puts the pane's one
/// standard control, the Restore Defaults button.
pub const PAD_X: f64 = 14.0;
/// Top of the first group's heading, under the caption line.
const ROWS_Y: f64 = 30.0;
/// Corner radius of a row strip.
const ROW_RADIUS: f64 = 8.0;
/// Inset from a row's leading edge to its first item. The real toolbar's run is *trailing*-aligned
/// (the flexible space eats the rest), but a row here is an editing surface with nothing else on it,
/// and a run that grows leftwards off a fixed right edge moves every button whenever one is added.
const ROW_PAD: f64 = 6.0;
/// Gap between one group's row and the next group's heading.
const ROW_GAP: f64 = 16.0;

/// One bordered toolbar item's footprint. Measured off a live window's own item views — AppKit
/// exposes no metric for it, and an item's frame is only readable once it has been inserted into a
/// toolbar that has been laid out. Items abut: the run's rhythm is the item width alone.
const ITEM_W: f64 = 44.0;
/// The capsule recent macOS draws behind a run of adjacent items: how far it is inset into the band,
/// and so how tall it is. Measured off the same window.
const PLATTER_INSET: f64 = 8.0;
/// The plate under the item riding the pointer during a drag — the capsule's own height.
const PLATE_H: f64 = 36.0;
/// The glyph box inside an item — the same 17pt box the card's strip buttons draw into, which is
/// what keeps the two sets of icons the same size where they share a row.
const ICON_BOX: f64 = 17.0;

/// A palette tile: the icon over its name.
const TILE_W: f64 = 76.0;
const TILE_H: f64 = 52.0;
const TILE_GAP: f64 = 10.0;
/// Gap between the last row and the palette's heading, and the height a heading occupies.
const PALETTE_TOP: f64 = 20.0;
const HEADING_H: f64 = 20.0;

/// How far the pointer must travel before a press becomes a drag. Below this the gesture is a
/// click, and a click here does nothing at all (see the module docs).
const DRAG_SLOP: f64 = 3.0;

/// Where a press landed. `Copy`, so the whole thing stays in a `Cell`.
#[derive(Clone, Copy, PartialEq)]
enum Press {
    None,
    /// A button in a row: which row, and where in it.
    Row(usize, usize),
    /// A tile in the palette, by its position there.
    Palette(usize),
}

/// No drop target — the pointer is over neither row, which for a button already in one means
/// "remove".
const NO_DROP: (usize, usize) = (usize::MAX, 0);

pub struct EditorIvars {
    controller: Cell<*const AppController>,
    /// The arrangement being edited, one list per group, in [`toolbar::Group::ALL`] order. Seeded
    /// from [`toolbar::rows`] and pushed back to the controller on mouse-up, so a drag in flight
    /// never touches the real toolbar.
    rows: RefCell<Vec<Vec<&'static str>>>,
    /// The buttons currently in neither row, in the table's own order.
    palette: RefCell<Vec<&'static str>>,
    press: Cell<Press>,
    start: Cell<NSPoint>,
    cur: Cell<NSPoint>,
    dragging: Cell<bool>,
    /// Where the dragged button would land: `(row, index)`, or [`NO_DROP`].
    drop_at: Cell<(usize, usize)>,
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
        // Top-down, like the rest of the app's self-drawn views: the first row is the first thing
        // you see and sits at the top.
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
            self.ivars().drop_at.set(self.drop_target(p));
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
            rows: RefCell::new(vec![Vec::new(); toolbar::Group::ALL.len()]),
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
        let (ai, common) = toolbar::rows();
        *self.ivars().rows.borrow_mut() = vec![ai, common];
        *self.ivars().palette.borrow_mut() = toolbar::removed();
        unsafe { self.setNeedsDisplay(true) };
    }

    /// Back to the bar a fresh config produces — [`toolbar::defaults`], not every entry: the table
    /// deliberately keeps the newer buttons in the palette, and "restore" must not drag them in.
    pub fn restore_defaults(&self) {
        if let Some(c) = self.controller() {
            let order = toolbar::defaults();
            let hidden: Vec<String> = toolbar::CUSTOMIZABLE
                .iter()
                .filter(|e| !order.contains(&e.key))
                .map(|e| e.key.to_string())
                .collect();
            c.set_toolbar_layout(order.iter().map(|k| k.to_string()).collect(), hidden);
        }
        self.reload();
    }

    /// How tall the pane's content is, so the dialog can place what goes under it. Measured against
    /// the *worst* case — every button in the palette — because the pane is a fixed size and a
    /// palette taller than its frame is clipped, not scrolled.
    pub fn height(&self, width: f64) -> f64 {
        let all: Vec<&'static str> = toolbar::CUSTOMIZABLE.iter().map(|e| e.key).collect();
        let rows = all.len().div_ceil(self.cols(width));
        self.palette_top() + HEADING_H + rows as f64 * (TILE_H + TILE_GAP)
    }

    // ---- geometry ----

    /// A row's height: the window's own title-bar band, read back off it, so an item in the pane is
    /// the size it will be in the window.
    fn row_h(&self) -> f64 {
        self.controller().map(|c| c.band_h()).unwrap_or(HEADER_H)
    }

    /// Heading and strip for group `g`, by index into [`toolbar::Group::ALL`].
    fn row_rect(&self, g: usize) -> NSRect {
        let w = self.frame().size.width;
        let pitch = HEADING_H + self.row_h() + ROW_GAP;
        rect(
            PAD_X,
            ROWS_Y + g as f64 * pitch + HEADING_H,
            (w - 2.0 * PAD_X).max(1.0),
            self.row_h(),
        )
    }

    fn heading_y(&self, g: usize) -> f64 {
        ROWS_Y + g as f64 * (HEADING_H + self.row_h() + ROW_GAP)
    }

    /// Where the palette's heading sits: under the last row.
    fn palette_top(&self) -> f64 {
        self.heading_y(toolbar::Group::ALL.len() - 1) + HEADING_H + self.row_h() + PALETTE_TOP
    }

    /// How many palette tiles fit on a row.
    fn cols(&self, width: f64) -> usize {
        (((width - 2.0 * PAD_X + TILE_GAP) / (TILE_W + TILE_GAP)).floor() as usize).max(1)
    }

    fn tile_rect(&self, i: usize) -> NSRect {
        let cols = self.cols(self.frame().size.width);
        let (col, row) = (i % cols, i / cols);
        rect(
            PAD_X + col as f64 * (TILE_W + TILE_GAP),
            self.palette_top() + HEADING_H + row as f64 * (TILE_H + TILE_GAP),
            TILE_W,
            TILE_H,
        )
    }

    /// Where each of `keys` sits inside `row`: one left-aligned run of abutting items.
    fn item_rects(&self, keys: &[&'static str], row: NSRect) -> Vec<NSRect> {
        let mut x = row.origin.x + ROW_PAD;
        keys.iter()
            .map(|_| {
                // An item viewer is the full height of the band, which is what the capsule and the
                // hit testing are both measured against.
                let r = rect(x, row.origin.y, ITEM_W, row.size.height);
                x += ITEM_W;
                r
            })
            .collect()
    }

    /// Row `g`'s list without the button being dragged out of it — what the run will look like once
    /// the drop lands, and therefore what the insertion point has to be measured against.
    fn preview_keys(&self, g: usize) -> Vec<&'static str> {
        let mut keys = self.ivars().rows.borrow()[g].clone();
        if let Press::Row(from, i) = self.ivars().press.get() {
            if self.ivars().dragging.get() && from == g && i < keys.len() {
                keys.remove(i);
            }
        }
        keys
    }

    fn hit(&self, p: NSPoint) -> Press {
        for g in 0..toolbar::Group::ALL.len() {
            let row = self.row_rect(g);
            if !contains(row, p) {
                continue;
            }
            let keys = self.ivars().rows.borrow()[g].clone();
            for (i, r) in self.item_rects(&keys, row).into_iter().enumerate() {
                if p.x >= r.origin.x && p.x <= r.origin.x + r.size.width {
                    return Press::Row(g, i);
                }
            }
            return Press::None;
        }
        for i in 0..self.ivars().palette.borrow().len() {
            if contains(self.tile_rect(i), p) {
                return Press::Palette(i);
            }
        }
        Press::None
    }

    /// Where the dragged button would land: `(row, index)`, or [`NO_DROP`] when the pointer is over
    /// neither row.
    fn drop_target(&self, p: NSPoint) -> (usize, usize) {
        for g in 0..toolbar::Group::ALL.len() {
            let row = self.row_rect(g);
            if !contains(row, p) {
                continue;
            }
            let keys = self.preview_keys(g);
            let rects = self.item_rects(&keys, row);
            let at = rects.iter().filter(|r| p.x > r.origin.x + r.size.width / 2.0).count();
            return (g, at);
        }
        NO_DROP
    }

    /// Apply the drag and push the result through to the toolbar. The one place the arrangement is
    /// committed, and only ever from a mouse-up.
    fn finish_drag(&self) {
        let press = self.ivars().press.get();
        let dragging = self.ivars().dragging.get();
        let (drop_row, drop_at) = self.ivars().drop_at.get();
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
            let mut rows = self.ivars().rows.borrow_mut();
            let mut palette = self.ivars().palette.borrow_mut();
            let landed = drop_row != NO_DROP.0;
            match press {
                Press::Row(g, i) if i < rows[g].len() => {
                    let key = rows[g].remove(i);
                    if landed {
                        let at = drop_at.min(rows[drop_row].len());
                        rows[drop_row].insert(at, key);
                    } else {
                        // Dragged out of both rows: removed, and offered back in the palette at the
                        // place the table would put it, not at the end of a growing pile.
                        insert_by_table(&mut palette, key);
                    }
                }
                Press::Palette(i) if i < palette.len() && landed => {
                    let key = palette.remove(i);
                    let at = drop_at.min(rows[drop_row].len());
                    rows[drop_row].insert(at, key);
                }
                // Dropped a palette tile outside the rows, or a press on nothing: no change.
                _ => {}
            }
        }
        let rows = self.ivars().rows.borrow();
        let order: Vec<String> =
            toolbar::flatten(&rows[0], &rows[1]).iter().map(|k| k.to_string()).collect();
        let mut hidden: Vec<String> = self.ivars().palette.borrow().iter().map(|k| k.to_string()).collect();
        // A key this build does not know is one a *newer* version hid, and it is not the
        // customizer's to drop: the editor only ever saw the buttons in this table, so anything
        // else is carried through untouched rather than rewritten out by the first drag. The
        // divider is not a button and never belongs in this set, whatever an older config says.
        hidden.extend(
            crate::settings::toolbar_hidden()
                .into_iter()
                .filter(|k| toolbar::entry(k).is_none() && k != toolbar::SPACE_KEY),
        );
        drop(rows);
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
        let dragging = self.ivars().dragging.get();

        let caption = unsafe { NSFont::systemFontOfSize_weight(11.0, NSFontWeightRegular) };
        let heading = unsafe { NSFont::systemFontOfSize_weight(11.0, NSFontWeightMedium) };
        let tile_font = unsafe { NSFont::systemFontOfSize_weight(10.0, NSFontWeightRegular) };
        let secondary = unsafe { NSColor::secondaryLabelColor() };

        draw_text(
            "Drag a button to arrange it, to the other row to move it there, or out to remove it.",
            NSPoint::new(PAD_X, 8.0),
            &caption,
            &secondary,
        );

        // ---- One row per group, each drawn as the window would draw it ----
        for (g, group) in toolbar::Group::ALL.iter().enumerate() {
            draw_text(group.label(), NSPoint::new(PAD_X, self.heading_y(g)), &heading, &secondary);

            let row = self.row_rect(g);
            // `ns_color`, not `ns_color_bg`: the opacity setting belongs to a window over a desktop,
            // and inside this panel it would show the panel's own background through the preview.
            round_fill(row, ROW_RADIUS, &ns_color(t.bg));
            round_stroke(inset(row, 0.5), ROW_RADIUS, 1.0, &ns_color(t.card_border()));

            let keys = self.preview_keys(g);
            let rects = self.item_rects(&keys, row);

            // Clipped to the row, because the pane is far narrower than a window: a long enough run
            // outgrows it, and the alternative to cutting it is drawing over the pane. Everything is
            // still laid out at full size — only the pixels are cut.
            save_state();
            clip(row);
            if let (Some(a), Some(b)) = (rects.first(), rects.last()) {
                // The capsule recent macOS draws behind a run of adjacent items. One row is one run,
                // which is exactly what makes the row break a capsule break in the window.
                let plate = rect(
                    a.origin.x,
                    row.origin.y + PLATTER_INSET,
                    b.origin.x + b.size.width - a.origin.x,
                    row.size.height - 2.0 * PLATTER_INSET,
                );
                let h = plate.size.height;
                round_fill(plate, h / 2.0, &ns_color(theme::mix(t.fg, t.bg, 0.88)));
            }
            for (k, r) in keys.iter().zip(rects.iter()) {
                self.draw_entry(k, *r, tone);
            }

            // The insertion caret, in the system accent color — the one mark here that must read the
            // same on every theme, so it is deliberately not theme-derived.
            let (drop_row, drop_at) = self.ivars().drop_at.get();
            if dragging && drop_row == g {
                let i = drop_at.min(rects.len());
                let x = match (i, rects.get(i.wrapping_sub(1))) {
                    (0, _) => row.origin.x + ROW_PAD,
                    (_, Some(r)) => r.origin.x + r.size.width,
                    (_, None) => row.origin.x + ROW_PAD,
                };
                let caret = rect(
                    x.round() - 1.0,
                    row.origin.y + PLATTER_INSET - 2.0,
                    2.0,
                    row.size.height - 2.0 * (PLATTER_INSET - 2.0),
                );
                round_fill(caret, 1.0, unsafe { &NSColor::controlAccentColor() });
            }
            restore_state();

            // An empty row is a drop target with nothing in it to say so.
            if keys.is_empty() && !(dragging && drop_row == g) {
                let attrs = make_attrs(&caption, Some(&ns_color(theme::mix(t.fg, t.bg, 0.6))));
                let s = NSString::from_str("Drag a button here");
                let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
                unsafe {
                    s.drawAtPoint_withAttributes(
                        NSPoint::new(
                            row.origin.x + ROW_PAD + 6.0,
                            row.origin.y + (row.size.height - sz.height) / 2.0,
                        ),
                        Some(&attrs),
                    )
                };
            }
            // …and a run wider than the pane says so at the cut, rather than looking sliced.
            if rects.last().map(|r| r.origin.x + r.size.width > row.origin.x + row.size.width).unwrap_or(false) {
                let attrs = make_attrs(&caption, Some(&ns_color(theme::mix(t.fg, t.bg, 0.55))));
                let s = NSString::from_str("…");
                let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
                unsafe {
                    s.drawAtPoint_withAttributes(
                        NSPoint::new(
                            row.origin.x + row.size.width - sz.width - 4.0,
                            row.origin.y + (row.size.height - sz.height) / 2.0,
                        ),
                        Some(&attrs),
                    )
                };
            }
        }

        // ---- The palette: what is in neither row ----
        let palette = self.ivars().palette.borrow().clone();
        draw_text("Not in the toolbar", NSPoint::new(PAD_X, self.palette_top()), &heading, &secondary);
        let skip = match (dragging, self.ivars().press.get()) {
            // The tile being dragged is under the pointer instead of in its slot.
            (true, Press::Palette(i)) => Some(i),
            _ => None,
        };
        for (i, k) in palette.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            let r = self.tile_rect(i);
            let icon = rect(r.origin.x + (TILE_W - ICON_BOX) / 2.0, r.origin.y + 8.0, ICON_BOX, ICON_BOX);
            self.draw_entry(k, icon, tone);
            if let Some(e) = toolbar::entry(k) {
                draw_centered(e.label, rect(r.origin.x, r.origin.y + 30.0, TILE_W, 14.0), &tile_font, &secondary);
            }
        }
        if palette.is_empty() {
            draw_text(
                "Every button is in the toolbar.",
                NSPoint::new(PAD_X, self.palette_top() + HEADING_H + 4.0),
                &tile_font,
                &unsafe { NSColor::tertiaryLabelColor() },
            );
        }

        // ---- The button under the pointer, drawn last so it rides over everything ----
        if dragging {
            let key = match self.ivars().press.get() {
                Press::Row(g, i) => self.ivars().rows.borrow()[g].get(i).copied(),
                Press::Palette(i) => palette.get(i).copied(),
                Press::None => None,
            };
            if let Some(k) = key {
                let p = self.ivars().cur.get();
                let r = rect(p.x - ITEM_W / 2.0, p.y - PLATE_H / 2.0, ITEM_W, PLATE_H);
                // A faint plate under it, so the icon stays legible over the panel's own background
                // as well as over a themed row.
                let plate = unsafe { NSColor::controlAccentColor().colorWithAlphaComponent(0.16) };
                round_fill(r, PLATE_H / 2.0, &plate);
                self.draw_entry(k, r, tone);
            }
        }
    }

    /// One button's icon, centered in `r`.
    fn draw_entry(&self, key: &str, r: NSRect, tone: theme::Rgb) {
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

/// Push/pop the drawing state around a clip, on the context `drawRect:` is running in — there is no
/// `NSView` API for a scoped clip, and the state has to be restored or the clip leaks into whatever
/// is drawn next in the same pass.
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

/// A line of the panel's own text — system label colors, unlike a row's contents, which are the
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

//! term-core — terminal emulation core (pure logic, zero OS dependencies).
//!
//! Step 3: replace the minimal `feed` from step 2 (which only handled
//! printable ASCII / line feed / carriage return) with a real VT/ANSI parsing
//! state machine (modeled on Paul Williams' VT500 parser state diagram:
//! ground / escape / csi / osc …). It now correctly handles:
//!   - SGR (colors plus bold/italic/underline/inverse) → applied to the current "pen", written into cells;
//!   - cursor movement CUU/CUD/CUF/CUB, CUP/HVP, CHA/VPA;
//!   - erase ED/EL/ECH, insert/delete ICH/DCH/IL/DL;
//!   - scroll region DECSTBM and IND/RI/NEL, SU/SD;
//!   - save/restore cursor DECSC/DECRC and CSI s/u;
//!   - DEC private modes `?…h/l` (autowrap, cursor visibility, alt screen, app cursor keys) are
//!     applied; bracketed paste (2004) and alternate scroll (1007) are tracked for the input layer
//!     via `bracketed_paste()` / `alt_scroll_keys()`;
//!     others are acknowledged and swallowed, no longer leaking out as literal text;
//!   - mouse tracking (DECSET 1000/1002/1003) in both the legacy and the SGR (1006) encoding: the
//!     input layer hands each event to `mouse_report_bytes()`, which gates it on the mode the
//!     application asked for and encodes it;
//!   - OSC title (`]0;…`) collected into `title`;
//!   - OSC 7 cwd reporting, percent-decoded;
//!   - alternate screen buffer (DECSET 47/1047/1049), used by vim/less/htop/man;
//!   - device status reports (DSR `CSI 6n`/`5n`) and device attributes (DA `CSI c`), queued via
//!     `take_replies()` for the caller to write back to the PTY;
//!   - UTF-8 multibyte character decoding;
//!   - the DEC Special Graphics charset (`ESC ( 0` / `ESC ) 0` plus SI/SO), which is what every
//!     ncurses box border is actually made of;
//!   - tab stops: HT against a per-column stop table, edited by HTS/TBC and walked by CHT/CBT;
//!   - IRM (ANSI mode 4), where printing opens a gap instead of overwriting.
//!
//!   - a scrollback buffer: lines that scroll off the top of the main screen are kept in
//!     `history` (capped at `HISTORY_MAX`), and the renderer reads through `view_cell()` so the
//!     viewport can be scrolled back with `scroll_view()`.
//!
//! Still not implemented (left for later milestones): reflowing the scrollback when the window is
//! resized.

use std::collections::VecDeque;

/// A single screen cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8, // see the BOLD/ITALIC/… bit definitions below
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

/// A blank cell, returned by `view_cell()` for columns past the end of a (trimmed) history row.
const BLANK: Cell = Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 };

/// DEC Special Graphics, the line-drawing set selected by `ESC ( 0`, covering `_` (0x5f) through
/// `~` (0x7e) in order. The middle stretch is the box-drawing alphabet: `lqk` / `x x` / `mqj` draws
/// a rectangle, which is what every ncurses frame is made of underneath.
#[rustfmt::skip]
const DEC_GRAPHICS: [char; 32] = [
    ' ',      // _  blank
    '◆',      // `  diamond
    '▒',      // a  checkerboard
    '␉',      // b  HT
    '␌',      // c  FF
    '␍',      // d  CR
    '␊',      // e  LF
    '°',      // f  degree
    '±',      // g  plus/minus
    '␤',      // h  NL
    '␋',      // i  VT
    '┘',      // j  lower right corner
    '┐',      // k  upper right corner
    '┌',      // l  upper left corner
    '└',      // m  lower left corner
    '┼',      // n  crossing lines
    '⎺',      // o  horizontal scan line 1
    '⎻',      // p  horizontal scan line 3
    '─',      // q  horizontal scan line 5 (the plain horizontal rule)
    '⎼',      // r  horizontal scan line 7
    '⎽',      // s  horizontal scan line 9
    '├',      // t  left tee
    '┤',      // u  right tee
    '┴',      // v  bottom tee
    '┬',      // w  top tee
    '│',      // x  vertical line
    '≤',      // y  less than or equal
    '≥',      // z  greater than or equal
    'π',      // {  pi
    '≠',      // |  not equal
    '£',      // }  pound sterling
    '·',      // ~  centered dot
];

/// How much mouse activity the running application asked to be told about. Ordered least to most,
/// so the effective level is simply the largest one currently enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MouseMode {
    /// No tracking: the mouse belongs to the terminal (text selection).
    Off,
    /// DECSET 1000 — button presses and releases.
    Press,
    /// DECSET 1002 — the above, plus motion while a button is held.
    Drag,
    /// DECSET 1003 — the above, plus motion with no button held.
    Any,
}

/// A mouse button, as the protocol counts them. The wheel is reported as a button too, which is
/// why its notches live here rather than in a separate event kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

impl MouseButton {
    /// The button field of a mouse report. The wheel occupies a separate block starting at 64.
    fn code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
            MouseButton::WheelLeft => 66,
            MouseButton::WheelRight => 67,
        }
    }

    /// Whether this is a wheel notch, which is reported as a press with no matching release.
    fn is_wheel(self) -> bool {
        matches!(
            self,
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    }
}

/// Modifier keys held during a mouse event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// A mouse event to be reported to the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press(MouseButton),
    Release(MouseButton),
    /// Pointer motion: `Some(button)` while a button is held (a drag), `None` otherwise.
    Motion(Option<MouseButton>),
}

/// Maximum number of scrolled-off lines retained per grid. Rows are stored with their trailing
/// blank cells trimmed, so a typical shell session costs far less than `HISTORY_MAX * cols`.
pub const HISTORY_MAX: usize = 5000;

// Cell attribute bits.
pub const BOLD: u8 = 1 << 0;
pub const ITALIC: u8 = 1 << 1;
pub const UNDERLINE: u8 = 1 << 2;
pub const INVERSE: u8 = 1 << 3;
/// Trailing (right) half of a double-width character (e.g. CJK). The left cell holds the glyph;
/// this cell is a placeholder so the grid column count matches the display width. Its `ch` is `'\0'`
/// and renderers/`to_lines()` skip it. Set on the second cell of every wide glyph.
pub const WIDE_TRAILER: u8 = 1 << 4;

/// Display width of a character in terminal cells: 0 (combining/zero-width), 1 (normal), or 2
/// (East Asian wide / fullwidth). A dependency-free approximation of Unicode East Asian Width —
/// covers the CJK/Kana/Hangul/fullwidth blocks that matter in practice, not the full UAX #11 table.
pub fn char_width(ch: char) -> usize {
    let c = ch as u32;
    // Zero-width: combining marks and the common zero-width spaces/joiners.
    if matches!(c,
        0x0300..=0x036F | // combining diacritical marks
        0x200B..=0x200F | // zero-width space/joiner/marks
        0xFE00..=0xFE0F | // variation selectors
        0xFEFF            // zero-width no-break space (BOM)
    ) {
        return 0;
    }
    // Double-width: East Asian wide and fullwidth ranges.
    let wide = matches!(c,
        0x1100..=0x115F | // Hangul Jamo
        0x2E80..=0x303E | // CJK radicals, Kangxi, CJK symbols & punctuation
        0x3041..=0x33FF | // Hiragana, Katakana, Bopomofo, Hangul Compat Jamo, enclosed CJK, …
        0x3400..=0x4DBF | // CJK Unified Ideographs Extension A
        0x4E00..=0x9FFF | // CJK Unified Ideographs
        0xA000..=0xA4CF | // Yi Syllables/Radicals
        0xA960..=0xA97F | // Hangul Jamo Extended-A
        0xAC00..=0xD7A3 | // Hangul Syllables
        0xF900..=0xFAFF | // CJK Compatibility Ideographs
        0xFE10..=0xFE19 | // vertical forms
        0xFE30..=0xFE6F | // CJK compatibility forms, small form variants
        0xFF00..=0xFF60 | // fullwidth forms
        0xFFE0..=0xFFE6 | // fullwidth signs
        0x1F300..=0x1FAFF | // emoji & pictographs (mostly wide)
        0x20000..=0x3FFFD   // CJK Unified Ideographs Extension B and beyond
    );
    if wide {
        2
    } else {
        1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),     // 0-255 palette (0-7 normal, 8-15 bright, 16-255 color cube/grayscale)
    Rgb(u8, u8, u8), // true color
}

/// Parser state (a reduced subset of Williams' VT500 state diagram).
#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Escape,
    EscInt,   // intermediate bytes after ESC (charset selection, etc.), ignored wholesale
    CsiEntry, // just consumed ESC [
    CsiParam, // collecting parameters
    CsiInt,   // CSI intermediate bytes, ignored wholesale
    CsiIgnore,
    Osc,       // ESC ] string, up to BEL or ST
    DcsIgnore, // ESC P …, ignored wholesale until ST
}

/// A fixed-size screen grid plus an embedded VT parsing state machine.
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    pub cursor: (usize, usize), // (col, row)
    // Deferred wrap: after filling the last column the cursor stays put and this
    // bit is set; the actual wrap is deferred until the next printable character
    // arrives, matching xterm semantics.
    pending_wrap: bool,

    // ---- Current pen (SGR state): applied to newly written cells ----
    pen_fg: Color,
    pen_bg: Color,
    pen_flags: u8,

    // ---- Saved cursor (DECSC / CSI s): position + pen ----
    saved: Option<(usize, usize, Color, Color, u8)>,

    // ---- Alternate screen (alt screen, DECSET 47/1047/1049) ----
    // `inactive` holds the buffer not currently displayed; it is swapped with
    // `cells` when entering or leaving the alt screen.
    inactive: Vec<Cell>,
    alt: bool,
    // Cursor save dedicated to 1049 (kept separate from DECSC's `saved`, they don't interfere).
    alt_saved: Option<(usize, usize, Color, Color, u8)>,

    // ---- Scroll region (rows, inclusive on both ends), defaults to the full screen ----
    scroll_top: usize,
    scroll_bot: usize,

    // ---- Scrollback ----
    // Lines that scrolled off the top of the main screen, oldest first, capped at HISTORY_MAX.
    // Rows are trimmed of trailing blank cells; `view_cell` substitutes BLANK past a row's end.
    // The alt screen never contributes here (vim/less redraw themselves; their scrolling is not history).
    history: VecDeque<Vec<Cell>>,
    // Total number of lines that have ever scrolled off the top, *including* those since evicted
    // from the capped deque. Monotonic — it is what makes `buf_cell`'s coordinates stable: the
    // deque's own indices shift down by one on every eviction at HISTORY_MAX, so a coordinate
    // stored by the caller (the mouse selection) would silently slide onto other text.
    // `history[i]` is absolute row `scrolled - history.len() + i`; screen row `r` is `scrolled + r`.
    scrolled: usize,
    // How many lines the viewport is scrolled back from the live bottom. 0 = following the output.
    // Always <= history.len().
    view_offset: usize,

    // ---- Modes ----
    autowrap: bool,
    cursor_visible: bool,
    // DECCKM: application cursor keys mode. When on, arrow keys send ESC O x; when off, ESC [ x.
    app_cursor_keys: bool,
    // DECSET 2004: bracketed paste. When on, the caller should wrap pasted text in
    // ESC[200~ ... ESC[201~ so the program can tell it apart from typed keystrokes.
    bracketed_paste: bool,
    // DECSET 1007: alternate scroll. The alt screen has no scrollback of its own, so while it is
    // active the input layer turns wheel/trackpad scrolling into cursor-key presses instead —
    // otherwise the wheel is dead in every full-screen app (less, vim, TUIs). On by default, as in
    // most modern terminals; an application can turn it off with CSI ? 1007 l.
    alt_scroll: bool,
    // DECSET 1000/1002/1003: mouse tracking, and DECSET 1006: the SGR encoding for it. The three
    // tracking modes are kept as separate flags rather than one level, because they are separate
    // modes in xterm: turning 1000 off must not cancel a 1002 that is still on, which is exactly
    // what an application does when it switches between them. `mouse_mode()` derives the effective
    // level from all three; `mouse_report_bytes()` does the encoding.
    mouse_press: bool, // 1000: presses and releases
    mouse_drag: bool,  // 1002: the above, plus motion while a button is held
    mouse_any: bool,   // 1003: the above, plus motion with no button held
    mouse_sgr: bool,   // 1006: SGR encoding, which lifts the legacy 223-column coordinate limit

    // ANSI mode 4 (IRM): printing pushes the rest of the line right instead of overwriting it.
    // Line editors and `less` use it to open a gap without redrawing the whole row.
    insert_mode: bool,

    // ---- Horizontal tab stops, one flag per column ----
    // Power-on layout is every eighth column; HTS/TBC edit it and `resize` extends it. Kept as a
    // per-column flag rather than a stride because an application may put a stop anywhere, which
    // is the whole point of HTS.
    tab_stops: Vec<bool>,

    // ---- Character sets ----
    // Which of G0/G1 currently holds DEC Special Graphics (designated by `ESC ( 0` / `ESC ) 0`),
    // and which of the two SI/SO has selected. Applications commonly park the line-drawing set in
    // G1 and flip it in and out with SO/SI around each run of box characters.
    g0_graphics: bool,
    g1_graphics: bool,
    shift_out: bool,
    // The intermediate byte that opened the current `EscInt` sequence, needed because its final
    // byte means nothing on its own — `0` designates the graphics set into G0 or G1 depending on it.
    esc_intermediate: u8,

    // ---- Window title received via OSC ----
    pub title: String,
    // ---- Current working directory reported via OSC 7 (local path parsed from a file:// URL) ----
    cwd: String,

    // ---- Pending replies to write back to the PTY (DSR/DA) ----
    // This layer has no I/O of its own; the caller drains this via `take_replies()` after each
    // `feed()` and writes it to the master fd. Without a reply, programs that block waiting for
    // one (cursor-position queries, device-attribute probes used by some shells/tmux/vim) hang.
    replies: Vec<u8>,

    // ---- Parser state machine internals ----
    state: State,
    params: Vec<u16>,
    csi_cur: u32,   // parameter currently being accumulated
    private: u8,    // CSI private prefix byte ('?', '>', etc.), 0 if none
    osc: Vec<u8>,   // OSC string buffer
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor: (0, 0),
            pending_wrap: false,
            pen_fg: Color::Default,
            pen_bg: Color::Default,
            pen_flags: 0,
            saved: None,
            inactive: vec![Cell::default(); cols * rows],
            alt: false,
            alt_saved: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            history: VecDeque::new(),
            scrolled: 0,
            view_offset: 0,
            autowrap: true,
            cursor_visible: true,
            app_cursor_keys: false,
            bracketed_paste: false,
            alt_scroll: true,
            mouse_press: false,
            mouse_drag: false,
            mouse_any: false,
            mouse_sgr: false,
            insert_mode: false,
            tab_stops: Self::default_tab_stops(cols),
            g0_graphics: false,
            g1_graphics: false,
            shift_out: false,
            esc_intermediate: 0,
            title: String::new(),
            cwd: String::new(),
            replies: Vec::new(),
            state: State::Ground,
            params: Vec::new(),
            csi_cur: 0,
            private: 0,
            osc: Vec::new(),
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    pub fn cell(&self, col: usize, row: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    pub fn cell_mut(&mut self, col: usize, row: usize) -> &mut Cell {
        &mut self.cells[row * self.cols + col]
    }

    // ===================== Scrollback viewport =====================

    /// The cell shown at viewport position (col, row) — the accessor the renderer must use.
    ///
    /// With `view_offset == 0` this is exactly `cell(col, row)`. Scrolled back by `o` lines, the
    /// viewport shows the last `o` history lines on top, followed by the first `rows - o` screen
    /// rows. History rows are stored trimmed, so columns past a row's end read as blank.
    pub fn view_cell(&self, col: usize, row: usize) -> &Cell {
        let o = self.view_offset;
        if row < o {
            // history.len() >= o is an invariant of scroll_view/push_history, so this can't underflow.
            let line = &self.history[self.history.len() - o + row];
            return line.get(col).unwrap_or(&BLANK);
        }
        self.cell(col, row - o)
    }

    /// Number of lines retained in the scrollback.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// First virtual-buffer row still retained: rows below this scrolled off and were evicted from
    /// the capped history.
    pub fn buf_top(&self) -> usize {
        // `history` only ever grows in `scroll_line_into_history`, which bumps `scrolled` in the
        // same breath, so it can never outrun it. Worth asserting: this subtraction runs on every
        // `buf_cell` (so on every frame with a live selection), and the release profile is
        // `panic = "abort"` — an underflow here would take the whole app down, not just the
        // selection. A new push site that forgets the counter trips this in debug builds.
        debug_assert!(self.scrolled >= self.history.len());
        self.scrolled - self.history.len()
    }

    /// One past the last virtual-buffer row (the bottom screen row is `buf_end() - 1`).
    pub fn buf_end(&self) -> usize {
        self.scrolled + self.rows
    }

    /// The virtual-buffer row currently at the top of the viewport. The renderer and the mouse
    /// selection both need it to convert between the two coordinate spaces, and deriving it here
    /// keeps that one expression from being open-coded at every call site.
    pub fn view_base(&self) -> usize {
        self.scrolled - self.view_offset
    }

    /// The cell at a virtual-buffer row. Rows are *absolute*: counted from the first line ever
    /// printed, not from the head of the deque, so they stay attached to their text no matter how
    /// much history is later evicted or cleared. `buf_top()..scrolled` are scrollback lines and
    /// `scrolled..buf_end()` the screen rows. This is the viewport-independent counterpart of
    /// `view_cell`, for state that must survive the viewport moving (the mouse selection).
    ///
    /// `buf_cell(col, view_base() + row) == view_cell(col, row)` for every visible `row`.
    /// Coordinates outside the buffer — evicted below, past the screen above — read as blank
    /// rather than panicking, so a selection left over from before a reflow, an eviction or an ED3
    /// can never take the process down.
    pub fn buf_cell(&self, col: usize, row: usize) -> &Cell {
        if col >= self.cols || row < self.buf_top() {
            return &BLANK;
        }
        if row < self.scrolled {
            return self.history[row - self.buf_top()].get(col).unwrap_or(&BLANK);
        }
        let r = row - self.scrolled;
        if r >= self.rows {
            return &BLANK;
        }
        self.cell(col, r)
    }

    /// How many lines the viewport is scrolled back from the live bottom (0 = following output).
    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    /// Scroll the viewport by `lines` (positive = back toward older output). Clamped to the
    /// available history. No-op on the alt screen, which has no scrollback of its own.
    /// Returns whether the offset actually changed (i.e. whether a redraw is needed).
    pub fn scroll_view(&mut self, lines: isize) -> bool {
        if self.alt {
            return false;
        }
        let max = self.history.len() as isize;
        let next = (self.view_offset as isize + lines).clamp(0, max) as usize;
        let changed = next != self.view_offset;
        self.view_offset = next;
        changed
    }

    /// Snap the viewport back to the live bottom (on keypress, clear, alt-screen switch).
    /// Returns whether the offset actually changed.
    pub fn scroll_to_bottom(&mut self) -> bool {
        let changed = self.view_offset != 0;
        self.view_offset = 0;
        changed
    }

    /// Whether the cursor is shown (DECTCEM), for the rendering layer's reference.
    /// Current working directory (reported via OSC 7; empty when the shell hasn't reported it).
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Drain any DSR/DA replies queued since the last call. The caller must write these bytes
    /// back to the PTY master fd after each `feed()` — see the `replies` field's doc comment.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Whether application cursor keys mode (DECCKM) is active, so the input layer can pick the arrow-key escape prefix.
    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    /// Whether bracketed paste mode (DECSET 2004) is active, so the input layer knows whether to
    /// wrap a paste in `ESC[200~ ... ESC[201~`.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Whether the alternate screen is active (a full-screen application is running).
    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// The effective mouse tracking level asked for by the application (DECSET 1000/1002/1003).
    ///
    /// The three modes are independent flags, so the level is the most permissive one currently
    /// on: an application that turns 1002 on and then 1000 off still wants drag reports.
    pub fn mouse_mode(&self) -> MouseMode {
        if self.mouse_any {
            MouseMode::Any
        } else if self.mouse_drag {
            MouseMode::Drag
        } else if self.mouse_press {
            MouseMode::Press
        } else {
            MouseMode::Off
        }
    }

    /// Whether an application asked for mouse tracking at all.
    ///
    /// This is *not* on its own the test for "the application owns the mouse": mode 1000 reports
    /// presses and releases but no motion, so a drag under 1000 must still select text or the user
    /// gets neither a selection nor a working drag. The input layer gates each event kind on
    /// `mouse_mode()` instead — press/release on `!= Off`, motion on `>= Drag`.
    pub fn mouse_report(&self) -> bool {
        self.mouse_mode() != MouseMode::Off
    }

    /// The bytes to send for a mouse event at cell (`col`, `row`), or `None` when the current
    /// tracking mode does not want this event (which is the common case — an application in mode
    /// 1000 gets no motion at all, and no mode reports a wheel release).
    ///
    /// Coordinates are zero-based cell indices; the wire format is one-based.
    pub fn mouse_report_bytes(
        &self,
        event: MouseEvent,
        mods: MouseMods,
        col: usize,
        row: usize,
    ) -> Option<Vec<u8>> {
        let mode = self.mouse_mode();
        if mode == MouseMode::Off {
            return None;
        }

        // Which events each mode wants, and the button field each carries. Note that a wheel notch
        // is reported as a button *press* with no matching release, as in xterm — hence no wheel
        // arm under `Release`. Motion with no button held uses button code 3, the same "no button"
        // value a legacy release uses.
        const NO_BUTTON: u8 = 3;
        let (base, motion, release) = match event {
            MouseEvent::Press(b) => (b.code(), false, false),
            MouseEvent::Release(b) if b.is_wheel() => return None,
            MouseEvent::Release(b) => (b.code(), false, true),
            MouseEvent::Motion(Some(b)) if mode >= MouseMode::Drag => (b.code(), true, false),
            MouseEvent::Motion(None) if mode == MouseMode::Any => (NO_BUTTON, true, false),
            MouseEvent::Motion(_) => return None,
        };

        let mut code = base
            + if motion { 32 } else { 0 }
            + if mods.shift { 4 } else { 0 }
            + if mods.alt { 8 } else { 0 }
            + if mods.ctrl { 16 } else { 0 };

        if self.mouse_sgr {
            // SGR (1006): `CSI < code ; col ; row` and a final byte that carries press vs release,
            // so the real button survives a release and the coordinates are unbounded.
            let final_byte = if release { 'm' } else { 'M' };
            return Some(
                format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, final_byte).into_bytes(),
            );
        }

        // Legacy (X10): `CSI M` and three bytes biased by 32. A release cannot name its button
        // here — the encoding has no room for the distinction, so every release is button 3.
        if release {
            code = NO_BUTTON
                + if mods.shift { 4 } else { 0 }
                + if mods.alt { 8 } else { 0 }
                + if mods.ctrl { 16 } else { 0 };
        }
        // A byte tops out at 255, so this encoding cannot express a coordinate past 223. We clamp
        // rather than drop the report: an application stuck on the legacy encoding in a window
        // wider than 223 columns is better served by a click at the last column it can name than
        // by a mouse that silently dies over the right-hand edge of the window.
        let cx = (col + 1).min(223) as u8 + 32;
        let cy = (row + 1).min(223) as u8 + 32;
        Some(vec![0x1b, b'[', b'M', code + 32, cx, cy])
    }

    /// The byte sequence a wheel scroll of `lines` should send while the alternate screen is up,
    /// or `None` when the wheel must not be translated (main screen, or DECSET 1007 turned off).
    ///
    /// Positive `lines` means scrolling back toward older output, i.e. cursor up. The count is
    /// capped at one screenful so a fast flick cannot flood the shell with keystrokes.
    ///
    /// Mouse tracking wins: an application that asked for it gets the wheel as a real mouse report
    /// (`mouse_report_bytes`) instead, and synthesizing cursor keys on top of that would feed it
    /// phantom keystrokes it never asked for.
    pub fn alt_scroll_keys(&self, lines: isize) -> Option<Vec<u8>> {
        if !self.alt || !self.alt_scroll || lines == 0 || self.mouse_report() {
            return None;
        }
        let seq: &[u8] = match (lines > 0, self.app_cursor_keys) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        let n = (lines.unsigned_abs()).min(self.rows);
        Some(seq.repeat(n))
    }

    /// Feed bytes: advance the parsing state machine one byte at a time.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => self.ground(b),
            State::Escape => self.escape(b),
            State::EscInt => self.esc_int(b),
            State::CsiEntry => self.csi_entry(b),
            State::CsiParam => self.csi_param(b),
            State::CsiInt => self.csi_int(b),
            State::CsiIgnore => self.csi_ignore(b),
            State::Osc => self.osc(b),
            State::DcsIgnore => self.dcs(b),
        }
    }

    // ===================== Ground (normal state) =====================

    fn ground(&mut self, b: u8) {
        if self.utf8_need > 0 {
            self.utf8_collect(b);
            return;
        }
        match b {
            0x1b => self.enter_escape(),
            0x00..=0x1f => self.execute(b),
            0x20..=0x7e => self.print(b as char),
            0x7f => {} // DEL ignored
            _ => self.utf8_begin(b),
        }
    }

    fn utf8_begin(&mut self, b: u8) {
        let need = if b >= 0xf0 {
            4
        } else if b >= 0xe0 {
            3
        } else if b >= 0xc0 {
            2
        } else {
            0 // a stray continuation byte 0x80-0xbf: invalid
        };
        if need == 0 {
            self.print('\u{fffd}');
            return;
        }
        self.utf8_buf[0] = b;
        self.utf8_len = 1;
        self.utf8_need = need;
    }

    fn utf8_collect(&mut self, b: u8) {
        if b & 0xc0 == 0x80 && self.utf8_len < self.utf8_need {
            self.utf8_buf[self.utf8_len] = b;
            self.utf8_len += 1;
            if self.utf8_len == self.utf8_need {
                let ch = std::str::from_utf8(&self.utf8_buf[..self.utf8_len])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('\u{fffd}');
                self.utf8_need = 0;
                self.utf8_len = 0;
                self.print(ch);
            }
        } else {
            // Continuation broke off: treat the previous character as corrupt and reprocess the current byte in the normal state.
            self.utf8_need = 0;
            self.utf8_len = 0;
            self.print('\u{fffd}');
            self.ground(b);
        }
    }

    // ===================== Tab stops =====================

    /// The column HT moves to from `col`: the next stop to its right, or the last column when there
    /// is none. Clearing every stop therefore parks HT at the right margin rather than doing
    /// nothing, which is what a terminal with no stops left should do.
    fn next_tab_stop(&self, col: usize) -> usize {
        let last = self.cols.saturating_sub(1);
        ((col + 1)..self.cols).find(|&c| self.tab_stops[c]).unwrap_or(last)
    }

    /// The column CBT moves to from `col`: the next stop to its left, or column 0.
    fn prev_tab_stop(&self, col: usize) -> usize {
        (0..col).rev().find(|&c| self.tab_stops[c]).unwrap_or(0)
    }

    /// CHT / CBT: move forward or back `n` tab stops.
    fn tab_move(&mut self, n: usize, forward: bool) {
        self.pending_wrap = false;
        for _ in 0..n {
            self.cursor.0 = if forward {
                self.next_tab_stop(self.cursor.0)
            } else {
                self.prev_tab_stop(self.cursor.0)
            };
        }
    }

    /// TBC: clear the stop under the cursor (`0`) or every stop (`3`).
    fn clear_tab_stop(&mut self, mode: u16) {
        match mode {
            0 => {
                let c = self.cursor.0;
                if c < self.tab_stops.len() {
                    self.tab_stops[c] = false;
                }
            }
            3 => self.tab_stops.iter_mut().for_each(|s| *s = false),
            _ => {}
        }
    }

    /// The power-on stop layout: every eighth column, which is what HT used to hardcode.
    fn default_tab_stops(cols: usize) -> Vec<bool> {
        (0..cols).map(|c| c > 0 && c % 8 == 0).collect()
    }

    /// Translate a character through the active graphic set.
    ///
    /// Only DEC Special Graphics does anything here, and only over `_` to `~`: that block is where
    /// it replaces ASCII punctuation and lowercase with the box-drawing pieces, so every ncurses
    /// border, `tree` branch and `mc` panel is really `ESC ( 0` plus the letters `qwxlkmjntuv`.
    /// Bytes outside the block, and anything that arrived as multi-byte UTF-8, are unaffected.
    fn map_charset(&self, ch: char) -> char {
        let graphics = if self.shift_out { self.g1_graphics } else { self.g0_graphics };
        if !graphics {
            return ch;
        }
        let c = ch as u32;
        if !(0x5f..=0x7e).contains(&c) {
            return ch;
        }
        DEC_GRAPHICS[(c - 0x5f) as usize]
    }

    fn print(&mut self, ch: char) {
        let ch = self.map_charset(ch);
        let w = char_width(ch);
        if w == 0 {
            // Zero-width (combining marks, etc.): the minimal core does not compose them onto the
            // previous cell — drop them rather than let them consume a column.
            return;
        }
        if self.pending_wrap {
            self.cursor.0 = 0;
            self.linefeed();
            self.pending_wrap = false;
        }
        // A double-width glyph can't straddle the right margin: if only one column is left, wrap to
        // the next line first so the pair stays together (matches how the shell lays out CJK).
        if w == 2 && self.cursor.0 + 1 >= self.cols && self.autowrap {
            self.cursor.0 = 0;
            self.linefeed();
        }
        // IRM: make room by pushing the rest of the line right, instead of overwriting what is
        // already there. A wide glyph needs both of its columns.
        if self.insert_mode {
            self.insert_chars(w);
        }
        let (c, r) = self.cursor;
        let (fg, bg, fl) = (self.pen_fg, self.pen_bg, self.pen_flags);
        {
            let cell = self.cell_mut(c, r);
            cell.ch = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = fl;
        }
        // Second half of a wide glyph: a placeholder cell the renderer/to_lines() skip.
        if w == 2 && c + 1 < self.cols {
            let cell = self.cell_mut(c + 1, r);
            cell.ch = '\0';
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = fl | WIDE_TRAILER;
        }
        if c + w >= self.cols {
            // Stay in the last column; only set the deferred-wrap bit if autowrap is on.
            if self.autowrap {
                self.pending_wrap = true;
            }
        } else {
            self.cursor.0 = c + w;
        }
    }

    /// C0 control characters.
    fn execute(&mut self, b: u8) {
        match b {
            0x08 => {
                // BS
                self.pending_wrap = false;
                if self.cursor.0 > 0 {
                    self.cursor.0 -= 1;
                }
            }
            0x09 => {
                // HT
                self.pending_wrap = false;
                self.cursor.0 = self.next_tab_stop(self.cursor.0);
            }
            0x0e => self.shift_out = true,  // SO: select G1 into GL
            0x0f => self.shift_out = false, // SI: select G0 into GL
            0x0a | 0x0b | 0x0c => self.linefeed(), // LF / VT / FF
            0x0d => {
                // CR
                self.pending_wrap = false;
                self.cursor.0 = 0;
            }
            _ => {} // BEL (0x07) and others: ignored
        }
    }

    fn enter_escape(&mut self) {
        self.state = State::Escape;
    }

    // ===================== Escape family =====================

    fn escape(&mut self, b: u8) {
        match b {
            0x1b => {}                        // consecutive ESC: stay in Escape
            0x18 | 0x1a => self.state = State::Ground, // CAN / SUB: abort
            0x00..=0x1f => self.execute(b),   // C0 embedded in an escape is still executed immediately
            b'[' => {
                self.csi_reset();
                self.state = State::CsiEntry;
            }
            b']' => {
                self.osc.clear();
                self.state = State::Osc;
            }
            b'P' => self.state = State::DcsIgnore,
            0x20..=0x2f => {
                // Intermediate byte; the final byte that follows is only meaningful together with
                // it (`ESC ( 0` designates G0, `ESC ) 0` designates G1), so remember which one.
                self.esc_intermediate = b;
                self.state = State::EscInt;
            }
            0x30..=0x7e => {
                self.esc_dispatch(b);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn esc_int(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x20..=0x2f => self.esc_intermediate = b, // keep consuming intermediate bytes; the last one wins
            _ => {
                // Final byte. Only character-set designation is acted on: `0` is DEC Special
                // Graphics (the line-drawing set every ncurses box is made of), anything else is
                // treated as a return to ASCII. Other intermediates are still ignored wholesale.
                match self.esc_intermediate {
                    b'(' => self.g0_graphics = b == b'0',
                    b')' => self.g1_graphics = b == b'0',
                    _ => {}
                }
                self.state = State::Ground;
            }
        }
    }

    fn esc_dispatch(&mut self, b: u8) {
        match b {
            b'7' => self.save_cursor(),       // DECSC
            b'8' => self.restore_cursor(),    // DECRC
            b'D' => self.linefeed(),          // IND
            b'M' => self.reverse_index(),     // RI
            b'E' => {
                // NEL
                self.cursor.0 = 0;
                self.linefeed();
            }
            b'H' => {
                // HTS: set a tab stop at the cursor's column
                let c = self.cursor.0;
                if c < self.tab_stops.len() {
                    self.tab_stops[c] = true;
                }
            }
            b'c' => self.hard_reset(),        // RIS
            b'=' | b'>' => {}                 // keypad application/numeric mode: ignored
            _ => {}
        }
    }

    // ===================== CSI family =====================

    fn csi_reset(&mut self) {
        self.params.clear();
        self.csi_cur = 0;
        self.private = 0;
    }

    fn push_param(&mut self) {
        self.params.push(self.csi_cur.min(65535) as u16);
        self.csi_cur = 0;
    }

    fn csi_entry(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x30..=0x39 => {
                self.csi_cur = (b - 0x30) as u32;
                self.state = State::CsiParam;
            }
            b';' => {
                self.push_param();
                self.state = State::CsiParam;
            }
            0x3c..=0x3f => {
                // private prefix < = > ?
                self.private = b;
                self.state = State::CsiParam;
            }
            0x3a => self.state = State::CsiIgnore, // ':' subparameters: ignored wholesale
            0x20..=0x2f => self.state = State::CsiInt,
            0x40..=0x7e => {
                self.finalize_and_dispatch(b);
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_param(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x30..=0x39 => {
                self.csi_cur = (self.csi_cur * 10 + (b - 0x30) as u32).min(65535);
            }
            b';' => self.push_param(),
            0x3a | 0x3c..=0x3f => self.state = State::CsiIgnore,
            0x20..=0x2f => self.state = State::CsiInt,
            0x40..=0x7e => {
                self.finalize_and_dispatch(b);
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_int(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x20..=0x2f => {}
            _ => self.state = State::Ground, // CSI with intermediate bytes: ignored
        }
    }

    fn csi_ignore(&mut self, b: u8) {
        match b {
            0x00..=0x1f => self.execute(b),
            0x40..=0x7e => self.state = State::Ground,
            _ => {}
        }
    }

    fn finalize_and_dispatch(&mut self, final_byte: u8) {
        self.push_param(); // finalize: push the last parameter (possibly the default 0)
        self.csi_dispatch(final_byte);
        self.state = State::Ground;
    }

    /// Get the i-th parameter, interpreted as "1 is the default (both 0 and missing count as 1)" — used for cursor-type operations.
    fn p1(&self, i: usize) -> usize {
        match self.params.get(i) {
            Some(&v) if v != 0 => v as usize,
            _ => 1,
        }
    }

    /// Get the raw value of the i-th parameter (default 0) — used for SGR / erase modes, etc.
    fn praw(&self, i: usize) -> u16 {
        self.params.get(i).copied().unwrap_or(0)
    }

    fn csi_dispatch(&mut self, f: u8) {
        match f {
            b'A' => self.move_up(self.p1(0)),
            b'B' | b'e' => self.move_down(self.p1(0)),
            b'C' | b'a' => self.move_right(self.p1(0)),
            b'D' => self.move_left(self.p1(0)),
            b'E' => {
                self.cursor.0 = 0;
                self.move_down(self.p1(0));
            }
            b'F' => {
                self.cursor.0 = 0;
                self.move_up(self.p1(0));
            }
            b'G' | b'`' => self.set_col(self.p1(0) - 1),
            b'd' => self.set_row(self.p1(0) - 1),
            b'H' | b'f' => {
                let r = self.p1(0) - 1;
                let c = self.p1(1) - 1;
                self.set_pos(c, r);
            }
            b'J' => self.erase_display(self.praw(0)),
            b'K' => self.erase_line(self.praw(0)),
            b'm' => self.sgr(),
            b'r' => self.set_scroll_region(),
            b'L' => self.insert_lines(self.p1(0)),
            b'M' => self.delete_lines(self.p1(0)),
            b'@' => self.insert_chars(self.p1(0)),
            b'P' => self.delete_chars(self.p1(0)),
            b'X' => self.erase_chars(self.p1(0)),
            b'S' => self.scroll_up_saving(self.scroll_top, self.scroll_bot, self.p1(0)),
            b'T' => self.scroll_down(self.scroll_top, self.scroll_bot, self.p1(0)),
            b'h' => self.set_mode(true),
            b'l' => self.set_mode(false),
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            b'I' => self.tab_move(self.p1(0), true),  // CHT: forward n tab stops
            b'Z' => self.tab_move(self.p1(0), false), // CBT: back n tab stops
            b'g' => self.clear_tab_stop(self.praw(0)), // TBC
            b'n' => self.report_status(),
            b'c' => self.report_device_attrs(),
            _ => {}
        }
    }

    /// DSR (`CSI Ps n`): Ps=5 "are you OK?" → `CSI 0 n`; Ps=6 "report cursor position" →
    /// `CSI row;col R`. Some programs (vim, tmux, shell prompt themes) block waiting for one of
    /// these and would otherwise hang.
    fn report_status(&mut self) {
        match self.praw(0) {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let (row, col) = (self.cursor.1 + 1, self.cursor.0 + 1);
                self.replies.extend_from_slice(format!("\x1b[{};{}R", row, col).as_bytes());
            }
            _ => {}
        }
    }

    /// DA (`CSI c` / `CSI > c`): device attributes. Programs probe this to detect terminal
    /// capabilities; any minimally-valid reply unblocks them. `?1;2c` = VT100 with AVO, the same
    /// baseline many minimal terminal emulators report.
    fn report_device_attrs(&mut self) {
        self.replies.extend_from_slice(b"\x1b[?1;2c");
    }

    // ===================== OSC / DCS strings =====================

    fn osc(&mut self, b: u8) {
        match b {
            0x07 => {
                self.osc_dispatch();
                self.state = State::Ground;
            }
            0x1b => {
                // expecting ST (ESC \): finalize the OSC first, then let Escape consume that '\'
                self.osc_dispatch();
                self.state = State::Escape;
            }
            0x18 | 0x1a => self.state = State::Ground,
            _ => {
                if self.osc.len() < 1024 {
                    self.osc.push(b);
                }
            }
        }
    }

    fn dcs(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Escape,
            0x07 => self.state = State::Ground,
            _ => {}
        }
    }

    fn osc_dispatch(&mut self) {
        // Of the form "n;text": n ∈ {0,1,2} sets the title; n=7 reports the cwd (file://host/path).
        let s = String::from_utf8_lossy(&self.osc);
        if let Some((num, text)) = s.split_once(';') {
            if matches!(num, "0" | "1" | "2") {
                self.title = text.to_string();
            } else if num == "7" {
                if let Some(rest) = text.strip_prefix("file://") {
                    if let Some(slash) = rest.find('/') {
                        self.cwd = percent_decode(&rest[slash..]);
                    }
                }
            }
        }
        self.osc.clear();
    }

    // ===================== Cursor movement =====================

    fn move_up(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.1 = self.cursor.1.saturating_sub(n);
    }
    fn move_down(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.1 = (self.cursor.1 + n).min(self.rows - 1);
    }
    fn move_left(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.0 = self.cursor.0.saturating_sub(n);
    }
    fn move_right(&mut self, n: usize) {
        self.pending_wrap = false;
        self.cursor.0 = (self.cursor.0 + n).min(self.cols - 1);
    }
    fn set_col(&mut self, c: usize) {
        self.pending_wrap = false;
        self.cursor.0 = c.min(self.cols - 1);
    }
    fn set_row(&mut self, r: usize) {
        self.pending_wrap = false;
        self.cursor.1 = r.min(self.rows - 1);
    }
    fn set_pos(&mut self, c: usize, r: usize) {
        self.pending_wrap = false;
        self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
    }

    fn save_cursor(&mut self) {
        self.saved = Some((self.cursor.0, self.cursor.1, self.pen_fg, self.pen_bg, self.pen_flags));
    }
    fn restore_cursor(&mut self) {
        if let Some((c, r, fg, bg, fl)) = self.saved {
            self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
            self.pen_fg = fg;
            self.pen_bg = bg;
            self.pen_flags = fl;
            self.pending_wrap = false;
        }
    }

    // ===================== Line feed / scrolling =====================

    /// LF / IND: scroll up if at the bottom of the scroll region, otherwise move down one row (column unchanged).
    fn linefeed(&mut self) {
        if self.cursor.1 == self.scroll_bot {
            self.scroll_up_saving(self.scroll_top, self.scroll_bot, 1);
        } else if self.cursor.1 + 1 < self.rows {
            self.cursor.1 += 1;
        }
        // A vertical move cancels any deferred autowrap from the previous row; otherwise the
        // next printed character would force a spurious extra line feed + column reset.
        self.pending_wrap = false;
    }

    /// RI: scroll down if at the top of the scroll region, otherwise move up one row.
    fn reverse_index(&mut self) {
        if self.cursor.1 == self.scroll_top {
            self.scroll_down(self.scroll_top, self.scroll_bot, 1);
        } else if self.cursor.1 > 0 {
            self.cursor.1 -= 1;
        }
        self.pending_wrap = false;
    }

    /// Scroll up, first pushing the rows about to be discarded into the scrollback.
    ///
    /// Only content that genuinely falls off the top of the screen belongs in history, so this is
    /// called from `linefeed`/SU — never from the shared `scroll_up`. `delete_lines` (DL) also
    /// reaches `scroll_up(0, …)` when the cursor sits on row 0, and those lines were deleted by the
    /// application, not scrolled away: saving them would corrupt the scrollback with text the user
    /// never saw scroll past. The alt screen and any partial scroll region are excluded for the
    /// same reason.
    fn scroll_up_saving(&mut self, top: usize, bot: usize, n: usize) {
        if top >= bot || n == 0 {
            return;
        }
        if !self.alt && top == 0 {
            let n = n.min(bot - top + 1);
            for r in 0..n {
                self.push_history(r);
            }
        }
        self.scroll_up(top, bot, n);
    }

    /// Copy screen row `r` into the scrollback, trimming trailing blank cells.
    ///
    /// Keeps the viewport pinned to the same content while scrolled back: appending a line shifts
    /// every visible line up by one, so `view_offset` grows to compensate. Once history is at cap
    /// the oldest line is evicted, and the clamp against the (unchanged) length keeps the top of
    /// the scrollback from scrolling out from under the viewport.
    fn push_history(&mut self, r: usize) {
        let base = r * self.cols;
        let row = &self.cells[base..base + self.cols];
        let end = row.iter().rposition(|c| *c != BLANK).map_or(0, |i| i + 1);
        if self.history.len() == HISTORY_MAX {
            self.history.pop_front();
        }
        self.history.push_back(row[..end].to_vec());
        self.scrolled += 1;
        if self.view_offset > 0 {
            self.view_offset = (self.view_offset + 1).min(self.history.len());
        }
    }

    /// Scroll the row range [top, bot] up by n rows, filling the bottom with blanks.
    fn scroll_up(&mut self, top: usize, bot: usize, n: usize) {
        if top >= bot || n == 0 {
            return;
        }
        let cols = self.cols;
        let h = bot - top + 1;
        let n = n.min(h);
        let end = (bot + 1) * cols;
        if n < h {
            self.cells.copy_within((top + n) * cols..end, top * cols);
        }
        for r in (bot + 1 - n)..=bot {
            self.blank_row(r);
        }
    }

    /// Scroll the row range [top, bot] down by n rows, filling the top with blanks.
    fn scroll_down(&mut self, top: usize, bot: usize, n: usize) {
        if top >= bot || n == 0 {
            return;
        }
        let cols = self.cols;
        let h = bot - top + 1;
        let n = n.min(h);
        let start = top * cols;
        if n < h {
            self.cells.copy_within(start..(bot + 1 - n) * cols, (top + n) * cols);
        }
        for r in top..(top + n) {
            self.blank_row(r);
        }
    }

    fn insert_lines(&mut self, n: usize) {
        let r = self.cursor.1;
        if r < self.scroll_top || r > self.scroll_bot {
            return;
        }
        self.scroll_down(r, self.scroll_bot, n);
        self.cursor.0 = 0;
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let r = self.cursor.1;
        if r < self.scroll_top || r > self.scroll_bot {
            return;
        }
        self.scroll_up(r, self.scroll_bot, n);
        self.cursor.0 = 0;
        self.pending_wrap = false;
    }

    // ===================== In-line insert / delete / erase =====================

    fn insert_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.cells.copy_within(base + c..base + cols - n, base + c + n);
        self.blank_range(base + c, base + c + n);
        self.pending_wrap = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.cells.copy_within(base + c + n..base + cols, base + c);
        self.blank_range(base + cols - n, base + cols);
        self.pending_wrap = false;
    }

    fn erase_chars(&mut self, n: usize) {
        let (c, r) = self.cursor;
        let cols = self.cols;
        let n = n.min(cols - c);
        let base = r * cols;
        self.blank_range(base + c, base + c + n);
        self.pending_wrap = false;
    }

    fn erase_display(&mut self, mode: u16) {
        let (c, r) = self.cursor;
        let idx = r * self.cols + c;
        let len = self.cells.len();
        match mode {
            0 => self.blank_range(idx, len),   // cursor to end of screen
            1 => self.blank_range(0, idx + 1), // start of screen to cursor
            2 => self.blank_range(0, len),     // whole screen
            3 => {
                // ED 3 (xterm): whole screen *and* the scrollback.
                self.blank_range(0, len);
                self.history.clear();
                self.view_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (c, r) = self.cursor;
        let base = r * self.cols;
        match mode {
            0 => self.blank_range(base + c, base + self.cols), // cursor to end of line
            1 => self.blank_range(base, base + c + 1),         // start of line to cursor
            2 => self.blank_range(base, base + self.cols),     // whole line
            _ => {}
        }
    }

    fn blank_row(&mut self, r: usize) {
        let base = r * self.cols;
        self.blank_range(base, base + self.cols);
    }

    fn blank_range(&mut self, start: usize, end: usize) {
        for cell in &mut self.cells[start..end] {
            *cell = Cell::default();
        }
    }

    // ===================== SGR / modes / scroll region =====================

    fn sgr(&mut self) {
        // No parameters is equivalent to [0] (reset).
        let params = if self.params.is_empty() { vec![0u16] } else { self.params.clone() };
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => {
                    self.pen_fg = Color::Default;
                    self.pen_bg = Color::Default;
                    self.pen_flags = 0;
                }
                1 => self.pen_flags |= BOLD,
                3 => self.pen_flags |= ITALIC,
                4 => self.pen_flags |= UNDERLINE,
                7 => self.pen_flags |= INVERSE,
                21 | 22 => self.pen_flags &= !BOLD,
                23 => self.pen_flags &= !ITALIC,
                24 => self.pen_flags &= !UNDERLINE,
                27 => self.pen_flags &= !INVERSE,
                30..=37 => self.pen_fg = Color::Indexed((p - 30) as u8),
                38 => self.pen_fg = Self::ext_color(&params, &mut i).unwrap_or(self.pen_fg),
                39 => self.pen_fg = Color::Default,
                40..=47 => self.pen_bg = Color::Indexed((p - 40) as u8),
                48 => self.pen_bg = Self::ext_color(&params, &mut i).unwrap_or(self.pen_bg),
                49 => self.pen_bg = Color::Default,
                90..=97 => self.pen_fg = Color::Indexed((p - 90 + 8) as u8),
                100..=107 => self.pen_bg = Color::Indexed((p - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    /// Parse the extended color for 38/48: `5;n` (indexed) or `2;r;g;b` (true color).
    /// After the call, `i` points at the last subparameter consumed.
    fn ext_color(params: &[u16], i: &mut usize) -> Option<Color> {
        match params.get(*i + 1).copied() {
            Some(5) => {
                let n = params.get(*i + 2).copied().unwrap_or(0) as u8;
                *i += 2;
                Some(Color::Indexed(n))
            }
            Some(2) => {
                let r = params.get(*i + 2).copied().unwrap_or(0) as u8;
                let g = params.get(*i + 3).copied().unwrap_or(0) as u8;
                let b = params.get(*i + 4).copied().unwrap_or(0) as u8;
                *i += 4;
                Some(Color::Rgb(r, g, b))
            }
            _ => None,
        }
    }

    fn set_mode(&mut self, set: bool) {
        if self.private == b'?' {
            let params = self.params.clone();
            for p in params {
                match p {
                    1 => self.app_cursor_keys = set, // DECCKM application cursor keys
                    7 => self.autowrap = set,        // DECAWM autowrap
                    25 => self.cursor_visible = set, // DECTCEM cursor visibility
                    1048 => {
                        // save/restore cursor only
                        if set {
                            self.save_cursor();
                        } else {
                            self.restore_cursor();
                        }
                    }
                    47 | 1047 => {
                        // switch alt screen (leave cursor untouched)
                        if set {
                            self.enter_alt();
                        } else {
                            self.leave_alt();
                        }
                    }
                    1049 => {
                        // switch alt screen + save/restore cursor + clear and home on entry
                        if set {
                            self.alt_saved = Some((
                                self.cursor.0,
                                self.cursor.1,
                                self.pen_fg,
                                self.pen_bg,
                                self.pen_flags,
                            ));
                            self.enter_alt();
                            self.set_pos(0, 0);
                        } else {
                            self.leave_alt();
                            if let Some((c, r, fg, bg, fl)) = self.alt_saved.take() {
                                self.cursor = (c.min(self.cols - 1), r.min(self.rows - 1));
                                self.pen_fg = fg;
                                self.pen_bg = bg;
                                self.pen_flags = fl;
                                self.pending_wrap = false;
                            }
                        }
                    }
                    1000 => self.mouse_press = set, // mouse tracking: press/release
                    1002 => self.mouse_drag = set,  // ... plus drag motion
                    1003 => self.mouse_any = set,   // ... plus button-less motion
                    1006 => self.mouse_sgr = set,   // SGR mouse encoding
                    // 1005 (UTF-8) and 1015 (urxvt) are rival extended encodings, both superseded
                    // by 1006 and both ambiguous to decode. Left off deliberately: an application
                    // that is refused them falls back to the legacy encoding, which does work.
                    1005 | 1015 => {}
                    1007 => self.alt_scroll = set, // alternate scroll
                    2004 => self.bracketed_paste = set,
                    _ => {} // other private modes: acknowledged and ignored
                }
            }
        } else {
            let params = self.params.clone();
            for p in params {
                match p {
                    4 => self.insert_mode = set, // IRM: insert rather than overwrite
                    _ => {}                      // other ANSI modes: acknowledged and ignored
                }
            }
        }
    }

    /// Enter the alt screen: swap with the undisplayed buffer, clear the newly displayed one, and reset the scroll region.
    fn enter_alt(&mut self) {
        if self.alt {
            return;
        }
        self.alt = true;
        self.view_offset = 0; // the alt screen has no scrollback; never show it through one
        std::mem::swap(&mut self.cells, &mut self.inactive);
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.pending_wrap = false;
    }

    /// Leave the alt screen: switch back to the main screen (whose contents were kept in the undisplayed buffer all along).
    fn leave_alt(&mut self) {
        if !self.alt {
            return;
        }
        self.alt = false;
        self.view_offset = 0; // land on the live bottom of the restored main screen
        std::mem::swap(&mut self.cells, &mut self.inactive);
        self.pending_wrap = false;
    }

    /// Resize the screen: reallocate the main/alt buffers, top-anchored (content keeps its
    /// position from the top; growing just adds blank rows at the bottom). Only when shrinking
    /// below the cursor are the oldest top rows dropped, so the cursor / most recent output stays
    /// visible, with the cursor position following along.
    /// No automatic reflow; after receiving SIGWINCH the shell redraws the current line itself.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let (oc, or) = (self.cols, self.rows);
        // Top-anchored: content keeps its position from the top (so a taller screen just gains blank
        // rows at the bottom). Only when shrinking below the cursor do we drop the oldest (top) rows,
        // so the cursor / most-recent output stays visible.
        let drop_top = self.cursor.1.saturating_sub(rows - 1);
        self.cells = Self::resize_buf(&self.cells, oc, or, cols, rows, drop_top);
        self.inactive = Self::resize_buf(&self.inactive, oc, or, cols, rows, drop_top);
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.cursor.1 = (self.cursor.1 - drop_top).min(rows - 1);
        self.cursor.0 = self.cursor.0.min(cols - 1);
        self.pending_wrap = false;
        // Tab stops are per column, so a wider screen needs stops for the columns it gained. The
        // existing ones are kept (an application that placed them did so deliberately) and the new
        // tail gets the default every-eighth-column layout.
        let old_len = self.tab_stops.len();
        self.tab_stops.resize(cols, false);
        for c in old_len..cols {
            self.tab_stops[c] = c % 8 == 0;
        }
        // History rows keep their old width (no reflow); `view_cell` pads short rows with blanks
        // and ignores the overhang, so only the offset needs re-clamping.
        self.view_offset = self.view_offset.min(self.history.len());
    }

    /// Move a buffer into the new dimensions: columns left-aligned, rows top-anchored after dropping
    /// `drop_top` oldest rows (nonzero only when shrinking below the cursor).
    fn resize_buf(old: &[Cell], oc: usize, or: usize, nc: usize, nr: usize, drop_top: usize) -> Vec<Cell> {
        let mut v = vec![Cell::default(); nc * nr];
        let copy_c = oc.min(nc);
        let copy_r = or.saturating_sub(drop_top).min(nr);
        for i in 0..copy_r {
            let old_row = drop_top + i;
            let new_row = i; // top-anchored
            for c in 0..copy_c {
                v[new_row * nc + c] = old[old_row * oc + c];
            }
        }
        v
    }

    fn set_scroll_region(&mut self) {
        let top = self.p1(0) - 1;
        let bot = match self.praw(1) {
            0 => self.rows - 1,
            v => (v as usize).saturating_sub(1).min(self.rows - 1),
        };
        if top < bot {
            self.scroll_top = top;
            self.scroll_bot = bot;
        }
        // DECSTBM resets the cursor to the top-left corner of the screen.
        self.set_pos(0, 0);
    }

    fn hard_reset(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        for cell in &mut self.inactive {
            *cell = Cell::default();
        }
        self.alt = false;
        self.alt_saved = None;
        self.cursor = (0, 0);
        self.pending_wrap = false;
        self.pen_fg = Color::Default;
        self.pen_bg = Color::Default;
        self.pen_flags = 0;
        self.saved = None;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.autowrap = true;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.alt_scroll = true;
        self.mouse_press = false;
        self.mouse_drag = false;
        self.mouse_any = false;
        self.mouse_sgr = false;
        self.insert_mode = false;
        self.tab_stops = Self::default_tab_stops(self.cols);
        self.g0_graphics = false;
        self.g1_graphics = false;
        self.shift_out = false;
        self.esc_intermediate = 0;
        self.state = State::Ground;
        self.params.clear();
        self.csi_cur = 0;
        self.private = 0;
        self.osc.clear();
        self.utf8_need = 0;
        self.utf8_len = 0;
    }

    /// For dumb rendering / tests: export as per-row text (with trailing whitespace stripped).
    pub fn to_lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|r| {
                (0..self.cols)
                    .map(|c| self.cell(c, r).ch)
                    .filter(|&ch| ch != '\0') // drop wide-char trailer placeholders
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// Percent-decode an OSC 7 path (e.g. %20 → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Decode the two hex digits directly from bytes (not via str slicing): a '%' can be
        // immediately followed by a multibyte UTF-8 character whose bytes don't fall on a char
        // boundary at i+1/i+3, which would panic if sliced as a &str.
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a single ASCII hex digit byte (0-9/a-f/A-F) to its numeric value.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_and_wrap() {
        let mut g = Grid::new(5, 2);
        g.feed(b"hello worl");
        assert_eq!(g.to_lines(), vec!["hello", " worl"]);
    }

    #[test]
    fn linefeed_after_wrap_pending_does_not_skip_a_row() {
        // Regression test: a bare LF right after filling the last column used to leave
        // pending_wrap set, so the NEXT printed character forced an extra spurious linefeed +
        // column reset, silently skipping a row instead of the plain "move down one row, same
        // column" that a bare LF should do.
        let mut g = Grid::new(5, 3);
        g.feed(b"hello"); // fills the last column, sets pending_wrap
        g.feed(b"\n"); // bare LF: should just move down one row, same column
        g.feed(b"X");
        assert_eq!(g.to_lines(), vec!["hello", "    X", ""]);
    }

    #[test]
    fn reverse_index_after_wrap_pending_does_not_skip_a_row() {
        // Same bug as above, via RI (ESC M) instead of a bare LF: move the cursor down first so
        // RI moves up (not scrolling), fill the last column, then RI + print.
        let mut g = Grid::new(5, 3);
        g.feed(b"\n"); // cursor to row 1
        g.feed(b"hello"); // fills row 1's last column, sets pending_wrap
        g.feed(b"\x1bM"); // RI: should just move up one row, same column
        g.feed(b"X");
        assert_eq!(g.to_lines(), vec!["    X", "hello", ""]);
    }

    #[test]
    fn crlf_moves_cursor() {
        let mut g = Grid::new(10, 3);
        g.feed(b"ab\r\ncd");
        assert_eq!(g.to_lines(), vec!["ab", "cd", ""]);
        assert_eq!(g.cursor, (2, 1));
    }

    #[test]
    fn scroll_at_bottom() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3");
        assert_eq!(g.to_lines(), vec!["2", "3"]);
    }

    #[test]
    fn scroll_three_lines() {
        let mut g = Grid::new(3, 3);
        g.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(g.to_lines(), vec!["2", "3", "4"]);
    }

    // ===================== Scrollback =====================

    /// What the renderer would draw: the viewport read through `view_cell`, row by row.
    fn view_lines(g: &Grid) -> Vec<String> {
        (0..g.rows)
            .map(|r| {
                (0..g.cols)
                    .map(|c| g.view_cell(c, r).ch)
                    .filter(|&ch| ch != '\0')
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn lines_scrolled_off_the_top_land_in_history() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(g.to_lines(), vec!["3", "4"]);
        assert_eq!(g.history_len(), 2); // "1" and "2"
    }

    #[test]
    fn scrolling_back_shows_history_above_the_screen() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(view_lines(&g), vec!["3", "4"]); // following the output

        assert!(g.scroll_view(1));
        assert_eq!(view_lines(&g), vec!["2", "3"]); // one history line, then the top screen row

        assert!(g.scroll_view(1));
        assert_eq!(view_lines(&g), vec!["1", "2"]); // both viewport rows come from history

        assert!(!g.scroll_view(1)); // clamped at the oldest line: no change, no redraw
        assert_eq!(view_lines(&g), vec!["1", "2"]);

        assert!(g.scroll_to_bottom());
        assert_eq!(view_lines(&g), vec!["3", "4"]);
    }

    #[test]
    fn delete_lines_does_not_pollute_history() {
        // Regression test: DL with the cursor on row 0 reaches scroll_up(0, bot, n), which is
        // byte-for-byte the call linefeed makes when scrolling at the bottom. Those lines were
        // deleted by the application, not scrolled off the top — they must never enter history.
        let mut g = Grid::new(3, 3);
        g.feed(b"1\r\n2\r\n3"); // fills the screen without ever scrolling
        assert_eq!(g.history_len(), 0);
        g.feed(b"\x1b[H"); // cursor home (row 0)
        g.feed(b"\x1b[2M"); // DL 2: delete rows 0-1
        assert_eq!(g.to_lines(), vec!["3", "", ""]);
        assert_eq!(g.history_len(), 0, "DL'd lines must not enter the scrollback");
    }

    #[test]
    fn scroll_region_scroll_does_not_enter_history() {
        // A partial scroll region (top != 0) never spills off the top of the screen.
        let mut g = Grid::new(3, 3);
        g.feed(b"\x1b[2;3r"); // confine scrolling to rows 2-3
        g.feed(b"\x1b[2;1H"); // park the cursor inside the region
        g.feed(b"a\r\nb\r\nc");
        assert_eq!(g.history_len(), 0);
    }

    #[test]
    fn alt_screen_has_no_scrollback() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3"); // one line ("1") into history
        assert_eq!(g.history_len(), 1);

        g.feed(b"\x1b[?1049h"); // enter alt screen
        g.feed(b"a\r\nb\r\nc\r\nd"); // scrolls a lot, contributes nothing
        assert_eq!(g.history_len(), 1);
        assert!(!g.scroll_view(1), "the alt screen must not scroll back");
        assert_eq!(g.view_offset(), 0);

        g.feed(b"\x1b[?1049l"); // leave: the main screen and its history come back untouched
        assert_eq!(g.history_len(), 1);
        assert_eq!(g.to_lines(), vec!["2", "3"]);
    }

    #[test]
    fn new_output_keeps_the_scrolled_back_viewport_pinned() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4");
        g.scroll_view(2);
        assert_eq!(view_lines(&g), vec!["1", "2"]);

        g.feed(b"\r\n5"); // more output arrives while the user is reading back
        assert_eq!(view_lines(&g), vec!["1", "2"], "the viewport must not jump");
        assert_eq!(g.view_offset(), 3);

        g.scroll_to_bottom();
        assert_eq!(view_lines(&g), vec!["4", "5"]);
    }

    #[test]
    fn history_is_capped_and_the_viewport_survives_eviction() {
        let mut g = Grid::new(6, 2);
        for i in 0..HISTORY_MAX + 10 {
            g.feed(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(g.history_len(), HISTORY_MAX);

        // Scrolled fully back, the oldest surviving line is on top; further output evicts from the
        // front, and the clamp keeps the offset legal rather than reading past the end.
        g.scroll_view(HISTORY_MAX as isize);
        assert_eq!(g.view_offset(), HISTORY_MAX);
        g.feed(b"tail\r\n");
        assert_eq!(g.view_offset(), HISTORY_MAX);
        assert_eq!(g.history_len(), HISTORY_MAX);
        let _ = view_lines(&g); // must not panic on the evicted front
    }

    #[test]
    fn alt_scroll_translates_the_wheel_into_cursor_keys() {
        let mut g = Grid::new(4, 3);
        g.feed(b"1\r\n2\r\n3\r\n4"); // some scrollback on the main screen

        // Main screen: the viewport scrolls, so the wheel must not be translated.
        assert!(!g.alt_screen());
        assert_eq!(g.alt_scroll_keys(1), None);

        // Alt screen: no scrollback of its own, so the wheel becomes cursor keys.
        g.feed(b"\x1b[?1049h");
        assert!(g.alt_screen());
        assert_eq!(g.alt_scroll_keys(2), Some(b"\x1b[A\x1b[A".to_vec()));
        assert_eq!(g.alt_scroll_keys(-1), Some(b"\x1b[B".to_vec()));
        assert_eq!(g.alt_scroll_keys(0), None);
        // Never more than a screenful of keystrokes per event.
        assert_eq!(g.alt_scroll_keys(50).unwrap().len(), 3 * b"\x1b[A".len());

        // DECCKM picks the application-mode prefix, exactly like the arrow keys themselves.
        g.feed(b"\x1b[?1h");
        assert_eq!(g.alt_scroll_keys(1), Some(b"\x1bOA".to_vec()));
        g.feed(b"\x1b[?1l");

        // An application that turns 1007 off gets nothing; one that turns it back on gets keys again.
        g.feed(b"\x1b[?1007l");
        assert_eq!(g.alt_scroll_keys(1), None);
        g.feed(b"\x1b[?1007h");
        assert_eq!(g.alt_scroll_keys(1), Some(b"\x1b[A".to_vec()));

        // Mouse tracking suppresses the translation: the wheel goes out as a real mouse report
        // instead, and cursor keys on top of that would be keystrokes the application never asked for.
        g.feed(b"\x1b[?1000h");
        assert!(g.mouse_report());
        assert_eq!(g.alt_scroll_keys(1), None);
        g.feed(b"\x1b[?1000l");
        assert!(!g.mouse_report());
        assert_eq!(g.alt_scroll_keys(1), Some(b"\x1b[A".to_vec()));

        // Back on the main screen the viewport takes over again.
        g.feed(b"\x1b[?1049l");
        assert_eq!(g.alt_scroll_keys(1), None);
    }

    // ---- IRM ----

    #[test]
    fn irm_inserts_instead_of_overwriting() {
        let mut g = Grid::new(10, 1);
        g.feed(b"abcdef");
        // Replace mode (the default): typing over the line overwrites it.
        g.feed(b"\x1b[1GXY");
        assert_eq!(g.to_lines()[0], "XYcdef");

        // Insert mode: the rest of the line is pushed right instead.
        g.feed(b"\x1b[4h\x1b[1GZ");
        assert_eq!(g.to_lines()[0], "ZXYcdef");

        // ... and turning it back off resumes overwriting.
        g.feed(b"\x1b[4l\x1b[1GQ");
        assert_eq!(g.to_lines()[0], "QXYcdef");
    }

    #[test]
    fn irm_pushes_content_off_the_right_margin() {
        let mut g = Grid::new(6, 1);
        g.feed(b"abcdef\x1b[4h\x1b[1GZ");
        // The line cannot grow, so the last column falls off rather than wrapping.
        assert_eq!(g.to_lines()[0], "Zabcde");
    }

    #[test]
    fn irm_makes_room_for_both_columns_of_a_wide_glyph() {
        let mut g = Grid::new(6, 1);
        g.feed("abcdef\x1b[4h\x1b[1G\u{4e2d}".as_bytes());
        // A wide glyph occupies two cells, so two are opened and two fall off the end.
        assert_eq!(g.to_lines()[0], "中abcd");
    }

    #[test]
    fn irm_is_cleared_by_a_hard_reset() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b[4h\x1bc");
        g.feed(b"abc\x1b[1GX");
        assert_eq!(g.to_lines()[0], "Xbc");
    }

    // ---- tab stops ----

    #[test]
    fn tabs_default_to_every_eighth_column() {
        let mut g = Grid::new(40, 1);
        g.feed(b"a\tb\tc");
        assert_eq!(g.to_lines()[0], "a       b       c");
    }

    #[test]
    fn hts_and_tbc_edit_the_stops() {
        let mut g = Grid::new(40, 1);
        // Clear every stop, then place one at column 5 and one at column 12.
        g.feed(b"\x1b[3g");
        g.feed(b"\x1b[6G\x1bH\x1b[13G\x1bH\x1b[1G");
        g.feed(b"a\tb\tc");
        assert_eq!(g.to_lines()[0], "a    b      c");

        // TBC with no parameter clears just the stop under the cursor, so the tab overshoots it.
        g.feed(b"\x1b[H\x1b[K\x1b[6G\x1b[g\x1b[1G");
        g.feed(b"a\tb");
        assert_eq!(g.to_lines()[0], "a           b");
    }

    #[test]
    fn tab_with_no_stops_left_parks_at_the_right_margin() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b[3g"); // clear every stop
        g.feed(b"a\tb");
        assert_eq!(g.to_lines()[0], "a        b");
    }

    #[test]
    fn cht_and_cbt_walk_the_stops() {
        let mut g = Grid::new(40, 1);
        // CHT: forward three stops from column 0 → column 24 (stops at 8/16/24).
        g.feed(b"\x1b[3IX");
        assert_eq!(g.to_lines()[0], "                        X");

        // CBT counts from where the cursor is now, which printing X advanced to 25 — so two stops
        // back is 24 then 16, not 16 then 8.
        g.feed(b"\x1b[2ZY");
        assert_eq!(g.to_lines()[0].find('Y'), Some(16));

        // CBT past the first stop stops at column 0 rather than wrapping.
        g.feed(b"\x1b[9ZZ");
        assert_eq!(g.to_lines()[0].find('Z'), Some(0));
    }

    #[test]
    fn tab_stops_survive_a_resize_and_reset_with_ris() {
        let mut g = Grid::new(20, 1);
        g.feed(b"\x1b[3g\x1b[6G\x1bH"); // only stop: column 5
        g.resize(40, 1);
        // The stop the application placed survives the widening.
        g.feed(b"\x1b[1G\x1b[Ka\tb");
        assert_eq!(g.to_lines()[0], "a    b");
        // The columns gained by widening (20..39) get the default layout, so the next stop past 5
        // is 24 — the old columns keep the cleared state they were left in, they are not refilled.
        g.feed(b"\x1b[1G\x1b[Ka\t\tb");
        assert_eq!(g.to_lines()[0].find('b'), Some(24));

        g.feed(b"\x1bc"); // RIS restores the power-on stops everywhere
        g.feed(b"a\tb");
        assert_eq!(g.to_lines()[0], "a       b");
    }

    // ---- DEC Special Graphics ----

    #[test]
    fn dec_graphics_draws_a_box_through_g0() {
        let mut g = Grid::new(5, 3);
        // The canonical ncurses rectangle: lqk / x x / mqj, with the set designated into G0.
        g.feed(b"\x1b(0lqk\r\nx x\r\nmqj");
        assert_eq!(g.to_lines()[0], "┌─┐");
        assert_eq!(g.to_lines()[1], "│ │");
        assert_eq!(g.to_lines()[2], "└─┘");

        // Back to ASCII: the same letters are letters again.
        g.feed(b"\x1b(B\r\x1b[Hqqq");
        assert_eq!(g.to_lines()[0], "qqq");
    }

    #[test]
    fn dec_graphics_in_g1_is_toggled_by_shift_out_and_in() {
        let mut g = Grid::new(9, 1);
        // The other common idiom: park the set in G1 and flip it in around each run.
        g.feed(b"\x1b)0a\x0eqqq\x0fb");
        assert_eq!(g.to_lines()[0], "a───b");
        // G0 is untouched by the G1 designation, so SI leaves plain ASCII behind.
        assert_eq!(g.to_lines()[0].chars().next(), Some('a'));
    }

    #[test]
    fn dec_graphics_only_remaps_its_own_block() {
        let mut g = Grid::new(20, 1);
        // Digits and uppercase sit below 0x5f and pass through untouched, as does UTF-8 above it.
        g.feed("\x1b(0A1z\u{4e2d}".as_bytes());
        assert_eq!(g.to_lines()[0], "A1≥中");
    }

    #[test]
    fn dec_graphics_is_cleared_by_a_hard_reset() {
        let mut g = Grid::new(5, 1);
        g.feed(b"\x1b(0\x1b)0\x0e");
        g.feed(b"\x1bc"); // RIS
        g.feed(b"qqq");
        assert_eq!(g.to_lines()[0], "qqq");
    }

    // ---- mouse reporting ----

    /// A press of the left button at (col, row) with no modifiers, the common case in these tests.
    fn press(g: &Grid, col: usize, row: usize) -> Option<Vec<u8>> {
        g.mouse_report_bytes(
            MouseEvent::Press(MouseButton::Left),
            MouseMods::default(),
            col,
            row,
        )
    }

    #[test]
    fn mouse_modes_are_independent_and_ordered() {
        let mut g = Grid::new(80, 24);
        assert_eq!(g.mouse_mode(), MouseMode::Off);
        assert!(!g.mouse_report());

        g.feed(b"\x1b[?1002h");
        assert_eq!(g.mouse_mode(), MouseMode::Drag);
        // Turning a *different* mode off must not cancel the one that is on — applications switch
        // between the modes by setting one and clearing the others, in either order.
        g.feed(b"\x1b[?1000l");
        assert_eq!(g.mouse_mode(), MouseMode::Drag);
        // The effective level is the most permissive one enabled, whatever order they arrived in.
        g.feed(b"\x1b[?1003h");
        assert_eq!(g.mouse_mode(), MouseMode::Any);
        g.feed(b"\x1b[?1003l");
        assert_eq!(g.mouse_mode(), MouseMode::Drag);
        g.feed(b"\x1b[?1002l");
        assert_eq!(g.mouse_mode(), MouseMode::Off);
    }

    #[test]
    fn mouse_off_reports_nothing() {
        let g = Grid::new(80, 24);
        assert_eq!(press(&g, 0, 0), None);
    }

    #[test]
    fn mouse_legacy_encoding_biases_by_32_and_is_one_based() {
        let mut g = Grid::new(80, 24);
        g.feed(b"\x1b[?1000h");
        // Cell (0,0) → coordinates 1,1 → bytes 33,33. Left button press → code 0 → byte 32.
        assert_eq!(press(&g, 0, 0), Some(b"\x1b[M\x20\x21\x21".to_vec()));
        // Cell (9,4) → 10,5 → 42,37.
        assert_eq!(press(&g, 9, 4), Some(vec![0x1b, b'[', b'M', 32, 42, 37]));
    }

    #[test]
    fn mouse_legacy_release_loses_the_button_and_wheel_has_none() {
        let mut g = Grid::new(80, 24);
        g.feed(b"\x1b[?1000h");
        // The legacy encoding has no room to name the button being released: it is always 3.
        let right_up = g.mouse_report_bytes(
            MouseEvent::Release(MouseButton::Right),
            MouseMods::default(),
            0,
            0,
        );
        assert_eq!(right_up, Some(vec![0x1b, b'[', b'M', 3 + 32, 33, 33]));

        // A wheel notch is a press with no matching release; the release must produce nothing.
        let wheel_down = g.mouse_report_bytes(
            MouseEvent::Press(MouseButton::WheelDown),
            MouseMods::default(),
            0,
            0,
        );
        assert_eq!(wheel_down, Some(vec![0x1b, b'[', b'M', 65 + 32, 33, 33]));
        let wheel_up_release = g.mouse_report_bytes(
            MouseEvent::Release(MouseButton::WheelDown),
            MouseMods::default(),
            0,
            0,
        );
        assert_eq!(wheel_up_release, None);
    }

    #[test]
    fn mouse_modifiers_are_added_to_the_button_code() {
        let mut g = Grid::new(80, 24);
        g.feed(b"\x1b[?1000h\x1b[?1006h");
        let mods = MouseMods { shift: true, alt: false, ctrl: true };
        // Left (0) + shift (4) + ctrl (16) = 20.
        assert_eq!(
            g.mouse_report_bytes(MouseEvent::Press(MouseButton::Left), mods, 0, 0),
            Some(b"\x1b[<20;1;1M".to_vec())
        );
    }

    #[test]
    fn mouse_sgr_keeps_the_button_on_release_and_lifts_the_coordinate_limit() {
        let mut g = Grid::new(400, 24);
        g.feed(b"\x1b[?1000h\x1b[?1006h");
        // Press and release differ only in the final byte, so the button survives the release.
        assert_eq!(press(&g, 0, 0), Some(b"\x1b[<0;1;1M".to_vec()));
        assert_eq!(
            g.mouse_report_bytes(
                MouseEvent::Release(MouseButton::Right),
                MouseMods::default(),
                0,
                0
            ),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
        // Past the 223-column ceiling of the legacy encoding, which SGR does not have.
        assert_eq!(press(&g, 299, 0), Some(b"\x1b[<0;300;1M".to_vec()));
    }

    #[test]
    fn mouse_legacy_clamps_coordinates_past_its_ceiling() {
        let mut g = Grid::new(400, 24);
        g.feed(b"\x1b[?1000h");
        // 223 is the largest coordinate a 32-biased byte can carry; beyond it we clamp rather than
        // drop the report, so the mouse does not silently die over the right-hand edge.
        assert_eq!(press(&g, 222, 0), Some(vec![0x1b, b'[', b'M', 32, 255, 33]));
        assert_eq!(press(&g, 350, 0), Some(vec![0x1b, b'[', b'M', 32, 255, 33]));
    }

    #[test]
    fn mouse_motion_is_gated_on_the_tracking_mode() {
        let mut g = Grid::new(80, 24);
        let drag = MouseEvent::Motion(Some(MouseButton::Left));
        let hover = MouseEvent::Motion(None);
        let m = MouseMods::default();

        // 1000: presses only, no motion of any kind.
        g.feed(b"\x1b[?1000h");
        assert_eq!(g.mouse_report_bytes(drag, m, 0, 0), None);
        assert_eq!(g.mouse_report_bytes(hover, m, 0, 0), None);

        // 1002: drags, but still no button-less motion. Motion adds 32 to the button code.
        g.feed(b"\x1b[?1002h");
        assert_eq!(
            g.mouse_report_bytes(drag, m, 0, 0),
            Some(vec![0x1b, b'[', b'M', 32 + 32, 33, 33])
        );
        assert_eq!(g.mouse_report_bytes(hover, m, 0, 0), None);

        // 1003: everything, and button-less motion reports the "no button" code 3.
        g.feed(b"\x1b[?1003h");
        assert_eq!(
            g.mouse_report_bytes(hover, m, 0, 0),
            Some(vec![0x1b, b'[', b'M', 3 + 32 + 32, 33, 33])
        );
    }

    #[test]
    fn mouse_state_is_cleared_by_a_hard_reset() {
        let mut g = Grid::new(80, 24);
        g.feed(b"\x1b[?1003h\x1b[?1006h");
        assert_eq!(g.mouse_mode(), MouseMode::Any);
        g.feed(b"\x1bc"); // RIS
        assert_eq!(g.mouse_mode(), MouseMode::Off);
        assert_eq!(press(&g, 0, 0), None);
    }

    #[test]
    fn buf_cell_matches_the_viewport_at_every_offset() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4\r\n5"); // three lines in history, "4"/"5" on screen

        // The identity the selection relies on: a virtual-buffer coordinate resolves to the same
        // cell as the viewport coordinate it is currently displayed at, whatever the offset is.
        for off in 0..=g.history_len() {
            g.scroll_to_bottom();
            g.scroll_view(off as isize);
            for r in 0..g.rows {
                for c in 0..g.cols {
                    assert_eq!(
                        g.buf_cell(c, g.view_base() + r),
                        g.view_cell(c, r),
                        "offset {off}, cell ({c},{r})"
                    );
                }
            }
        }

        // Out of range in either axis reads as blank instead of panicking.
        assert_eq!(g.buf_cell(0, g.buf_end()), &BLANK);
        assert_eq!(g.buf_cell(0, g.buf_end() + 100), &BLANK);
        assert_eq!(g.buf_cell(99, g.buf_end() - 1), &BLANK);
        assert_eq!(g.buf_cell(99, g.buf_top()), &BLANK); // past the end of a trimmed history line
    }

    /// The bug the absolute coordinates exist to prevent: at HISTORY_MAX every new line evicts one
    /// from the front, so a deque-relative row would slide onto different text once per line.
    #[test]
    fn buf_cell_coordinates_survive_history_eviction() {
        const LINES: usize = HISTORY_MAX + 10;
        let mut g = Grid::new(10, 2);
        for i in 0..LINES {
            g.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(g.history_len(), HISTORY_MAX, "history must be at the cap for this to bite");

        // Pin a coordinate to a known line, then push enough output to evict a chunk of the front.
        let row = g.buf_end() - 3; // the newest line that has scrolled off
        let read = |g: &Grid, row| (0..10).map(|c| g.buf_cell(c, row).ch).collect::<String>();
        let text = read(&g, row);
        assert_eq!(text.trim_end(), format!("line{}", LINES - 2));
        for i in 0..100 {
            g.feed(format!("more{i}\r\n").as_bytes());
        }
        assert_eq!(read(&g, row), text, "the coordinate must still name the line it was taken from");

        // Rows evicted out of the deque read as blank rather than as whatever slid into their slot.
        assert!(g.buf_top() > 0);
        assert_eq!(g.buf_cell(0, g.buf_top() - 1), &BLANK);
    }

    /// ED3 (`clear`) drops the whole scrollback. `scrolled` is monotonic, so the screen keeps its
    /// coordinates across it — a deque-relative row would have every stored coordinate jump by the
    /// discarded `history_len()` at once, which is the fast way to reproduce the drift.
    #[test]
    fn erasing_the_scrollback_keeps_the_screen_rows_addressable() {
        let mut g = Grid::new(4, 2);
        g.feed(b"1\r\n2\r\n3\r\n4"); // "1"/"2" scrolled off, "3"/"4" on screen
        let base = g.view_base();
        assert_eq!(g.buf_cell(0, base + 1).ch, '4');

        // ED3 blanks the screen as well as the history, so the row goes blank — but it must still
        // *be* that row, not fall out of the buffer and resolve somewhere else.
        g.feed(b"\x1b[3J");
        assert_eq!(g.history_len(), 0);
        assert_eq!(g.view_base(), base, "the viewport's top row must not move");
        assert_eq!(g.buf_top(), base, "the dropped scrollback is simply no longer retained");
        g.feed(b"\x1b[HX");
        assert_eq!(g.buf_cell(0, base).ch, 'X');
    }

    #[test]
    fn ed3_clears_the_scrollback() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4");
        g.scroll_view(2);
        g.feed(b"\x1b[3J");
        assert_eq!(g.history_len(), 0);
        assert_eq!(g.view_offset(), 0);
    }

    #[test]
    fn resize_clamps_the_view_offset() {
        let mut g = Grid::new(3, 2);
        g.feed(b"1\r\n2\r\n3\r\n4");
        g.scroll_view(2);
        g.resize(5, 4);
        assert!(g.view_offset() <= g.history_len());
        let _ = view_lines(&g); // wider rows read blanks past each trimmed history row
    }

    #[test]
    fn cup_positions_cursor() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[3;5HX"); // row 3, column 5 (1-based) → (col=4, row=2)
        assert_eq!(g.cell(4, 2).ch, 'X');
        assert_eq!(g.cursor, (5, 2));
    }

    #[test]
    fn cursor_movement_relative() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[2B\x1b[3CY"); // down 2, right 3 → (3,2)
        assert_eq!(g.cell(3, 2).ch, 'Y');
    }

    #[test]
    fn sgr_sets_pen() {
        let mut g = Grid::new(10, 2);
        g.feed(b"\x1b[1;31mA\x1b[0mB");
        assert_eq!(g.cell(0, 0).fg, Color::Indexed(1));
        assert_ne!(g.cell(0, 0).flags & BOLD, 0);
        assert_eq!(g.cell(1, 0).fg, Color::Default);
        assert_eq!(g.cell(1, 0).flags, 0);
    }

    #[test]
    fn sgr_truecolor() {
        let mut g = Grid::new(4, 1);
        g.feed(b"\x1b[38;2;10;20;30mZ");
        assert_eq!(g.cell(0, 0).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn private_mode_is_swallowed() {
        // Private modes such as bracketed paste should not leak out as literal text.
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b[?2004hhi\x1b[?2004l");
        assert_eq!(g.to_lines()[0], "hi");
    }

    #[test]
    fn bracketed_paste_mode_is_tracked() {
        let mut g = Grid::new(10, 1);
        assert!(!g.bracketed_paste());
        g.feed(b"\x1b[?2004h");
        assert!(g.bracketed_paste());
        g.feed(b"\x1b[?2004l");
        assert!(!g.bracketed_paste());
    }

    #[test]
    fn erase_line_to_end() {
        let mut g = Grid::new(5, 1);
        g.feed(b"abc\r\x1b[K"); // carriage return to start of line, then erase to end of line
        assert_eq!(g.to_lines()[0], "");
    }

    #[test]
    fn erase_display_all() {
        let mut g = Grid::new(3, 2);
        g.feed(b"abcdef\x1b[2J");
        assert_eq!(g.to_lines(), vec!["", ""]);
    }

    #[test]
    fn delete_chars_shifts_left() {
        let mut g = Grid::new(6, 1);
        g.feed(b"abcdef\x1b[1G\x1b[2P"); // back to column 1, delete 2 characters
        assert_eq!(g.to_lines()[0], "cdef");
    }

    #[test]
    fn scroll_region_confines_scroll() {
        let mut g = Grid::new(3, 3);
        g.feed(b"\x1b[1;2r"); // scroll region confined to the first two rows
        g.feed(b"a\r\nb\r\nc\r\nd");
        assert_eq!(g.to_lines(), vec!["c", "d", ""]);
    }

    #[test]
    fn osc_sets_title() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b]0;hello\x07X");
        assert_eq!(g.title, "hello");
        assert_eq!(g.cell(0, 0).ch, 'X');
    }

    #[test]
    fn dsr_reports_cursor_position() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[3;5H"); // move to row 3, col 5 (1-based)
        g.feed(b"\x1b[6n"); // DSR: report cursor position
        assert_eq!(g.take_replies(), b"\x1b[3;5R");
    }

    #[test]
    fn dsr_reports_ok_status() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[5n"); // DSR: "are you OK?"
        assert_eq!(g.take_replies(), b"\x1b[0n");
    }

    #[test]
    fn da_reports_device_attributes() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[c"); // DA: primary device attributes
        assert_eq!(g.take_replies(), b"\x1b[?1;2c");
    }

    #[test]
    fn take_replies_drains_and_does_not_leak_into_the_grid() {
        let mut g = Grid::new(10, 5);
        g.feed(b"\x1b[6n");
        assert!(!g.take_replies().is_empty());
        assert!(g.take_replies().is_empty()); // second call: nothing left
        assert_eq!(g.to_lines()[0], ""); // the query never printed as visible text
    }

    #[test]
    fn osc7_sets_cwd() {
        let mut g = Grid::new(10, 1);
        g.feed(b"\x1b]7;file://host/Users/me/My%20Code\x07");
        assert_eq!(g.cwd(), "/Users/me/My Code");
    }

    #[test]
    fn osc7_percent_before_multibyte_does_not_panic() {
        // Regression test: a '%' immediately followed by a multibyte UTF-8 character (here "€",
        // 3 bytes) used to panic in percent_decode's raw &str byte-offset slicing, because the
        // slice end landed mid-character instead of on a char boundary.
        let mut g = Grid::new(10, 1);
        let mut msg = b"\x1b]7;file://host/%\xe2\x82\xac".to_vec(); // '%' + '€' (U+20AC)
        msg.push(0x07);
        g.feed(&msg); // must not panic
    }

    #[test]
    fn utf8_decoding() {
        let mut g = Grid::new(5, 1);
        g.feed("héλ".as_bytes());
        assert_eq!(g.cell(0, 0).ch, 'h');
        assert_eq!(g.cell(1, 0).ch, 'é');
        assert_eq!(g.cell(2, 0).ch, 'λ');
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        // Each CJK glyph takes two columns: the lead cell holds the char, the next is a '\0' trailer
        // flagged WIDE_TRAILER. The cursor advances by 2, keeping the grid in sync with the shell.
        let mut g = Grid::new(10, 1);
        g.feed("你a".as_bytes());
        assert_eq!(g.cell(0, 0).ch, '你');
        assert_eq!(g.cell(1, 0).ch, '\0');
        assert_ne!(g.cell(1, 0).flags & WIDE_TRAILER, 0);
        assert_eq!(g.cell(2, 0).ch, 'a'); // 'a' lands after the 2-cell wide char, not at col 1
        assert_eq!(g.cursor, (3, 0));
        assert_eq!(g.to_lines()[0], "你a"); // trailer placeholder is not exported
    }

    #[test]
    fn wide_char_wraps_at_right_margin() {
        // Two columns wide, only one left: the pair must not straddle the margin — it wraps whole.
        let mut g = Grid::new(3, 2);
        g.feed("ab你".as_bytes()); // 'a','b' fill cols 0,1; one col left → 你 wraps to row 1
        assert_eq!(g.cell(0, 0).ch, 'a');
        assert_eq!(g.cell(1, 0).ch, 'b');
        assert_eq!(g.cell(0, 1).ch, '你');
        assert_eq!(g.cell(1, 1).ch, '\0');
    }

    #[test]
    fn alt_screen_preserves_main() {
        let mut g = Grid::new(4, 2);
        g.feed(b"main");
        g.feed(b"\x1b[?1049h"); // enter alt screen
        assert_eq!(g.to_lines(), vec!["", ""]); // the alt screen is empty
        g.feed(b"XY");
        assert_eq!(g.to_lines(), vec!["XY", ""]);
        g.feed(b"\x1b[?1049l"); // leave alt screen
        assert_eq!(g.to_lines()[0], "main"); // main screen contents restored as-is
    }

    #[test]
    fn resize_shrink_keeps_top_when_cursor_fits() {
        // Unlike naive bottom-anchoring (always keep the last N rows), a shrink that still fits
        // the cursor should NOT drop anything — content stays anchored to the top.
        let mut g = Grid::new(4, 3);
        g.feed(b"ab\r\ncd"); // cursor ends on row 1 (0-indexed), row 2 was never written
        g.resize(4, 2);
        assert_eq!(g.to_lines(), vec!["ab", "cd"]);
    }

    #[test]
    fn resize_shrink_drops_rows_above_cursor() {
        let mut g = Grid::new(4, 3);
        g.feed(b"ab\r\ncd\r\nef"); // cursor ends on the last row
        g.resize(4, 2); // shrink to 2 rows: drop the oldest row so the cursor stays visible
        assert_eq!(g.to_lines(), vec!["cd", "ef"]);
    }

    #[test]
    fn resize_grow_keeps_content() {
        let mut g = Grid::new(4, 2);
        g.feed(b"ab\r\ncd");
        g.resize(4, 3);
        // Top-anchored: the original rows stay at the top; the extra row is blank at the bottom.
        assert_eq!(g.to_lines(), vec!["ab", "cd", ""]);
    }

    #[test]
    fn save_restore_cursor() {
        let mut g = Grid::new(10, 3);
        g.feed(b"\x1b[2;3H\x1b7\x1b[1;1HX\x1b8Y"); // save at (2,1), go to origin and write X, restore and write Y
        assert_eq!(g.cell(0, 0).ch, 'X');
        assert_eq!(g.cell(2, 1).ch, 'Y');
    }
}

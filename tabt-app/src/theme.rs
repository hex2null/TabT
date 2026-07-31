//! Color themes (classic styles).
//!
//! Centralizes theme data + the global state of the "current theme" (main-thread exclusive, read when drawing), shared by view /
//! app / sidebar without them depending on each other. CRT-style themes (classic green / amber) are treated as a "monochrome phosphor screen":
//! SGR colors are ignored, body text always uses the phosphor color (bold slightly brightened).
//!
//! The themes themselves live in a **config file**, not in this source, and are reached through the
//! runtime registry ([`names`], [`by_index`], [`index_of`]). Two copies of that file exist, in
//! precedence order (see [`ensure_loaded`]):
//!
//! 1. `~/.tabt/themes.conf` (`~/.tabt-dev` for a development build — `config::themes_file`): the
//!    user's, authoritative when present, only ever read by the app.
//! 2. `Contents/Resources/themes.conf` inside the app bundle (`config::bundled_themes_file`): the
//!    shipped read-only default, authored as `bundle/themes.conf`. It is what the user copy is
//!    seeded from on first run, and what an unbundled build has instead of one.
//!
//! The only theme still living in this source is [`DEFAULT_THEME`] — the base a parsed entry
//! inherits unset fields from, and the last resort if neither file can be read. See [`parse`] for
//! the format.

use std::cell::{Cell, RefCell};

/// Normalized RGB components ([0,1]).
pub type Rgb = (f64, f64, f64);

#[derive(Clone, Copy)]
pub struct Theme {
    pub fg: Rgb,                      // default foreground
    pub bg: Rgb,                      // default background + window/body base color
    pub palette: [(u8, u8, u8); 16],  // ANSI 16 colors (unused in mono themes)
    pub mono: bool,                   // true=monochrome phosphor screen, ignore SGR colors
}

impl Theme {
    /// Whether the theme's body background is a dark one (decides which way the panel and text
    /// tiers derived from it have to move).
    pub fn is_dark(&self) -> bool {
        luminance(self.bg) <= 0.5
    }

    /// Fill of the sidebar card: the body background stepped one notch deeper, so the panel is set
    /// apart by its own surface and the hairline only has to finish the edge.
    ///
    /// The step is a fixed *perceived* one — a luminance delta, not a constant blend fraction — so
    /// it lands the same on a near-black theme and on a white one, where the same fraction would be
    /// invisible on the first and a slab on the second. Blending toward black keeps the panel in the
    /// theme's own hue family. A background already near black has nowhere darker to go, so there
    /// the step is taken toward the foreground instead: still a distinct surface, just the only
    /// direction the theme leaves open (the phosphor CRT themes are the ones this covers).
    pub fn card_bg(&self) -> Rgb {
        const STEP: f64 = 0.03;
        let l = luminance(self.bg);
        if l < STEP * 2.0 {
            mix(self.bg, self.fg, self.blend_for(STEP))
        } else {
            // Darkening by t scales luminance to l*(1-t), so t = STEP/l lands the delta. Capped so a
            // very dark theme dims rather than collapsing to black.
            mix(self.bg, (0.0, 0.0, 0.0), (STEP / l).min(0.35))
        }
    }

    /// Hairline around the sidebar card. With [`Theme::card_bg`] now carrying the separation, this
    /// is only the edge that keeps the rounded corner crisp — deliberately fainter than the surface
    /// step, so the card is read as depth rather than as an outlined box.
    ///
    /// The step is solved for a fixed *perceived* separation rather than being a constant fraction,
    /// because a constant one leaves a low-contrast theme (a light one whose body text sits close to
    /// its background) with an edge indistinguishable from the body. Blending toward
    /// the foreground — rather than adding a flat gray offset — lightens dark themes and darkens
    /// light ones with one rule, and keeps the line inside the theme's own hue family, so an amber
    /// CRT gets a warm rim instead of a gray-brown smudge.
    ///
    /// It has to be measured from [`Theme::card_bg`], not from `bg`: the line is drawn *on* the card,
    /// and `card_bg` already stepped away from `bg`. On a dark theme the two steps go opposite ways
    /// (the card darkens, the line lightens) and the edge shows up by accident; on a light one both
    /// go down and the same delta lands the line exactly on the fill it is supposed to bound, which
    /// is why every light theme used to have no visible border at all.
    ///
    /// The step is smaller on a light theme, because the same delta does not read the same way in
    /// both directions: there the line is *darker* than the surface it bounds, and a dark line on a
    /// light field reads as a drawn outline, where the light rim a dark theme gets reads as an edge
    /// catching the light. Matching them by number makes the light one heavier than the dark one.
    pub fn card_border(&self) -> Rgb {
        let card = self.card_bg();
        let step = if self.is_dark() { 0.055 } else { 0.032 };
        mix(card, self.fg, self.blend_from(card, step))
    }

    /// The blend fraction toward `fg` that shifts `bg`'s luminance by `target`.
    fn blend_for(&self, target: f64) -> f64 {
        self.blend_from(self.bg, target)
    }

    /// The blend fraction toward `fg` that shifts `base`'s luminance by `target`. Capped, because on
    /// a theme whose fg and bg nearly coincide no blend reaches the target and an uncapped one
    /// would wash the surface out entirely.
    fn blend_from(&self, base: Rgb, target: f64) -> f64 {
        let contrast = (luminance(self.fg) - luminance(base)).abs().max(1e-3);
        (target / contrast).min(0.20)
    }

    /// The accent the sidebar's focused inputs draw in — the ring around the search and rename
    /// boxes, and the selection behind their text.
    ///
    /// Taken from the theme's own bright blue (palette 12): every scheme defines one, and it is the
    /// same family the theme picker's miniature already uses for a path, so a focused box reads as
    /// belonging to the scheme rather than to the app. It was a fixed amber until it had to live on
    /// half a dozen light themes and a green phosphor.
    ///
    /// Lifted toward the foreground when it does not separate from the card it is drawn on: this is
    /// a hairline ring, and a blue that sits at the card's own luminance — xterm's `#0000ee` on a
    /// near-black panel, which is what the base palette gives a theme that sets no colors of its
    /// own — disappears into it. A monochrome phosphor theme ignores SGR entirely and its palette
    /// means nothing, so there the phosphor itself is the accent.
    pub fn accent(&self) -> Rgb {
        if self.mono {
            return self.fg;
        }
        let (r, g, b) = self.palette[12];
        let c = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
        const MIN: f64 = 0.18; // luminance separation the ring needs from the card to be seen
        let d = (luminance(c) - luminance(self.card_bg())).abs();
        if d >= MIN {
            return c;
        }
        // At no separation at all this lands on the foreground, which the card is guaranteed to
        // show — the theme's body text is drawn on it.
        mix(c, self.fg, ((MIN - d) / MIN).clamp(0.0, 1.0))
    }

}

/// Linear blend a→b by t (0 = a, 1 = b). The single RGB-mixing helper used across the app.
pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Perceptual-ish luminance of an Rgb (0..1), used to tell light themes from dark ones.
fn luminance(c: Rgb) -> f64 {
    0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2
}

/// One entry of the theme registry: the name shown in the settings pop-up and stored in
/// `layout.conf`, plus the colors it selects.
#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub theme: Theme,
}

/// The theme a fresh install starts on. Kept here rather than in `config.rs` so the on-disk default
/// and the value the drawing code falls back to before the layout is read cannot drift apart. It is
/// a *name*, because with a user-editable theme file an index means nothing across installs.
pub const DEFAULT_NAME: &str = "Tokyo Night";

/// Standard xterm 16 colors (used by the default theme).
const BASE16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// The one theme still compiled in: a warm dark-gray background (#1d1c1b, not pure black) under
/// light text (#e8e8ec), the xterm 16 below it.
///
/// It is not "the default theme" the app starts on — that is [`DEFAULT_NAME`], which comes out of
/// the file like every other. This is the *base*: the values a `[theme]` section inherits for every
/// field it leaves unset or writes unparseably (see [`parse`]), and the single-entry registry left
/// if neither themes.conf can be read. Keeping one theme here rather than the whole shipped list
/// means the list has exactly one home — `bundle/themes.conf` — and cannot drift against a copy.
const DEFAULT_THEME: Theme = Theme {
    fg: (232.0 / 255.0, 232.0 / 255.0, 236.0 / 255.0),
    bg: (29.0 / 255.0, 28.0 / 255.0, 27.0 / 255.0),
    palette: BASE16,
    mono: false,
};

// ---- The registry: themes.conf, read once ------------------------------------------------------

thread_local! {
    /// The loaded theme list. Filled on first use rather than by an explicit call at startup, so
    /// no ordering against `config::load()` has to be maintained; never empty once filled.
    static REGISTRY: RefCell<Option<Vec<Entry>>> = const { RefCell::new(None) };
    static CURRENT: Cell<Theme> = Cell::new(default_theme());
}

/// Read `themes.conf` into the registry, once: the user's copy if there is one, otherwise the
/// read-only default shipped in the bundle, which is *copied out* to the user's path as it is read
/// — a first run leaves the user something to edit, byte for byte what the app just loaded.
///
/// The copy is a convenience, not a step of the load: a read-only config directory (or no `HOME` at
/// all) costs the user their editable copy, not their themes. Anything unusable at either level (an
/// unreadable file, no valid `[theme]` section, no bundle to read from — an unbundled `cargo build`
/// has none) falls through to the next, and finally to [`DEFAULT_THEME`] alone rather than leaving
/// the app themeless: with `panic = "abort"` an empty registry would be a crash, not a degraded UI.
fn ensure_loaded() {
    REGISTRY.with(|r| {
        if r.borrow().is_some() {
            return;
        }
        // A file that exists but holds no usable section is treated as absent, so a truncated or
        // half-edited user copy still lands on the shipped themes instead of on the bare base.
        let read = |path: Option<std::path::PathBuf>| -> Option<(String, Vec<Entry>)> {
            let text = std::fs::read_to_string(path?).ok()?;
            let list = parse(&text);
            (!list.is_empty()).then_some((text, list))
        };
        let user = crate::config::themes_file();
        let mut list = match read(Some(user.clone())) {
            Some((_, list)) => list,
            None => match read(crate::config::bundled_themes_file()) {
                Some((text, list)) => {
                    // Seeded only when there is nothing there — a user copy that failed to parse is
                    // still the user's, and having the app replace it would throw away the edit
                    // rather than the launch it broke.
                    if !user.exists() {
                        let _ = std::fs::create_dir_all(crate::config::dir());
                        let _ = std::fs::write(&user, text);
                    }
                    list
                }
                None => fallback_list(),
            },
        };
        // The pop-up is a list to find a name in, so it is ordered like one, whatever order the file
        // was written in — the shipped file groups its dark themes and then its light ones, which
        // reads well as a file and badly as a menu. Sorted here rather than in `parse` so the parser
        // stays a faithful reading of the file, and so a hand-edited user copy gets the same
        // treatment. Nothing is pinned to a position: `style` is stored by name, and every index the
        // app holds is resolved through this list.
        list.sort_by_key(|e| e.name.to_lowercase());
        *r.borrow_mut() = Some(list);
    });
}

/// Run `f` over the loaded registry.
fn with_registry<T>(f: impl FnOnce(&[Entry]) -> T) -> T {
    ensure_loaded();
    REGISTRY.with(|r| f(r.borrow().as_deref().unwrap_or(&[])))
}

/// The theme names, in display order (settings pop-up order = index order).
pub fn names() -> Vec<String> {
    with_registry(|list| list.iter().map(|e| e.name.clone()).collect())
}

/// How many themes are loaded (the valid range of a theme index).
pub fn count() -> usize {
    with_registry(|list| list.len())
}

/// Get a theme by index (out of bounds falls back to the default).
pub fn by_index(i: usize) -> Theme {
    with_registry(|list| match list.get(i) {
        Some(e) => e.theme,
        None => list.get(default_index_in(list)).map(|e| e.theme).unwrap_or(DEFAULT_THEME),
    })
}

/// The name at an index (out of bounds yields the default name, so a stale index can never panic
/// on the way back into `layout.conf`).
pub fn name_of(i: usize) -> String {
    with_registry(|list| list.get(i).map(|e| e.name.clone()).unwrap_or_else(|| DEFAULT_NAME.to_string()))
}

/// Reverse-look up an index by name. A name the file no longer defines lands on the default theme
/// (or, if the file dropped that too, on the first entry) rather than silently on whatever happens
/// to sit at index 0.
pub fn index_of(name: &str) -> usize {
    with_registry(|list| list.iter().position(|e| e.name == name).unwrap_or_else(|| default_index_in(list)))
}

/// Index of the theme a fresh install starts on.
pub fn default_index() -> usize {
    with_registry(default_index_in)
}

fn default_index_in(list: &[Entry]) -> usize {
    list.iter().position(|e| e.name == DEFAULT_NAME).unwrap_or(0)
}

/// The default theme's colors, used to seed [`CURRENT`] before the layout has been read.
fn default_theme() -> Theme {
    by_index(default_index())
}

/// The registry left when no themes.conf can be read at all: [`DEFAULT_THEME`] under a name of its
/// own, so the settings pop-up still offers something selectable and `layout.conf` still round-trips
/// a name. The shipped file does not define an entry by this name — it is only ever reached when
/// there is no file to define anything.
fn fallback_list() -> Vec<Entry> {
    vec![Entry { name: "Default".to_string(), theme: DEFAULT_THEME }]
}

// ---- themes.conf: the format ------------------------------------------------------------------

/// Parse `themes.conf`. Same hand-written INI shape as `layout.conf` (no serde, to keep the binary
/// small): `[theme]` opens an entry, `key = value` fills it, `#`/`;` lines and blanks are ignored.
///
/// ```ini
/// [theme]
/// name = Tokyo Night
/// fg = #c0caf5
/// bg = #1a1b26
/// mono = false
/// color0 = #15161e
/// ... through color15
/// ```
///
/// The file is authoritative: what it lists is what the settings pop-up offers, in its order — so
/// the shipped themes can be reordered or dropped, at the cost of a theme added by a later version
/// of the app not appearing in a user copy seeded by an earlier one. Parsing is deliberately
/// lenient, because the file is hand-edited and a malformed line must never take the app down: an
/// unnamed section, an unknown key and an unparseable color are each skipped, leaving that field at
/// the value it inherits from [`DEFAULT_THEME`] (`BASE16` for an incomplete palette).
fn parse(text: &str) -> Vec<Entry> {
    let base = DEFAULT_THEME;
    let mut out: Vec<Entry> = Vec::new();
    let mut cur: Option<(String, Theme)> = None;

    // Close the open section: an entry only counts once it has a name.
    fn flush(out: &mut Vec<Entry>, cur: Option<(String, Theme)>) {
        if let Some((name, theme)) = cur {
            if !name.is_empty() {
                out.push(Entry { name, theme });
            }
        }
    }

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&mut out, cur.take());
            if section.trim() == "theme" {
                cur = Some((String::new(), base));
            }
            continue; // an unknown section leaves `cur` empty, so its keys go nowhere
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        let (name, theme) = match cur.as_mut() {
            Some(c) => c,
            None => continue,
        };
        match key {
            "name" => *name = value.to_string(),
            "fg" => {
                if let Some(c) = parse_rgb(value) {
                    theme.fg = c;
                }
            }
            "bg" => {
                if let Some(c) = parse_rgb(value) {
                    theme.bg = c;
                }
            }
            "mono" => theme.mono = value.eq_ignore_ascii_case("true") || value == "1",
            _ => {
                if let Some(i) = key.strip_prefix("color").and_then(|n| n.parse::<usize>().ok()) {
                    if let (true, Some(c)) = (i < 16, parse_rgb8(value)) {
                        theme.palette[i] = c;
                    }
                }
            }
        }
    }
    flush(&mut out, cur);
    out
}

/// `#rrggbb` / `rrggbb` / `#rgb` -> 8-bit components. Anything else is None.
fn parse_rgb8(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#').unwrap_or(s).trim();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        6 => Some((byte(0)?, byte(2)?, byte(4)?)),
        // #rgb shorthand: each nibble doubled, so f -> ff.
        3 => {
            let nib = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
            Some((nib(0)?, nib(1)?, nib(2)?))
        }
        _ => None,
    }
}

/// Same, normalized to the [0,1] components the drawing code uses.
fn parse_rgb(s: &str) -> Option<Rgb> {
    let (r, g, b) = parse_rgb8(s)?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

/// Set the current theme (called when switching styles; takes effect once each view redraws).
pub fn set(t: Theme) {
    CURRENT.with(|c| c.set(t));
}

/// Read the current theme (called when drawing).
pub fn current() -> Theme {
    CURRENT.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The themes the app ships, as installed into the bundle. Compiled in for the tests only, so
    /// the shipped file's validity is checked at build time without those bytes reaching the
    /// release binary — which reads the installed copy at runtime.
    const SHIPPED: &str = include_str!("../../bundle/themes.conf");

    #[test]
    fn hex_forms() {
        assert_eq!(parse_rgb8("#abb2bf"), Some((0xab, 0xb2, 0xbf)));
        assert_eq!(parse_rgb8("abb2bf"), Some((0xab, 0xb2, 0xbf)));
        assert_eq!(parse_rgb8("#ABB2BF"), Some((0xab, 0xb2, 0xbf)));
        assert_eq!(parse_rgb8("#f0a"), Some((0xff, 0x00, 0xaa)));
        // Malformed input yields None rather than a panic — the file is hand-edited.
        assert_eq!(parse_rgb8("#abb2b"), None);
        assert_eq!(parse_rgb8("#gggggg"), None);
        assert_eq!(parse_rgb8(""), None);
        assert_eq!(parse_rgb8("rebeccapurple"), None);
    }

    #[test]
    fn parses_sections() {
        let list = parse(
            "# comment\n\
             [theme]\n\
             name = Mine\n\
             fg = #ffffff\n\
             bg = #000000\n\
             color3 = #ff0000\n\
             \n\
             [theme]\n\
             name = Phosphor\n\
             mono = 1\n",
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "Mine");
        assert_eq!(list[0].theme.fg, (1.0, 1.0, 1.0));
        assert_eq!(list[0].theme.palette[3], (255, 0, 0));
        // Untouched palette slots keep the base theme's.
        assert_eq!(list[0].theme.palette[4], BASE16[4]);
        assert!(!list[0].theme.mono);
        assert!(list[1].theme.mono);
    }

    #[test]
    fn skips_junk() {
        let list = parse(
            "[other]\n\
             name = Ignored\n\
             [theme]\n\
             fg = #ffffff\n\
             [theme]\n\
             name = Kept\n\
             bg = not-a-color\n\
             stray line without an equals sign\n\
             unknown_key = 7\n\
             color99 = #ff0000\n\
             colorX = #ff0000\n",
        );
        // An unknown section's keys go nowhere, and the nameless section is dropped.
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Kept");
        // Every bad key/value left its field at the inherited default.
        assert_eq!(list[0].theme.bg, DEFAULT_THEME.bg);
        assert_eq!(list[0].theme.palette, BASE16);
    }

    /// The shipped file is now the only home of the theme list, so its being parseable is the whole
    /// contract: a typo in it would ship an app with one theme (see `fallback_list`), and parsing is
    /// lenient enough that nothing else would complain.
    #[test]
    fn shipped_file_parses() {
        let list = parse(SHIPPED);
        assert_eq!(
            list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [
                // six dark…
                "Tokyo Night",
                "Catppuccin Mocha",
                "Dracula",
                "Nord",
                "Gruvbox Dark",
                "Solarized Dark",
                // …then six light.
                "Catppuccin Latte",
                "Rose Pine Dawn",
                "Gruvbox Light",
                "Solarized Light",
                "GitHub Light",
                "Ayu Light",
            ],
            "shipped themes.conf no longer holds the themes the app ships, in pop-up order"
        );
        // No entry may lose its name — an unnamed section is silently dropped, so a `name` line
        // gone missing would shorten the pop-up rather than fail anything.
        for e in &list {
            assert!(!e.name.is_empty());
        }
        // Every color line has to have landed: a mistyped one would leave BASE16 showing through.
        let tokyo = list.iter().find(|e| e.name == DEFAULT_NAME).expect("shipped file defines the default theme");
        assert_eq!(tokyo.theme.palette[1], (0xf7, 0x76, 0x8e));
        assert_ne!(tokyo.theme.palette, BASE16);
        // The check above only covers one theme, and the parser inherits a missing key from the base
        // rather than complaining, so a misspelled `colour3 =` in any *other* section would ship a
        // theme silently wearing Tokyo Night's value there. Walk the raw text instead: each section
        // has to spell out all 19 keys itself.
        // A section opens on a line that is exactly `[theme]` — splitting the text on that string
        // would also cut at the header comment, which names it in prose.
        let mut sections: Vec<Vec<&str>> = Vec::new();
        for line in SHIPPED.lines().map(str::trim) {
            if line == "[theme]" {
                sections.push(Vec::new());
            } else if let (Some(keys), Some(key)) = (sections.last_mut(), line.split('=').next()) {
                keys.push(key.trim());
            }
        }
        assert_eq!(sections.len(), list.len(), "a shipped [theme] section was dropped by the parser");
        for (i, keys) in sections.iter().enumerate() {
            let mut want: Vec<String> =
                ["name", "fg", "bg"].iter().map(|k| k.to_string()).collect();
            want.extend((0..16).map(|c| format!("color{c}")));
            for k in want {
                assert!(keys.contains(&k.as_str()), "shipped theme #{} has no `{k}` line", i + 1);
            }
        }
    }

    /// The name the app falls back to must be one the shipped file actually defines, or a fresh
    /// install would start on whatever sits at index 0 instead.
    #[test]
    fn default_name_is_shipped() {
        let list = parse(SHIPPED);
        assert!(list.iter().any(|e| e.name == DEFAULT_NAME));
    }
}

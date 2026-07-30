//! Persistence for the sidebar layout (~/.tabt/layout.conf; a development build uses its own
//! directory, see `branding.rs`).
//!
//! Stores "structure + light state": the [`Settings`] block (colors/font/window/terminal
//! preferences) + each group's name/collapsed state + each tab's title and last working directory
//! (cwd). On next launch a new shell is spawned directly in the restored cwd.
//!
//! The format is hand-written INI (no serde, to keep binary size down): `[section]` headers +
//! `key = value` lines. Ungrouped tabs go in an optional single `[tabs]` section (rendered at the top of the list);
//! groups are represented by **repeated `[group]` sections** to preserve order and allow duplicate names. Within a section each `tab = title` is
//! one tab, optionally followed by a `cwd = path` line (belonging to the nearest tab). Lines starting with `#`/`;` and
//! blank lines are ignored.
//!
//! ```ini
//! [settings]
//! style = Tokyo Night
//! font_family = Menlo
//! font_size = 13
//! sidebar_width = 200
//! sidebar_right = false
//! cursor_shape = bar
//! scrollback = 5000
//! shell = /bin/zsh
//!
//! [tabs]
//! tab = Loose tab
//! cwd = /Users/me
//!
//! [group]
//! name = Default
//! collapsed = false
//! tab = Terminal 1
//! cwd = /Users/me/proj
//! tab = server
//!
//! [group]
//! name = Work
//! collapsed = true
//! tab = api
//! ```
//!
//! Note: values are not escaped -- a newline in a group name/tab title would corrupt the format (renaming should forbid it).
//!
//! [`parse`] and [`render`] are the whole format; [`load`] and [`save`] are just `fs` around them.
//! The split is what makes the format testable: `load()` reads a path under `$HOME`, so a test
//! written against it would read (and `save` would overwrite) the user's live config.
//!
//! Two rules hold for every key. Unknown keys and unknown sections are **ignored**, so a config
//! written by a newer version stays readable by an older one and no migration step is ever needed;
//! and enumerated values are stored **by name**, never by index, so adding or reordering a variant
//! cannot silently repoint an existing config (the same reason `style` is a theme name).

use std::fs;
use std::path::PathBuf;

use crate::settings::{CursorShape, NewTabDir};

/// Highest valid status-dot color index (must match sidebar::DOT_COLORS: indices 0..=8).
const MAX_DOT: u8 = 8;

/// Persistent state of one tab (title + last working directory + status-dot color index + lock).
pub struct TabState {
    pub title: String,
    pub cwd: String,
    pub dot: u8,      // 0 = default/auto; 1..=8 = classic colors (see sidebar::DOT_COLORS)
    pub locked: bool, // locked tabs are protected from being closed (⌘W / the tab menu's Close)
}

/// One tab as the caller hands it back to [`save`]: title, cwd, dot color, locked.
pub type SavedTab = (String, String, u8, bool);
/// One group as the caller hands it back to [`save`]: name, collapsed, its tabs.
pub type SavedGroup = (String, bool, Vec<SavedTab>);

/// Everything in the `[settings]` block: the app-wide preferences, all of them live-editable
/// through the settings dialog and written back on every change.
pub struct Settings {
    pub style: String, // theme name, never an index (themes may be added/reordered)
    pub font_family: String,
    pub font_size: f64,
    pub sidebar_w: f64,
    pub sidebar_right: bool, // true = sidebar on the right
    pub show_border: bool,   // whether to draw the sidebar/header separator lines
    pub window_w: f64,       // saved window content width (0 = use the built-in default)
    pub window_h: f64,       // saved window content height (0 = use the built-in default)
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub scrollback: usize,   // lines of history per terminal
    pub shell: String,       // empty = resolve $SHELL at spawn time (see pty::spawn)
    pub new_tab_dir: NewTabDir,
    pub padding: f64,        // inset between the terminal's edge and the first cell
    pub opacity: f64,        // background opacity, 1.0 = opaque
}

/// Widest scrollback a config may ask for. Rows are stored trimmed, but this is still an
/// allocation per line — the cap is what keeps a typo in a hand-edited file from eating memory.
const MAX_SCROLLBACK: usize = 200_000;
/// Narrowest scrollback: shallower than a screen makes the wheel useless.
const MIN_SCROLLBACK: usize = 100;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            style: crate::theme::DEFAULT_NAME.to_string(),
            font_family: crate::settings::DEFAULT_FAMILY.to_string(),
            font_size: crate::settings::DEFAULT_SIZE,
            sidebar_w: crate::sidebar::SIDEBAR_W,
            sidebar_right: false,
            show_border: false,
            window_w: 0.0,
            window_h: 0.0,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            scrollback: tabt_core::DEFAULT_HISTORY_MAX,
            shell: String::new(),
            new_tab_dir: NewTabDir::Active,
            padding: crate::settings::DEFAULT_PAD,
            opacity: 1.0,
        }
    }
}

/// The layout read back: the settings block + the ungrouped tabs + each group.
pub struct Layout {
    pub settings: Settings,
    pub ungrouped: Vec<TabState>, // tabs not belonging to any group (rendered at the top of the list)
    pub groups: Vec<ParsedGroup>,
}

/// A group as parsed: name, collapsed, its tabs.
pub type ParsedGroup = (String, bool, Vec<TabState>);

pub fn dir() -> PathBuf {
    let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    // `.tabt` for the shipped app; a development build gets its own directory so the two don't
    // overwrite each other's layout (see `branding.rs`).
    p.push(crate::branding::CONFIG_DIR);
    p
}

fn file() -> PathBuf {
    let mut p = dir();
    p.push("layout.conf");
    p
}

/// The theme definitions, in the same directory (see `theme::parse` for the format). Kept in a file
/// of its own rather than as more sections of `layout.conf`, because [`save`] rewrites that file
/// wholesale from in-memory state on every change — hand-written themes living there would be
/// dropped by the first save that didn't model them. This one the app writes exactly once, when it
/// isn't there, copying [`bundled_themes_file`] out verbatim.
pub fn themes_file() -> PathBuf {
    let mut p = dir();
    p.push("themes.conf");
    p
}

/// The read-only theme defaults shipped inside the app bundle (`bundle/themes.conf` in the repo,
/// installed by the Makefile into `Contents/Resources/`), used when the user has no copy yet.
///
/// Resolved from the executable's own path — `Contents/MacOS/<exec>` → `../Resources` — rather than
/// through `NSBundle`, which would mean enabling another `objc2-foundation` class feature for one
/// lookup. A bare `cargo build` binary is not in a bundle, so the path it yields simply doesn't
/// exist and the caller falls through; `None` is the case where the executable can't be located at
/// all. The file is inside the signed bundle, so it is genuinely read-only: editing it invalidates
/// the app's signature, which is why the editable copy in `~/.tabt` exists at all.
pub fn bundled_themes_file() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut p = exe.parent()?.parent()?.to_path_buf();
    p.push("Resources");
    p.push("themes.conf");
    Some(p)
}

/// The INI section currently being parsed.
enum Section {
    None,
    Settings,
    Tabs, // [tabs]: ungrouped tabs
    Group,
}

/// Load the layout; if the file is missing or empty, provide a default group + a single tab.
pub fn load() -> Layout {
    parse(&fs::read_to_string(file()).unwrap_or_default())
}

/// Parse the config text. Every unrecognized line, key, section or value is skipped and leaves the
/// default in place — `panic = "abort"` turns one bad line in a hand-edited file into a dead app,
/// so this must never fail.
pub fn parse(text: &str) -> Layout {
    let mut s = Settings::default();
    let mut ungrouped: Vec<TabState> = Vec::new();
    let mut groups: Vec<ParsedGroup> = Vec::new();
    let mut section = Section::None;

    for raw in text.lines() {
        let line = raw.trim();
        // Skip blank lines and comments (# / ;).
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        // Header [name]: switch the current section; each [group] opens a new group (order preserved, duplicate names allowed).
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            match name.trim() {
                "settings" => section = Section::Settings,
                "tabs" => section = Section::Tabs,
                "group" => {
                    section = Section::Group;
                    groups.push((String::new(), false, Vec::new()));
                }
                _ => section = Section::None, // unknown section: ignore its keys
            }
            continue;
        }
        // key = value (value may contain '='; split only on the first '=').
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match section {
            Section::Settings => match key {
                "style" => s.style = value.to_string(),
                "font_family" => {
                    // "system" was an older selectable value; map it back to the default so the
                    // settings pop-up always has a matching entry to select.
                    if !value.is_empty() && value != "system" {
                        s.font_family = value.to_string();
                    }
                }
                "font_size" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.font_size = n;
                    }
                }
                "sidebar_width" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.sidebar_w = n;
                    }
                }
                "sidebar_right" => s.sidebar_right = truthy(value),
                "show_border" => s.show_border = truthy(value),
                "window_width" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.window_w = n;
                    }
                }
                "window_height" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.window_h = n;
                    }
                }
                // Enumerated settings are read by name; an unknown name is the default, never an
                // error (a config from a newer version may well carry a variant this build lacks).
                "cursor_shape" => s.cursor_shape = CursorShape::from_name(value),
                "cursor_blink" => s.cursor_blink = truthy(value),
                "new_tab_dir" => s.new_tab_dir = NewTabDir::from_name(value),
                "scrollback" => {
                    if let Ok(n) = value.parse::<usize>() {
                        s.scrollback = n.clamp(MIN_SCROLLBACK, MAX_SCROLLBACK);
                    }
                }
                "shell" => s.shell = value.to_string(),
                "padding" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.padding = n.clamp(0.0, crate::settings::MAX_PAD);
                    }
                }
                "opacity" => {
                    if let Ok(n) = value.parse::<f64>() {
                        s.opacity = n.clamp(crate::settings::MIN_OPACITY, 1.0);
                    }
                }
                _ => {} // unknown key: ignored, so a newer version's config stays readable
            },
            Section::Tabs => read_tab_key(&mut ungrouped, key, value),
            Section::Group => {
                if let Some(g) = groups.last_mut() {
                    match key {
                        "name" => g.0 = value.to_string(),
                        "collapsed" => g.1 = truthy(value),
                        // tab / cwd / dot / lock belong to the nearest tab in this section.
                        _ => read_tab_key(&mut g.2, key, value),
                    }
                }
            }
            Section::None => {}
        }
    }

    // When entirely empty (no ungrouped tabs and no tabs inside groups), add one ungrouped tab so a terminal is available.
    let total_tabs = ungrouped.len() + groups.iter().map(|g| g.2.len()).sum::<usize>();
    if total_tabs == 0 {
        ungrouped.push(TabState { title: "Terminal 1".to_string(), cwd: String::new(), dot: 0, locked: false });
    }

    Layout { settings: s, ungrouped, groups }
}

/// The tab-level keys, shared by `[tabs]` and `[group]`: `tab` opens a new one, the rest attach to
/// the nearest one above them.
fn read_tab_key(tabs: &mut Vec<TabState>, key: &str, value: &str) {
    match key {
        "tab" => tabs.push(TabState { title: value.to_string(), cwd: String::new(), dot: 0, locked: false }),
        "cwd" => {
            if let Some(t) = tabs.last_mut() {
                t.cwd = value.to_string();
            }
        }
        "dot" => {
            if let (Some(t), Ok(n)) = (tabs.last_mut(), value.parse::<u8>()) {
                t.dot = if n <= MAX_DOT { n } else { 0 }; // ignore out-of-range indices
            }
        }
        "lock" => {
            if let Some(t) = tabs.last_mut() {
                t.locked = truthy(value);
            }
        }
        _ => {}
    }
}

/// A boolean value: `true` (any case) or `1`; anything else is false.
fn truthy(value: &str) -> bool {
    value.eq_ignore_ascii_case("true") || value == "1"
}

/// Write the layout back.
pub fn save(s: &Settings, ungrouped: &[SavedTab], groups: &[SavedGroup]) {
    let _ = fs::create_dir_all(dir());
    let _ = fs::write(file(), render(s, ungrouped, groups));
}

/// Render the config text: the counterpart of [`parse`], and the whole of the write side.
fn render(s: &Settings, ungrouped: &[SavedTab], groups: &[SavedGroup]) -> String {
    // For each tab, write one tab= line and optional cwd=/dot=/lock= lines.
    let write_tabs = |out: &mut String, tabs: &[SavedTab]| {
        for (title, cwd, dot, locked) in tabs {
            out.push_str(&format!("tab = {}\n", title));
            if !cwd.is_empty() {
                out.push_str(&format!("cwd = {}\n", cwd));
            }
            if *dot != 0 {
                out.push_str(&format!("dot = {}\n", dot));
            }
            if *locked {
                out.push_str("lock = true\n");
            }
        }
    };

    let mut out = String::new();
    out.push_str("[settings]\n");
    out.push_str(&format!("style = {}\n", s.style));
    out.push_str(&format!("font_family = {}\n", s.font_family));
    out.push_str(&format!("font_size = {}\n", s.font_size));
    out.push_str(&format!("sidebar_width = {}\n", s.sidebar_w));
    out.push_str(&format!("sidebar_right = {}\n", s.sidebar_right));
    out.push_str(&format!("show_border = {}\n", s.show_border));
    if s.window_w > 0.0 && s.window_h > 0.0 {
        out.push_str(&format!("window_width = {}\n", s.window_w));
        out.push_str(&format!("window_height = {}\n", s.window_h));
    }
    out.push_str(&format!("cursor_shape = {}\n", s.cursor_shape.name()));
    out.push_str(&format!("cursor_blink = {}\n", s.cursor_blink));
    out.push_str(&format!("scrollback = {}\n", s.scrollback));
    out.push_str(&format!("new_tab_dir = {}\n", s.new_tab_dir.name()));
    out.push_str(&format!("padding = {}\n", s.padding));
    out.push_str(&format!("opacity = {}\n", s.opacity));
    // Only when set: an empty value would read as "a shell called nothing" to a human editing the
    // file, where the absent key plainly means "whatever $SHELL says".
    if !s.shell.is_empty() {
        out.push_str(&format!("shell = {}\n", s.shell));
    }
    if !ungrouped.is_empty() {
        out.push_str("\n[tabs]\n");
        write_tabs(&mut out, ungrouped);
    }
    for (name, collapsed, tabs) in groups {
        out.push_str("\n[group]\n");
        out.push_str(&format!("name = {}\n", name));
        out.push_str(&format!("collapsed = {}\n", collapsed));
        write_tabs(&mut out, tabs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(title: &str, cwd: &str, dot: u8, locked: bool) -> SavedTab {
        (title.to_string(), cwd.to_string(), dot, locked)
    }

    /// Everything written must come back, so a settings change survives the next launch. Goes
    /// through `render`/`parse` rather than `save`/`load`, which would read and overwrite the
    /// developer's own ~/.tabt-dev/layout.conf.
    #[test]
    fn round_trip_preserves_every_field() {
        let s = Settings {
            style: "Gruvbox Light".to_string(),
            font_family: "Monaco".to_string(),
            font_size: 15.0,
            sidebar_w: 240.0,
            sidebar_right: true,
            show_border: true,
            window_w: 1200.0,
            window_h: 800.0,
            cursor_shape: CursorShape::Underline,
            cursor_blink: true,
            scrollback: 12_000,
            shell: "/bin/bash".to_string(),
            new_tab_dir: NewTabDir::Active,
            padding: 4.0,
            opacity: 0.85,
        };
        let ungrouped = vec![tab("Loose", "/tmp", 3, true)];
        let groups = vec![("Work".to_string(), true, vec![tab("api", "/srv", 0, false), tab("web", "", 8, false)])];

        let back = parse(&render(&s, &ungrouped, &groups));

        let g = back.settings;
        assert_eq!(g.style, "Gruvbox Light");
        assert_eq!(g.font_family, "Monaco");
        assert_eq!(g.font_size, 15.0);
        assert_eq!(g.sidebar_w, 240.0);
        assert!(g.sidebar_right);
        assert!(g.show_border);
        assert_eq!((g.window_w, g.window_h), (1200.0, 800.0));
        assert!(g.cursor_shape == CursorShape::Underline);
        assert!(g.cursor_blink);
        assert_eq!(g.scrollback, 12_000);
        assert_eq!(g.shell, "/bin/bash");
        assert!(g.new_tab_dir == NewTabDir::Active);
        assert_eq!(g.padding, 4.0);
        assert_eq!(g.opacity, 0.85);

        assert_eq!(back.ungrouped.len(), 1);
        assert_eq!(back.ungrouped[0].title, "Loose");
        assert_eq!(back.ungrouped[0].cwd, "/tmp");
        assert_eq!(back.ungrouped[0].dot, 3);
        assert!(back.ungrouped[0].locked);

        assert_eq!(back.groups.len(), 1);
        let (name, collapsed, tabs) = &back.groups[0];
        assert_eq!(name, "Work");
        assert!(collapsed);
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[1].title, "web");
        assert_eq!(tabs[1].dot, 8);
    }

    /// A config from a newer version (unknown key, unknown section) must still load, and a
    /// hand-edited garbage line must leave the default rather than abort the process.
    #[test]
    fn unknown_and_malformed_input_falls_back_to_defaults() {
        let text = "\
[settings]
style = Nord
font_size = not-a-number
cursor_shape = hologram
scrollback = 7
opacity = 12
future_key = 42
no equals sign here

[experiments]
style = Should Be Ignored

[tabs]
tab = One
dot = 99
";
        let l = parse(text);
        assert_eq!(l.settings.style, "Nord", "a known key still applies");
        assert_eq!(l.settings.font_size, crate::settings::DEFAULT_SIZE, "an unparseable value keeps the default");
        assert!(l.settings.cursor_shape == CursorShape::Block, "an unknown enum name is the default");
        assert_eq!(l.settings.scrollback, MIN_SCROLLBACK, "an out-of-range number is clamped, not rejected");
        assert_eq!(l.settings.opacity, 1.0);
        assert_eq!(l.ungrouped.len(), 1);
        assert_eq!(l.ungrouped[0].title, "One");
        assert_eq!(l.ungrouped[0].dot, 0, "an out-of-range dot index falls back to the default");
    }

    /// An empty file must still yield a terminal to show.
    #[test]
    fn empty_config_yields_one_tab() {
        let l = parse("");
        assert_eq!(l.ungrouped.len(), 1);
        assert!(l.groups.is_empty());
        assert_eq!(l.settings.style, crate::theme::DEFAULT_NAME);
    }
}

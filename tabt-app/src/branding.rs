//! Product identity, fixed at build time.
//!
//! The shipped app is "TabT"; the local development build deliberately uses a different name,
//! bundle id, executable name and config directory (see the Makefile) so a work-in-progress
//! build can be installed and run next to the real one without the two fighting over the same
//! `~/.tabt` layout file or being indistinguishable in the Dock, the menu bar and `ps`.
//!
//! Both values come from the environment at compile time (`make` sets them) and fall back to
//! the release identity, so a plain `cargo build` still produces the shipping app.

/// Displayed name: window title fallback, app menu, quit dialog, empty-pane placeholder.
pub const APP_NAME: &str = match option_env!("TABT_APP_NAME") {
    Some(name) => name,
    None => "TabT",
};

/// Directory under `$HOME` holding `layout.conf` (see `config.rs`).
pub const CONFIG_DIR: &str = match option_env!("TABT_CONFIG_DIR") {
    Some(dir) => dir,
    None => ".tabt",
};

---
name: run-tabt
description: Build, launch, screenshot and drive TabT, the native macOS terminal emulator in this repo. Use when asked to run or start the app, take a screenshot of it, reproduce a rendering or VT/ANSI bug in the real app, drive a tab, or confirm a change works outside the unit tests.
---

# Running and driving TabT

TabT is a native AppKit GUI, so there is no `npm start` to watch and no headless mode.
Everything an agent needs goes through **`.claude/skills/run-tabt/driver.mjs`**, which
launches an instance against a scratch `$HOME` and then reaches into it three ways:

| Channel | Command | Layer it covers | Needs |
|---|---|---|---|
| PTY feed | `feed` | `tabt-core` — the VT parser and renderer, which is what most PRs here touch | nothing |
| Screenshot | `shot` | the AppKit renderer: does the screen match the parser? | Screen Recording |
| Keystrokes | `keys` | `tabt-app` — tabs, sidebar, menus | Accessibility |

All paths below are relative to the repo root. **Read the Gotchas before `make run`** —
it kills processes by name and can take down the terminal you are working in.

## Prerequisites

macOS with Xcode command line tools and Rust. Nothing to install beyond that; the driver
uses only `node`, `osascript`, `screencapture` and `ps`, all of which ship with the system.

`shot` and `keys` are TCC-gated: the terminal app running the driver needs **Screen
Recording** and **Accessibility** respectively (System Settings → Privacy & Security).
Check both, against a running instance, with:

```bash
node .claude/skills/run-tabt/driver.mjs doctor
```

```
OK    app bundle built  -- /Users/gary/Code/tabt/dist/TabT Dev.app/Contents/MacOS/tabt-dev
OK    instance running  -- pid 50414
OK    Accessibility (window bounds, needed by shot/keys)  -- x,y,w,h = 256,116,1000,620
OK    Screen Recording (screencapture)  -- .../shots/doctor.png (331445 bytes)
OK    PTY feed channel  -- /dev/ttys035
```

`feed` needs no permission, so a check that must work anywhere sticks to it.

## Build

```bash
make            # builds dist/TabT Dev.app (dev identity) — use this, not `make run`
```

`make test` runs the `tabt-core` unit tests (pure logic, no GUI). A single one:

```bash
make test
cargo test -p tabt-core irm_inserting_into_a_wide_glyph_blanks_both_of_its_halves
```

`make test` covers **only** `tabt-core`. The AppKit half is not unit-tested at all, so
after touching `tabt-app` build it explicitly and then drive it:

```bash
cargo build -p tabt-app
```

## Run (agent path)

One self-checking pass — launches, asserts, tears down, exits nonzero on failure:

```bash
node .claude/skills/run-tabt/driver.mjs smoke
```

```
PASS  fixture shell ran inside a tab
PASS  login shell + TERM wiring  -- shell=-zsh ... term=xterm-256color term_program=TabT
PASS  DSR round-trip through the live app  -- "ESC[1;1R"
PASS  parser honoured CUP (expect ESC[10;5R)  -- "ESC[10;5R"
PASS  device attributes reply  -- "ESC[?1;2c"
PASS  wrote layout.conf under the scratch HOME
PASS  app still running (no crash)
PASS  restored session started
PASS  shell spawned in the restored cwd
9/9 checks passed
```

To poke at it interactively, keep an instance up and drive it:

```bash
node .claude/skills/run-tabt/driver.mjs launch          # prints the pid, returns immediately
node .claude/skills/run-tabt/driver.mjs feed '\e[31mred\e[0m and \e[1mbold\e[0m\r\n'
node .claude/skills/run-tabt/driver.mjs shot /tmp/tabt.png
node .claude/skills/run-tabt/driver.mjs keys t cmd      # ⌘T — new tab
node .claude/skills/run-tabt/driver.mjs menu 'Settings…'  # click a menu item
node .claude/skills/run-tabt/driver.mjs windows         # list the instance's windows
node .claude/skills/run-tabt/driver.mjs tty             # list each tab's PTY slave
node .claude/skills/run-tabt/driver.mjs feed 'into tab 2\r\n' 1
node .claude/skills/run-tabt/driver.mjs quit
```

`feed` writes raw bytes to a tab's PTY **slave**, so the app sees exactly what the shell
would have printed — this is the way to reproduce any parser or rendering bug in the live
app. It interprets `\e \n \r \t \a \0`. The trailing argument selects the tab: a 0-based
index, or `last`. Use `0` on a fresh instance and `last` right after `keys t cmd`, because
those are the two cases where the tab you are writing to is also the one on screen (see
Gotchas).

### Visual regression in one command

`scene` renders a page that exercises SGR, the DEC line-drawing charset, wide glyphs,
IRM-into-a-wide-pair, and a custom tab stop, then screenshots it:

```bash
node .claude/skills/run-tabt/driver.mjs scene vt /tmp/scene.png
```

Screenshots come back at Retina 2x, which is large to read; downscale before viewing:

```bash
sips -Z 1000 /tmp/scene.png --out /tmp/scene-small.png
```

Expected in that shot — the row `IRM+wide:  Z x`, one space where `中` was. A doubled or
clipped glyph there means a wide-pair regression in `insert_chars`.

## Run (human path)

```bash
make run
```

Builds, bundles, **`killall tabt-dev`**, and `open`s the app. Fine when a human wants a
window; see the first Gotcha before using it from an agent session.

## Gotchas

- **`make run` kills every `tabt-dev` by name.** If this session is hosted inside a TabT
  Dev window, `make run` kills its own terminal mid-command. The driver never matches by
  name: it signals only the pid it spawned, and `killOurs` refuses outright if that pid is
  an ancestor of the driver process. Prefer `make` + `driver.mjs launch`.
- **The driver runs against a scratch `$HOME`** (`$TMPDIR/tabt-driver-home`), so the real
  `~/.tabt-dev` layout is never touched and a fresh instance always comes up with exactly
  one tab. That is also why tab index 0 is unambiguous right after `launch`.
- **Mouse gestures cannot be synthesized.** `osascript -e 'tell application "System Events"
  to click at {700, 400}'` returns the element under the point
  (`window Terminal 1 of application process tabt-dev`) and delivers no click to the app.
  So the two-owner mouse routing in `view.rs` — click, drag, wheel, Shift-to-select — is
  the one area this harness cannot reach. Those changes need a human with `make run`.
  Keystrokes are unaffected: `keys` works.
- **`feed` bypasses the shell.** The bytes land on screen as terminal *output*; zsh has no
  idea, so it will not repaint its prompt. A scene starting with `\e[2J\e[H` wipes the
  visible prompt — harmless, press Enter (or start a new tab) to get it back.
- **A key equivalent is not guaranteed to arrive; a menu click is.** `keys t cmd` opens a new
  tab, but `keys , cmd` does nothing at all — AppKit does not route that synthesized event to
  the Settings menu item, which `menu 'Settings…'` opens reliably. Prefer `menu` whenever the
  action has a menu entry, and verify with `windows` rather than assuming.
- **`shot` only ever photographs window 1.** Auxiliary windows are missed, and the Settings
  window in particular opens off-screen (`2622,-140`), where a region capture comes back
  black. `windows` lists what exists; a shot of anything but the terminal needs its own
  bounds.
- **`launch` wipes the scratch `$HOME` every time.** To launch against a hand-edited
  `layout.conf` — a theme, a seeded cwd — use `launch --keep`, or the file you just wrote is
  deleted before the app ever reads it.
- **`feed` picks a tab, `shot` photographs the visible one — and nothing links them.** Feed
  tab 0 while tab 2 is on screen and the screenshot shows tab 2's contents, silently. The
  driver cannot ask which tab is visible: the sidebar is self-drawn, so it exposes nothing
  to the Accessibility API, and the app binds no ⌘1..⌘9 for `keys` to press. Two positions
  are known-good — tab `0` on a fresh instance, and `last` right after `keys t cmd`. `scene`
  defaults to `last` for this reason.
- **Tab order is ascending child pid.** Each tab is one forked login zsh, so
  `pgrep -P <app pid>` sorted ascending is the sidebar order for a driver-launched
  instance. It can diverge if tabs are closed and reopened.
- **Two build identities, and they must not be mixed.** Dev is `tabt-dev` + `~/.tabt-dev`
  + `dev.local.tabt.dev`; release is `tabt` + `~/.tabt` + `dev.local.tabt`. The driver
  picks whichever bundle exists in `dist/` and takes the config dir *and* bundle id from
  that same entry — hardcoding either one makes the layout assertions read a file the app
  never writes.
- **`quit` (SIGTERM) skips the final save.** `AppController::save()` already runs on every
  layout mutation, so little is lost, but `applicationWillTerminate:` → `persist()` is the
  last one. `driver.mjs quit --graceful` runs it via `osascript ... to quit`. That path can
  block on ⌘Q's confirmation dialog if a tab has a foreground job.
- **If you edit the driver: keep `stdio: 'ignore'` in `launch`.** A piped stdio keeps a
  handle referenced in node's event loop, so `launch` prints its pid and then hangs forever
  instead of exiting.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `no built app bundle found -- run \`make\` first` | Nothing in `dist/`. Run `make`. |
| `pid N from the pidfile is gone` | The instance died or was killed. `driver.mjs launch` again. |
| `doctor` fails on Accessibility | Grant Accessibility to the terminal running the driver; `shot` and `keys` both depend on the AX window-bounds query. |
| Screenshot shows the desktop, not the app | Screen Recording not granted to the terminal running the driver. `doctor` flags a suspiciously small file. |
| `driver.mjs launch` never returns | Someone reintroduced a piped `stdio` in `launch` — see the last Gotcha. |
| A `scene` assertion looks wrong at a column | Column arguments are 1-based (`\e[12G`), and a wide glyph occupies two of them. Aiming one column off lands on the neighbour and silently proves nothing. |
| Screenshot shows a prompt instead of what you just fed | You fed a tab that is not the visible one. Feed `0` on a fresh instance, or `last` after `keys t cmd`. |

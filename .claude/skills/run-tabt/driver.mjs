#!/usr/bin/env node
// Launch and drive TabT.app from a script.
//
// TabT is a native AppKit GUI with no remote-control surface, so this driver reaches into
// a running instance three different ways. Each covers a different layer of the app, and
// which one you want depends on what you changed:
//
//   1. PTY feed (`feed`)  -- the app is a terminal emulator, so every tab is a PTY whose
//      slave device (/dev/ttysNNN) is writable by us. Bytes written there are exactly what
//      the shell would have printed: GCD dispatch source -> Grid::feed -> parser -> render.
//      This is the handle for anything in tabt-core, which is what most PRs here touch.
//   2. Screenshot (`shot`) -- `screencapture -R` over the window rectangle read out of the
//      Accessibility API. This is how you check that the *renderer* agrees with the parser.
//   3. Keystrokes (`keys`) -- System Events, for the AppKit layer: tabs, sidebar, menus.
//
// (2) and (3) are TCC-gated: the terminal app running this driver needs Screen Recording
// and Accessibility respectively. `doctor` tells you whether they are granted. (1) needs
// no permission at all, so a smoke test that must run anywhere sticks to it.
//
// A fourth channel runs the other way: the app spawns a login zsh, so pointing it at a
// scratch $HOME whose .zshrc is ours puts an agent *inside* the GUI that can query the
// terminal and write the answers to files we read back. A DSR round-trip (`ESC[6n` ->
// `ESC[row;colR`) exercises the whole data flow, and preceding it with a CUP makes the
// reply an assertion about the parser rather than just a liveness check.
//
// Usage:
//   node .claude/skills/run-tabt/driver.mjs smoke        # full run + asserts, nonzero exit on failure
//   node .claude/skills/run-tabt/driver.mjs launch [--keep] # leave it running (--keep: don't wipe $HOME)
//   node .claude/skills/run-tabt/driver.mjs feed '\e[31mred\r\n' [tab]  # tab: index or "last"
//   node .claude/skills/run-tabt/driver.mjs shot [file]  # screenshot the window
//   node .claude/skills/run-tabt/driver.mjs keys t cmd   # send a keystroke (here: new tab)
//   node .claude/skills/run-tabt/driver.mjs menu 'Settings…'   # click a menu item (⌘, is not deliverable)
//   node .claude/skills/run-tabt/driver.mjs scene vt     # render a VT torture page, then shot
//   node .claude/skills/run-tabt/driver.mjs tty [n]      # print tab n's PTY slave path
//   node .claude/skills/run-tabt/driver.mjs doctor       # check the TCC permissions
//   node .claude/skills/run-tabt/driver.mjs quit [--graceful]  # stop it (--graceful runs persist())

import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync, realpathSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
// Either build identity will do: we spawn the executable directly, so nothing here depends on
// the bundle id or the app name. The dev bundle comes first because `make` is the default
// target. Each identity is a *pair* though -- it names its executable and its config directory
// differently (see the identity table in CLAUDE.md) -- so the config dir has to be taken from
// whichever bundle we ended up with, not hardcoded, or the layout assertions below read a path
// the app never writes.
const APP_BUILDS = [
  { bin: join(REPO, 'dist/TabT Dev.app/Contents/MacOS/tabt-dev'), configDir: '.tabt-dev', bundleId: 'dev.local.tabt.dev' },
  { bin: join(REPO, 'dist/TabT.app/Contents/MacOS/tabt'), configDir: '.tabt', bundleId: 'dev.local.tabt' },
];
const BUILD = APP_BUILDS.find((b) => existsSync(b.bin)) ?? APP_BUILDS[0];
const APP_BIN = BUILD.bin;
const HOME = join(process.env.TMPDIR || '/tmp', 'tabt-driver-home');
const PROOF = join(HOME, 'proof');
const PIDFILE = join(HOME, 'tabt.pid');
const MARKER_CWD = join(HOME, 'marker-cwd');
const CONF = join(HOME, BUILD.configDir, 'layout.conf');
const SHOTS = join(HOME, 'shots');

const log = (...a) => console.log(...a);

// ---------------------------------------------------------------------------
// Safety: this session may itself be running inside a TabT window. Killing by
// name (`killall tabt`, which is what `make run` does) would take down the
// terminal hosting the agent. We only ever signal a pid we spawned ourselves.
// ---------------------------------------------------------------------------
function ancestorTabtPids() {
  const pids = new Set();
  let pid = process.pid;
  for (let i = 0; i < 40 && pid > 1; i++) {
    let out;
    try {
      out = execFileSync('ps', ['-o', 'ppid=,comm=', '-p', String(pid)], { encoding: 'utf8' }).trim();
    } catch { break; }
    if (!out) break;
    const m = out.match(/^(\d+)\s+(.*)$/);
    if (!m) break;
    if (/tabt$/.test(m[2].trim())) pids.add(pid);
    pid = Number(m[1]);
  }
  return pids;
}

function killOurs(pid) {
  const forbidden = ancestorTabtPids();
  if (forbidden.has(pid)) {
    throw new Error(`refusing to kill pid ${pid}: it is an ancestor of this process (the TabT hosting this session)`);
  }
  try { process.kill(pid, 'SIGTERM'); } catch { /* already gone */ }
}

// ---------------------------------------------------------------------------
// The fixture $HOME. TabT reads $HOME/<config dir>/layout.conf (config.rs `dir()`, the
// directory itself coming from branding.rs), and the login zsh it spawns reads $HOME/.zshrc
// -- so one env var isolates the app's config from the real one AND gives us our hook
// inside the tab.
// ---------------------------------------------------------------------------
// NOTE: String.raw stops backslash escapes being eaten by JS, but it does NOT stop
// ${...} interpolation -- so this shell code must avoid ${...} entirely. That is why
// the ESC-byte -> "ESC" prettifying happens in Node (see `readReply`) rather than in
// zsh's ${var//.../...}.
const ZSHRC = String.raw`
# Fixture rc sourced by the login zsh that TabT spawns inside a tab.
# Everything here runs *inside the running GUI app*.
PROOF="$TABT_PROOF"
mkdir -p "$PROOF"

print -r -- "shell=$0 pid=$$ term=$TERM term_program=$TERM_PROGRAM cwd=$PWD" > "$PROOF/boot.txt"

# Ask the terminal a question and read its answer back off the tty.
# Needs raw mode: otherwise the reply is line-buffered and echoed back at us.
tabt_query() {
  local old resp c i
  old=$(stty -g </dev/tty)
  stty raw -echo min 0 time 20 </dev/tty      # up to 2.0s for a reply
  printf '%b' "$1" > /dev/tty
  resp=""
  for i in {1..32}; do
    c=$(dd bs=1 count=1 </dev/tty 2>/dev/null)
    [[ -z $c ]] && break
    resp+="$c"
    [[ $c == "$2" ]] && break
  done
  stty "$old" </dev/tty
  printf '%s' "$resp"
}

# 1. Plain DSR: proves the full PTY -> parser -> reply -> PTY loop is live.
tabt_query '\033[6n' R > "$PROOF/dsr_plain.txt"

# 2. CUP then DSR: the reply is now an assertion that the parser moved the cursor.
tabt_query '\033[10;5H\033[6n' R > "$PROOF/dsr_after_cup.txt"

# 3. Device attributes.
tabt_query '\033[c' c > "$PROOF/da.txt"

# 4. OSC 7 cwd report -- TabT percent-decodes this into Grid.cwd and persists it.
mkdir -p "$TABT_MARKER_CWD"
cd "$TABT_MARKER_CWD"
printf '\033]7;file://%s%s\a' "$HOST" "$PWD"

# 5. Render some real output (SGR + wide chars) so the window is not blank.
print -P '%F{green}TabT driver%f: SGR + wide chars 中文 ok'

print -r -- done > "$PROOF/ready.txt"
`;

function setupHome() {
  rmSync(HOME, { recursive: true, force: true });
  mkdirSync(PROOF, { recursive: true });
  writeFileSync(join(HOME, '.zshrc'), ZSHRC);
  // A login shell also reads .zprofile; keep it quiet and predictable.
  writeFileSync(join(HOME, '.zprofile'), '# intentionally empty (fixture)\n');
}

function launch({ fresh = true } = {}) {
  if (!existsSync(APP_BIN)) {
    throw new Error(
      `no built app bundle found -- run \`make\` first (NOT \`make run\`: it killalls by\n` +
      `executable name, which would take down a TabT window hosting this session).\n` +
      `Looked for:\n${APP_BUILDS.map((b) => `  ${b.bin}`).join('\n')}`,
    );
  }
  if (fresh) setupHome();
  const child = spawn(APP_BIN, [], {
    env: {
      ...process.env,
      HOME,
      TABT_PROOF: PROOF,
      TABT_MARKER_CWD: MARKER_CWD,
    },
    // 'ignore', not 'pipe': a piped stdio keeps a handle referenced in this process's event
    // loop, so `launch` would print its pid and then hang forever instead of exiting. Nothing
    // reads the app's stdout anyway -- it logs nothing useful, and the assertions come back
    // through $PROOF files.
    stdio: 'ignore',
    detached: true,
  });
  child.unref();
  writeFileSync(PIDFILE, String(child.pid));
  return child;
}

// ---------------------------------------------------------------------------
// Reaching into a *running* instance. Everything below targets the pid in PIDFILE,
// never a process matched by name -- see the killOurs note above.
// ---------------------------------------------------------------------------
function runningPid() {
  const pid = Number(read(PIDFILE));
  if (!pid) throw new Error('no instance running (no pidfile) -- start one with: driver.mjs launch');
  try { process.kill(pid, 0); } catch {
    throw new Error(`pid ${pid} from the pidfile is gone -- relaunch with: driver.mjs launch`);
  }
  return pid;
}

const osa = (script) => execFileSync('osascript', ['-e', script], { encoding: 'utf8' }).trim();

/// The tabs' PTY slave devices, in spawn order. Each tab is one forked login zsh (pty.rs), so
/// the app's direct children *are* the tabs; ascending pid is the order they were created in,
/// which for a driver-launched instance is also their order in the sidebar.
function tabTtys(pid = runningPid()) {
  const kids = execFileSync('pgrep', ['-P', String(pid)], { encoding: 'utf8' })
    .trim().split('\n').filter(Boolean).map(Number).sort((a, b) => a - b);
  return kids
    .map((k) => {
      const tty = execFileSync('ps', ['-o', 'tty=', '-p', String(k)], { encoding: 'utf8' }).trim();
      return tty && tty !== '??' ? { pid: k, dev: `/dev/${tty}` } : null;
    })
    .filter(Boolean);
}

/// Interpret the escapes a shell would: \e \n \r \t \0 \\. Written straight to the PTY slave,
/// so the app sees them exactly as if the shell had printed them.
function unescape(s) {
  return s.replace(/\\(e|E|n|r|t|a|0|\\)/g, (_, c) =>
    ({ e: '\x1b', E: '\x1b', n: '\n', r: '\r', t: '\t', a: '\x07', 0: '\0', '\\': '\\' })[c]);
}

/// `which` is a 0-based index, or "last" / a negative index counting from the end.
///
/// "last" matters because `shot` photographs whichever tab is *visible*, and the driver has no
/// way to ask which that is -- the sidebar is self-drawn, so it exposes nothing to the
/// Accessibility API, and the app binds no tab-switch key equivalent for `keys` to press.
/// What is knowable: a fresh `launch` has exactly one tab, and `keys t cmd` makes the newest
/// tab the visible one. So "the visible tab" is reachable as long as you only ever address tab
/// 0 (fresh instance) or the last one (right after ⌘T).
function resolveTab(which = 0) {
  const tabs = tabTtys();
  const i = which === 'last' ? tabs.length - 1 : Number(which) < 0 ? tabs.length + Number(which) : Number(which);
  const tab = tabs[i];
  if (!tab) throw new Error(`no tab ${which} (this instance has ${tabs.length}: ${tabs.map((t) => t.dev).join(' ')})`);
  return tab;
}

function feed(bytes, which = 0) {
  const tab = resolveTab(which);
  writeFileSync(tab.dev, unescape(bytes));
  return tab.dev;
}

/// The window rectangle, straight out of the Accessibility API. Needs the Accessibility
/// permission; without it osascript throws and `doctor` says so.
function windowBounds(pid = runningPid()) {
  const out = osa(
    `tell application "System Events" to tell (first process whose unix id is ${pid}) ` +
    `to get {position, size} of window 1`);
  const n = out.split(',').map((v) => Number(v.trim()));
  if (n.length !== 4 || n.some(Number.isNaN)) throw new Error(`unparseable window bounds: ${out}`);
  return n; // [x, y, w, h] in points
}

function shot(file) {
  const pid = runningPid();
  const [x, y, w, h] = windowBounds(pid);
  const out = file ? resolve(file) : join(SHOTS, 'latest.png');
  mkdirSync(dirname(out), { recursive: true });
  // -x: no shutter sound. -R: crop to the window, so the shot is the app and not the desktop.
  execFileSync('screencapture', ['-x', `-R${x},${y},${w},${h}`, out]);
  return out;
}

/// Click a menu item by title, e.g. `menu('Settings…')`. Needs the Accessibility permission.
///
/// Not redundant with `keys`: a key equivalent only fires if AppKit routes the synthesized event
/// to the menu, and it does not always do so. ⌘T arrives, ⌘, does not -- `keys , cmd` leaves the
/// app untouched while this opens the Settings window. When a menu item has an action, prefer this.
function menu(title, menuTitle = null) {
  const pid = runningPid();
  osa(`tell application "System Events" to set frontmost of (first process whose unix id is ${pid}) to true`);
  osa('delay 0.3');
  const where = menuTitle
    ? `menu 1 of menu bar item "${menuTitle}" of menu bar 1`
    // menu bar item 1 is the Apple menu, so the app's own menu is 2.
    : 'menu 1 of menu bar item 2 of menu bar 1';
  osa(`tell application "System Events" to tell (first process whose unix id is ${pid}) ` +
      `to click menu item "${title}" of ${where}`);
}

/// The named windows of the instance, so a shot can target something other than the terminal.
function windowNames(pid = runningPid()) {
  return osa(`tell application "System Events" to tell (first process whose unix id is ${pid}) ` +
             `to get name of every window`).split(',').map((s) => s.trim()).filter(Boolean);
}

/// Raise the instance and send one keystroke through System Events. `mods` is any of
/// cmd/shift/opt/ctrl, comma- or space-separated. Needs the Accessibility permission.
function keys(key, mods = '') {
  const pid = runningPid();
  const list = mods.split(/[ ,]+/).filter(Boolean).map((m) =>
    ({ cmd: 'command down', command: 'command down', shift: 'shift down', opt: 'option down',
       option: 'option down', alt: 'option down', ctrl: 'control down', control: 'control down' })[m]
    ?? (() => { throw new Error(`unknown modifier: ${m}`); })());
  const using = list.length ? ` using {${list.join(', ')}}` : '';
  osa(`tell application "System Events" to set frontmost of (first process whose unix id is ${pid}) to true`);
  osa('delay 0.3');
  osa(`tell application "System Events" to keystroke "${key}"${using}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(file, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(file)) return true;
    await sleep(150);
  }
  return false;
}

const read = (f) => (existsSync(f) ? readFileSync(f, 'utf8').trim() : '');

// Terminal replies are raw bytes; show ESC as "ESC" so they are readable and assertable.
const readReply = (f) => read(f).replace(/\x1b/g, 'ESC');

async function smoke() {
  const results = [];
  const check = (name, ok, detail) => {
    results.push({ name, ok, detail });
    log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  -- ${detail}` : ''}`);
  };

  log(`repo:   ${REPO}`);
  log(`app:    ${APP_BIN}`);
  log(`HOME:   ${HOME}  (isolated; your real ~/${BUILD.configDir} is untouched)`);
  log('');

  const child = launch();
  log(`launched pid ${child.pid}`);
  try {
    const ready = await waitFor(join(PROOF, 'ready.txt'), 20000);
    check('fixture shell ran inside a tab', ready, ready ? '' : 'no proof/ready.txt after 20s');

    const boot = read(join(PROOF, 'boot.txt'));
    check('login shell + TERM wiring', /term=xterm-256color/.test(boot) && /term_program=TabT/.test(boot), boot);

    const plain = readReply(join(PROOF, 'dsr_plain.txt'));
    check('DSR round-trip through the live app', /^ESC\[\d+;\d+R$/.test(plain), JSON.stringify(plain));

    const cup = readReply(join(PROOF, 'dsr_after_cup.txt'));
    check('parser honoured CUP (expect ESC[10;5R)', cup === 'ESC[10;5R', JSON.stringify(cup));

    const da = readReply(join(PROOF, 'da.txt'));
    check('device attributes reply', /^ESC\[\?\d/.test(da), JSON.stringify(da));

    // The app persists layout (incl. OSC 7 cwd) to $HOME/<config dir>/layout.conf.
    const confExists = await waitFor(CONF, 3000);
    check('wrote layout.conf under the scratch HOME', confExists, CONF);

    const alive = (() => { try { process.kill(child.pid, 0); return true; } catch { return false; } })();
    check('app still running (no crash)', alive);
  } finally {
    killOurs(child.pid);
    await sleep(400);
    log(`stopped pid ${child.pid}\n`);
  }

  // -------------------------------------------------------------------------
  // Phase 2: restore. Seed layout.conf with a cwd and relaunch -- the shell must
  // come up *in that directory*. Exercises config parse -> tab restore -> PTY
  // spawn-in-cwd. Seeding by hand rather than by quitting a live instance keeps this
  // phase independent of how the previous one was stopped: AppController::save() runs
  // on every layout mutation, but the *final* save is applicationWillTerminate: ->
  // persist(), which a SIGTERM skips (`quit --graceful` is the one that runs it).
  // -------------------------------------------------------------------------
  mkdirSync(MARKER_CWD, { recursive: true });
  rmSync(join(PROOF, 'boot.txt'), { force: true });
  rmSync(join(PROOF, 'ready.txt'), { force: true });
  // Phase 1 only reaches here if the app already created the directory, but `launch({fresh:true})`
  // wipes $HOME, so seed it unconditionally rather than depending on that ordering.
  mkdirSync(dirname(CONF), { recursive: true });
  writeFileSync(CONF,
    `[settings]\nstyle = Default\n\n[tabs]\ntab = Restored\ncwd = ${MARKER_CWD}\n`);

  const child2 = launch({ fresh: false });
  log(`relaunched pid ${child2.pid} against a seeded layout`);
  try {
    const ready2 = await waitFor(join(PROOF, 'ready.txt'), 20000);
    check('restored session started', ready2);
    const boot2 = read(join(PROOF, 'boot.txt'));
    const want = realpathSync(MARKER_CWD);
    const got = (boot2.match(/cwd=(.*)$/) || [])[1] || '';
    check('shell spawned in the restored cwd', got === want, `${got || '(none)'} vs ${want}`);
  } finally {
    killOurs(child2.pid);
    await sleep(400);
    log(`stopped pid ${child2.pid}`);
  }

  const failed = results.filter((r) => !r.ok);
  log(`\n${results.length - failed.length}/${results.length} checks passed`);
  return failed.length === 0 ? 0 : 1;
}

// ---------------------------------------------------------------------------
// Scenes: canned pages of escape sequences that put a lot of the parser on screen at
// once, so one screenshot is a visual regression test of `Grid::feed` + the renderer.
// ---------------------------------------------------------------------------
const SCENES = {
  // Each block is labelled on screen, so a diff in the screenshot points at a feature.
  vt: [
    '\x1b[2J\x1b[H',                                  // clear + home
    '\x1b[1mTabT VT scene\x1b[0m\r\n\r\n',
    'SGR:      ',
    ...[31, 32, 33, 34, 35, 36].map((c) => `\x1b[${c}m##\x1b[0m`),
    ' \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[7minverse\x1b[0m\r\n\r\n',
    'DEC gfx:  \x1b(0lqqqk\x1b(B\r\n',
    '          \x1b(0x   x\x1b(B\r\n',
    '          \x1b(0mqqqj\x1b(B\r\n\r\n',
    // Wide glyphs, then IRM inserting *into* a wide pair -- the case that used to leave an
    // orphan half behind. The label is 10 columns, so 中 occupies columns 11-12 (1-based) and
    // CHA to 12 parks the cursor on its *trailing* half: exactly the split that has to blank
    // both halves. Expect "IRM+wide:  Z x" -- one space where 中 was, no doubled or clipped
    // glyph. Aiming at column 13 instead would land on the `x` and prove nothing.
    'wide:     中文 CJK ok\r\n',
    'IRM+wide: 中x\x1b[4h\x1b[12GZ\x1b[4l\r\n\r\n',
    // Custom tab stops: clear the power-on set, place one at column 30, then walk to it.
    '\x1b[3g\x1b[31G\x1bH\x1b[1Gtabs:\tstop@30\r\n',
  ].join(''),
};

function scene(name, file, which = 'last') {
  const body = SCENES[name];
  if (!body) throw new Error(`unknown scene "${name}" (have: ${Object.keys(SCENES).join(', ')})`);
  // Defaults to the last tab, not tab 0: that is the one ⌘T leaves visible, and a shot of a
  // tab that is not on screen is a photo of some other tab's contents.
  const dev = feed(body, which);
  return { dev, out: shot(file) };
}

function doctor() {
  let ok = true;
  const line = (name, good, detail) => {
    log(`${good ? 'OK  ' : 'FAIL'}  ${name}${detail ? `  -- ${detail}` : ''}`);
    if (!good) ok = false;
  };
  line('app bundle built', existsSync(APP_BIN), APP_BIN);
  let pid = 0;
  try { pid = runningPid(); line('instance running', true, `pid ${pid}`); }
  catch (e) { line('instance running', false, e.message); return ok ? 0 : 1; }

  try {
    const b = windowBounds(pid);
    line('Accessibility (window bounds, needed by shot/keys)', true, `x,y,w,h = ${b.join(',')}`);
  } catch (e) {
    line('Accessibility (window bounds, needed by shot/keys)', false,
      `${String(e.message).split('\n')[0]} -- grant Accessibility to the app running this driver`);
  }
  try {
    const out = shot(join(SHOTS, 'doctor.png'));
    const bytes = readFileSync(out).length;
    // A capture without Screen Recording still succeeds but comes back as desktop wallpaper
    // only, which compresses far smaller than a window full of text.
    line('Screen Recording (screencapture)', bytes > 20000, `${out} (${bytes} bytes) -- open it to confirm it shows the app`);
  } catch (e) {
    line('Screen Recording (screencapture)', false, String(e.message).split('\n')[0]);
  }
  try {
    const tabs = tabTtys(pid);
    line('PTY feed channel', tabs.length > 0, tabs.map((t) => t.dev).join(' '));
  } catch (e) {
    line('PTY feed channel', false, String(e.message).split('\n')[0]);
  }
  return ok ? 0 : 1;
}

const cmd = process.argv[2] || 'smoke';
const arg = (i) => process.argv[3 + i];
if (cmd === 'smoke') {
  process.exit(await smoke());
} else if (cmd === 'launch') {
  // --keep preserves the scratch $HOME instead of rebuilding it, which is the only way to launch
  // against a layout.conf you edited by hand (a theme, a seeded cwd): a plain launch wipes it.
  const c = launch({ fresh: arg(0) !== '--keep' });
  log(`pid ${c.pid}  HOME=${HOME}${arg(0) === '--keep' ? '  (kept)' : ''}`);
  log(`quit with: node ${process.argv[1]} quit`);
} else if (cmd === 'quit') {
  const pid = Number(read(PIDFILE));
  if (!pid) { log('no pidfile'); }
  else if (arg(0) === '--graceful') {
    // Runs applicationWillTerminate: -> persist(), i.e. one last save of titles/cwds/geometry.
    // Beware: ⌘Q's confirm dialog (confirm_quit) can block this if a tab has a foreground job.
    osa(`tell application id "${BUILD.bundleId}" to quit`);
    log(`asked pid ${pid} to quit (graceful)`);
  } else { killOurs(pid); log(`stopped pid ${pid}`); }
} else if (cmd === 'feed') {
  log(`wrote ${arg(0)?.length ?? 0} chars to ${feed(arg(0) ?? '', arg(1) ?? 0)}`);
} else if (cmd === 'shot') {
  log(shot(arg(0)));
} else if (cmd === 'menu') {
  menu(arg(0), arg(1));
  log(`clicked menu item "${arg(0)}"`);
} else if (cmd === 'windows') {
  log(windowNames().join('\n'));
} else if (cmd === 'keys') {
  keys(arg(0), process.argv.slice(4).join(' '));
  log(`sent ${[process.argv.slice(4).join('+'), arg(0)].filter(Boolean).join('+')}`);
} else if (cmd === 'scene') {
  const { dev, out } = scene(arg(0) ?? 'vt', arg(1), arg(2) ?? 'last');
  log(`rendered scene "${arg(0) ?? 'vt'}" into ${dev}\n${out}`);
} else if (cmd === 'tty') {
  const tabs = tabTtys();
  const i = arg(0) === undefined ? null : Number(arg(0));
  log(i === null ? tabs.map((t, n) => `${n}  ${t.dev}  (shell pid ${t.pid})`).join('\n') : tabs[i]?.dev ?? '');
} else if (cmd === 'doctor') {
  process.exit(doctor());
} else {
  log('usage: driver.mjs [smoke|launch|quit|feed <bytes> [tab|last]|shot [file]|keys <key> [mods]|menu <item> [menu]|windows|scene <name> [file] [tab]|tty [n]|doctor]');
  process.exit(2);
}

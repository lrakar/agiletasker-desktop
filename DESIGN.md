# AgileTasker Desktop Shell — Design

## What this is

A thin, tray-resident Tauri 2 wrapper around `https://agiletasker.com`. The
one thing it does that a bookmark can't: it keeps the user's
`workspace-host`/`agent-bridge` Node daemons running for as long as the
machine is on, supervised natively instead of living in a terminal window
the user has to remember to keep open. Closing the window hides to tray;
only the tray's own "Quit" — after gracefully stopping every daemon —
actually exits the process.

## IPC contract (verbatim from the brief this was built against)

Commands (snake_case Rust fn names; Tauri auto-generates `allow-<kebab
case>` ACL permissions per command — see `build.rs`'s `AppManifest` and
"Capabilities" below). Argument/return JSON is camelCase via
`serde(rename_all = "camelCase")`.

- `desktop_info()` → `{ appVersion: string, platform: "windows"|"macos"|"linux", arch: string, autostart: boolean }`
- `set_autostart(enabled: boolean)` → `boolean` (resulting state)
- `pair_daemon(config: { kind: "host"|"agent", uid: string, key: string, cwd: string|null })` → `DaemonStatus`. Validates uid/key regex per kind, stores the key in the OS keychain (`keyring` crate, service `"AgileTasker Desktop"`, account = uid), persists config (WITHOUT the key) to `<app_data_dir>/daemons.json`, downloads the daemon bundle, spawns + supervises. Re-pairing an existing uid replaces its key and restarts it. First successful pair (from zero paired daemons) also enables autostart.
- `unpair_daemon(id: string)` → `null`. Stops the process, removes the config, deletes the keychain entry.
- `list_daemons()` → `DaemonStatus[]`
- `restart_daemon(id: string)` → `DaemonStatus` (re-downloads the bundle — this is how daemon updates roll out)
- `stop_daemon(id: string)` → `DaemonStatus` (desired = stopped, persisted)
- `start_daemon(id: string)` → `DaemonStatus` (desired = running)
- `daemon_log_tail(id: string, lines: number|null)` → `string` (in-memory ring buffer, default 200, cap 1000 lines per daemon)
- `pick_directory()` → `string|null` (native folder picker, for an agent's `cwd`)
- `check_for_shell_update()` → `{ available: boolean, version: string|null }`

Event: `daemon-status` (payload = `DaemonStatus`) on every state transition,
plus a full re-emit of every daemon's status after pair/unpair.

`DaemonStatus`: `{ id, kind, uid, state, pid, lastExitCode, message, startedAt }`
where `state ∈ "starting"|"running"|"backoff"|"auth-failed"|"conflict"|"stopped"`,
`id == uid`, `message` is a short human-readable detail, `startedAt` is
epoch ms or `null`.

Supervision policy: exponential backoff 1s→2s→4s→…cap 60s
(`daemon::backoff::delay_for_attempt`), attempt counter resets after 60s of
clean uptime. stderr classified as an auth failure → state `auth-failed`,
NO auto-restart. Twin-conflict → state `conflict`, retried every 120s.
Normal exit/crash → backoff restart.

## Capabilities — how remote-origin IPC access actually works (verified)

`src-tauri/capabilities/remote.json`:

```json
{
  "$schema": "../gen/schemas/remote-schema.json",
  "identifier": "remote-agiletasker",
  "windows": ["main"],
  "remote": { "urls": ["https://agiletasker.com"] },
  "permissions": [
    "core:event:allow-listen",
    "core:event:allow-unlisten",
    "allow-desktop-info",
    "allow-set-autostart",
    "allow-pair-daemon",
    "allow-unpair-daemon",
    "allow-list-daemons",
    "allow-restart-daemon",
    "allow-stop-daemon",
    "allow-start-daemon",
    "allow-daemon-log-tail",
    "allow-pick-directory",
    "allow-check-for-shell-update"
  ]
}
```

Two mechanisms had to be verified against the actual toolchain rather than
guessed (the community docs disagreed with the compiler on both — this is
what's actually true for tauri 2.11.5 / tauri-build 2.6.3):

1. **`remote` + `windows`** is the standard way to grant a capability's
   permissions to a window while it's displaying a non-`tauri://localhost`
   origin — `windows: ["main"]` names the window (built in `lib.rs`'s
   `setup()`, label `"main"`), `remote.urls` allowlists the origin(s) that
   capability applies to. Without this, a window navigated to a remote
   `https://` URL gets **no** IPC access at all by default (Tauri's remote
   security model).
2. **App-defined (non-plugin) commands are NOT auto-registered as ACL
   permissions.** Plugin commands are (each plugin ships its own
   `permissions/*.toml`, which is why e.g. `dialog:allow-open` resolves).
   For this crate's own `#[tauri::command]` functions, `cargo check`
   initially rejected `allow-desktop-info` with "Permission
   allow-desktop-info not found" and printed the *entire* list of known
   permissions — every one of them namespaced to a plugin (`core:*`,
   `dialog:*`, `shell:*`, ...), none of them ours. The fix is
   `build.rs`'s `tauri_build::Attributes::new().app_manifest(AppManifest::new().commands(&[...]))`
   — passing the **snake_case Rust function names** — which makes
   tauri-build autogenerate `allow-<kebab-case>`/`deny-<kebab-case>` ACL
   identifiers for exactly those commands (confirmed by the same
   compiler error going from "not found" to succeeding once this was
   wired in — the identifier itself must be lowercase-ASCII-and-hyphens
   only, no underscores, which is why the capability file uses
   `allow-desktop-info`, not `allow-desktop_info`, despite the Rust
   function being `desktop_info`).

Deliberately excluded: any raw plugin permission beyond what the commands
above wrap internally (no `dialog:allow-open`, no `shell:allow-spawn`,
etc., on the remote origin — `pick_directory` and daemon spawning happen
in Rust; the web page only ever gets the specific command surface). `core`
access is the narrow `core:event:allow-listen`/`allow-unlisten` pair (for
subscribing to `daemon-status`), not `core:default`.

No local capability was added for the `ui/` stub page — nothing loads it
today (see "Wave 2" below), so it needs no IPC access; `frontendDist`
merely has to point at *something* valid for the build to succeed.

## Lifecycle model

### Window / tray
- The main window is built **programmatically** in `lib.rs`'s `setup()`
  (`WebviewWindowBuilder`, not `tauri.conf.json`'s declarative `windows`
  array) specifically because `on_navigation`/`on_new_window` are only
  available on the builder, not expressible in JSON config.
- `on_navigation` allowlists `agiletasker.com`, `accounts.google.com`, and
  `*.googleusercontent.com` (see `navigation_allowed` in `lib.rs`) —
  everything else is redirected to the system browser via the opener
  plugin and the in-webview navigation is cancelled.
- `on_new_window` (i.e. `target=_blank`/`window.open`) is **unconditionally**
  denied and opened externally instead — this app only ever shows the one
  main window, even for a link that would itself be same-origin-allowed.
- Closing the window (`WindowEvent::CloseRequested`) calls
  `api.prevent_close()` and hides instead. The tray's "Quit AgileTasker
  (stops agents)" is the *only* path that calls `app.exit()`.
- `--hidden` (the arg `tauri_plugin_autostart` is configured to launch
  with) skips showing the window at startup and — on macOS — sets
  `ActivationPolicy::Accessory` so no Dock icon bounces for a window that
  was never going to appear.

### Daemon supervisor (`daemon::manager`)
One Tokio task per paired daemon (`run_supervisor`), each an actor loop
addressed via an `mpsc` command channel (`Start`/`Stop`/`Restart`/`Shutdown`).
Every state transition updates a shared `Arc<Mutex<DaemonStatus>>` and
emits `daemon-status`.

**Stop sequence** (per an orchestrator patch mid-build — see git history of
`scripts/agent/workspace-host.mjs` / `agent-daemon.mjs`): both daemon
sources now treat **stdin EOF** as a graceful-shutdown request when
launched with `AGILETASKER_MANAGED=1` (always set on every spawn below) —
they `resume()` stdin and call their own `shutdown()` (Firestore offline
stamp, `exit(0)`) on `'end'`/`'close'`. `tauri-plugin-shell`'s
`CommandChild` has exactly three public methods — `write`, `kill(self)`,
`pid` (verified against the plugin's actual source, 2.3.5) — with **no**
"close stdin without killing" method. So `graceful_stop` gets the same
effect the only way the API allows: capture the pid, then **drop** the
whole `CommandChild`. Dropping closes its private `stdin_writer` pipe
(the child sees EOF) and releases the wrapped
`Arc<shared_child::SharedChild>` handle without killing the process
(matching `std::process::Child`'s documented "drop does not kill" contract
that `shared_child` wraps unmodified — see the code comment in
`manager.rs` for the one residual honesty note: this specific
non-killing-on-drop behavior wasn't independently re-verified against
`shared_child`'s own source, only inferred from it being an undocumented,
unoverridden thin wrapper; the design is safe either way — see that
comment for why). The paired `CommandEvent` receiver is a *separate*
object from `CommandChild`, so it keeps delivering the eventual
`Terminated` event after the drop. If the child hasn't exited within 5s
(e.g. a still-live **production** bundle predating the stdin-close patch —
`agiletasker.com/agent/*.mjs` won't have it until the next web deploy),
`hard_kill` escalates via the platform's own `taskkill /F` (Windows) /
`kill -TERM` (unix) utility, addressed purely by pid.

**Bundle acquisition** (`daemon::bundles`): every (re)spawn tries a fresh
download first (`{AGILETASKER_BUNDLE_BASE}/<file>.mjs`, default base
`https://agiletasker.com/agent`, 15s timeout, rustls TLS via `reqwest`),
falling back to whatever's cached in `<app_data_dir>/daemons/` if the
download fails; only errors if neither exists. This is how `restart_daemon`
rolls out daemon-side updates. The host daemon additionally needs
`node_modules/{node-pty,werift}` physically next to it — copied from the
app's bundled `resources/daemon-deps/node_modules` into `app_data` on
first run or whenever the app version changes (a stamp file records which
version last synced it).

**Spawn**: `tauri_plugin_shell` sidecar `node`, args
`[bundlePath, --pair, "<uid>:<key>", (--cwd <dir> for agent kind)]`, env
`AGILETASKER_MANAGED=1`. The key is read fresh from the OS keychain on
every spawn attempt — never cached in the supervisor's own memory beyond
the single `spawn()` call that needs it.

**Exit classification** (`daemon::classify`) — see that module's doc
comment for the exact stderr substrings extracted from
`scripts/agent/workspace-host.mjs` / `agent-daemon.mjs` (read 2026-08-06):
- auth failure ⇔ stderr contains `"Pairing failed"`
- twin conflict ⇔ stderr contains `"refusing to start a twin"`
- everything else → plain crash → exponential backoff

## Login-shell PATH (macOS)

`login_env::fix_macos_login_path()` runs once, first thing in `setup()`.
GUI-launched macOS apps (Finder, Spotlight, and especially a LaunchAgent —
autostart's own mechanism) inherit `launchd`'s minimal PATH, not the PATH
an interactive Terminal session builds by sourcing `.zshrc`/`.zprofile` —
the only place Homebrew/nvm/asdf shims usually land. The agent bridge
shells out to `claude`/`codex` by bare name, so a minimal PATH here
propagates all the way down (this app → the Node sidecar → the daemon
child → its own `spawn('claude', ...)`) as a silent "command not found"
that never reproduces from a Terminal test. The fix: ask the user's own
`$SHELL -ilc 'echo -n "$PATH"'` what PATH it would build, adopt that for
the rest of this process's lifetime (every later spawn inherits it — shell
plugin `Command`s inherit the parent env by default). No-op on
Windows/Linux.

## Dev knobs

- `AGILETASKER_BUNDLE_BASE` (default `https://agiletasker.com/agent`) —
  overrides the daemon bundle download base. **Dev-only**: lets
  integration testing point at a locally served copy of a freshly built
  `.mjs` bundle before it ships in a real web deploy.

## Security notes

- The pairing **key** never touches disk in plaintext: `pair_daemon` writes
  it straight to the OS keychain (`keyring` crate, Windows Credential
  Manager / macOS Keychain / Linux Secret Service backends all compiled
  in — see `Cargo.toml`'s `keyring` feature list, named explicitly rather
  than relied on as per-platform defaults so every CI leg links the right
  backend) and `daemons.json` only ever stores `{ id, kind, uid, cwd,
  desired }`.
- The remote capability (`capabilities/remote.json`) is scoped to exactly
  `https://agiletasker.com` and exactly the command surface above — see
  "Capabilities" for what was deliberately excluded.
- Files this app creates under `app_data` (downloaded bundles, copied
  `node_modules`) get **no macOS quarantine attribute** — deliberate:
  quarantine is for files that crossed a network trust boundary *into* the
  app from outside (a browser download, an email attachment); these are
  written by the app's own process to its own data directory, which macOS
  already doesn't quarantine by default for that reason. Documented here
  rather than fought with extra code.
- `on_navigation`'s `*.googleusercontent.com` allowance was added per the
  product brief's explicit instruction (Google's OAuth
  consent/account-chooser flow, and where profile photo assets live) but
  was **not** independently re-traced against a live `signInWithRedirect`
  session — doing so needs an interactive OAuth round trip this task
  didn't have credentials to drive. Low risk (read-only navigation
  allowlist, not a permission grant), but worth a real trace before
  hardening this further.
- `shared_child::SharedChild`'s drop-doesn't-kill behavior (see "Stop
  sequence" above) was inferred, not independently confirmed by reading
  that crate's own source — the design is correct either way (see the
  code comment), but it's the one place this task leaned on inference
  over verification for something safety-adjacent.

## Deviations from the brief, with reasons

- **`tauri-plugin-deep-link` was not added**, despite being pinned in the
  brief's verified-versions list. Nothing in the feature spec calls for a
  custom URI scheme — Google OAuth is handled entirely via the
  `on_navigation` allowlist (an in-webview redirect chain), not a deep-link
  callback. Adding an unused plugin would just be extra attack surface and
  an extra capability to reason about for no behavior. Candidate for a
  future "sign in with a real native OAuth flow" wave.
- **`bundle.windows.nsis.webviewInstallMode` (as written in the brief)
  doesn't exist** in tauri-utils 2.9.3's actual `NsisConfig` — the field
  lives one level up, on `WindowsConfig` (`bundle.windows.webviewInstallMode`),
  sibling to `nsis`, not nested inside it. `tauri.conf.json` reflects the
  real schema; the *value* (`downloadBootstrapper`, `silent: true`) is
  exactly what was asked for — it's also the crate's own default, so this
  could be omitted entirely, but it's kept explicit per the brief's intent.
- **`bundle.targets` is the explicit list `["nsis", "dmg"]`**, not `"all"`
  with a Windows-only override file. This satisfies "restrict Windows
  bundling to NSIS" directly (no `"msi"` in the list at all — the WiX
  toolchain is never invoked) while keeping `dmg` available for the macOS
  CI leg from this same `tauri.conf.json`, with no platform-specific
  config-file split needed.
- **Capability permission identifiers are kebab-case**
  (`allow-desktop-info`), not the brief's literal snake_case command name
  with an `allow-` prefix — see "Capabilities" above for why (ACL
  identifier syntax forbids underscores; this was discovered from the
  compiler, not the docs).
- **The stop sequence is stdin-close → 5s grace → hard-kill**, replacing
  the brief's original "SIGTERM on unix / kill on Windows, 5s grace, then
  hard kill" — per an orchestrator mid-task patch note (both daemon
  sources gained stdin-EOF-triggers-graceful-shutdown support,
  env-gated on `AGILETASKER_MANAGED=1`). See "Lifecycle model" above.

## Wave 2 (explicitly out of scope for this task)

- **Automatic offline fallback.** `ui/index.html` exists so
  `build.frontendDist` points at something valid, but nothing wires it up
  automatically today — the main window always navigates straight to
  `https://agiletasker.com`. A real implementation would need to detect a
  failed initial load (e.g. `on_navigation`/webview load-failure events)
  and fall back to the local page, then retry.
- **A native "Add computer"/"Add agent" pairing UI in the shell itself.**
  Today `pair_daemon` is purely IPC — the *web app* (built in parallel
  against this contract) is the only UI. A native fallback (useful if the
  web app can't reach the user, e.g. mid-outage) is future work.
- **Real code-signing.** `signingIdentity: "-"` (ad-hoc) on macOS and no
  Windows Authenticode signing at all — fine for local/dev builds and even
  for an initial unsigned NSIS release (Windows SmartScreen will warn, but
  it installs), not fine for a polished GA release.
- **A real native OAuth deep-link flow**, if `signInWithRedirect` through
  the in-webview allowlist chain ever proves fragile — `tauri-plugin-deep-link`
  is the obvious next tool (see "Deviations" above for why it wasn't
  added now).

## Open risks for the CI agent (building the macOS legs from this source)

- **Keychain backend features**: `Cargo.toml` explicitly lists
  `windows-native`, `apple-native`, `linux-native` for the `keyring` crate
  — verified compiling on Windows here; the macOS (`apple-native`) and
  Linux (`linux-native`) backends were never actually exercised on their
  target OS by this task. If `apple-native` needs an entitlement
  (Keychain access group) that a plain ad-hoc-signed build doesn't get by
  default, `keyring::Entry::set_password`/`get_password` could fail at
  *runtime* even though it compiles — worth an early smoke test of
  `pair_daemon` on real macOS hardware, not just `cargo check`.
- **`shared_child` drop semantics** (see "Security notes") — inferred, not
  read from source. If it turns out dropping *does* send a kill signal on
  some platform, the practical effect is a courser stop (immediate kill
  instead of graceful-then-escalate) rather than a hang or crash, but it's
  worth a real "does stdin-close actually work end-to-end against a
  patched daemon" test on macOS specifically, since that's the platform
  the login-PATH fix and the LaunchAgent autostart mechanism both matter
  most for.
- **`on_new_window`/`on_navigation` platform coverage**: `docs.rs` notes
  `on_new_window` is "Not supported" on Android/iOS, which is irrelevant
  here (desktop-only app), but its actual behavior wasn't hand-tested on
  macOS/Linux webviews (WebKitGTK vs. WKWebView vs. WebView2 can differ in
  edge cases like a `window.open()` with no `target=_blank`) — worth a
  manual click-through of a real `target=_blank` link and a real
  Google-OAuth redirect on each platform once built.
- **NSIS `webviewInstallMode: downloadBootstrapper`** needs network access
  during *installation* on a machine without WebView2 already present
  (stock on Windows 11, not guaranteed on older Windows 10 builds) — not a
  code risk, just a real-world dependency worth knowing about for support.
- **The updater endpoint** (`https://github.com/lrakar/agiletasker-desktop/releases/latest/download/latest.json`)
  was wired into config exactly as specified but **never network-validated**
  — that GitHub repo doesn't exist yet (pending a permission grant). The
  Rust code around it (`tray::check_for_updates_interactive`/`_background`)
  compiles and is structurally correct, but the first real end-to-end
  "does a `latest.json` response actually get parsed and installed"
  check has to happen once that repo exists and has a real release.

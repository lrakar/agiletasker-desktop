# AgileTasker Desktop

The desktop app for [agiletasker.com](https://agiletasker.com) — Windows and macOS.
It runs the full AgileTasker web app plus what a browser can't do: your AI agents
(Claude/Codex workspace terminals, Messenger agents) keep working in the background
on your computer. Closes to tray, starts on login, one-click pairing.

## Download

- **Windows (x64):** [AgileTasker-Setup.exe](https://github.com/lrakar/agiletasker-desktop/releases/latest/download/AgileTasker-Setup.exe)
- **macOS (Apple Silicon):** [AgileTasker-AppleSilicon.dmg](https://github.com/lrakar/agiletasker-desktop/releases/latest/download/AgileTasker-AppleSilicon.dmg)
- **macOS (Intel):** [AgileTasker-Intel.dmg](https://github.com/lrakar/agiletasker-desktop/releases/latest/download/AgileTasker-Intel.dmg)

### First launch

- **Windows:** SmartScreen may show "Windows protected your PC" — click **More info → Run anyway**.
- **macOS:** the app is not yet notarized. Drag it to Applications, double-click once
  (it will be blocked), then **System Settings → Privacy & Security → Open Anyway**.
  Terminal alternative: `xattr -cr /Applications/AgileTasker.app`

## About this repository

This is a **build & release mirror** — the source of truth lives in the main
(private) AgileTasker repository and is synced here by script. Issues and PRs
here are not monitored; feedback goes through the app.

## Build from source

```
npm install
node scripts/prepare-deps.mjs   # fetches the Node sidecar runtime + daemon native deps
npx tauri build
```

Synced from main repo commit `2f85ddd`.

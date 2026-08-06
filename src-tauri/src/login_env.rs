//! macOS GUI-launch PATH fix.
//!
//! Apps launched from Finder, Spotlight, or (worse) a LaunchAgent
//! (autostart's own launch mechanism — see `tauri_plugin_autostart` init in
//! `lib.rs`) inherit `launchd`'s minimal PATH, not the PATH an interactive
//! Terminal session builds by sourcing `.zshrc`/`.zprofile` — the ONLY place
//! Homebrew, nvm, or asdf shims usually get added. The agent bridge daemon
//! shells out to `claude`/`codex` by bare name (see `agent-daemon.mjs`'s
//! header comment, which calls this out explicitly), so a minimal PATH here
//! propagates all the way down: this app's PATH → the Node sidecar's
//! inherited PATH → the daemon child process's inherited PATH → its
//! `spawn('claude', ...)` failing with a silent "command not found" that
//! never reaches a user who only ever tested from Terminal.
//!
//! Fixed once, at startup, by asking the user's own login shell what PATH
//! it would build and adopting that for the rest of this process's
//! lifetime — every sidecar/child spawned afterward inherits it normally,
//! since `tauri_plugin_shell`'s `Command` inherits the parent's environment
//! by default. Windows and Linux desktop launches don't share this problem
//! (Windows has no login-shell-vs-GUI-launch PATH split; Linux `.desktop`
//! entries and terminal launches both typically already carry a full user
//! PATH), so this is a deliberate no-op there.

#[cfg(target_os = "macos")]
pub fn fix_macos_login_path() {
    use std::process::Command;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    // -i (interactive) sources .zshrc/.bashrc, where nvm and most manual
    // PATH exports live; -l (login) additionally sources .zprofile/.profile,
    // which is where Homebrew's own post-install instructions put its PATH
    // line. -c runs one command non-interactively despite -i.
    let result = Command::new(&shell).args(["-ilc", "echo -n \"$PATH\""]).output();
    match result {
        Ok(out) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if path.is_empty() {
                log::warn!("{shell} -ilc reported an empty PATH — keeping the GUI-launch PATH");
                return;
            }
            log::info!("adopting login-shell PATH from {shell} -ilc ({} bytes)", path.len());
            // SAFETY: called once, synchronously, at the very start of
            // `run()` before any thread has been spawned (no other Tauri
            // plugin, daemon supervisor, or async task is running yet to
            // race a concurrent env read/write against).
            unsafe { std::env::set_var("PATH", path) };
        }
        Ok(out) => {
            log::warn!(
                "{shell} -ilc exited with {:?} while resolving login PATH — keeping the GUI-launch PATH",
                out.status.code()
            );
        }
        Err(e) => {
            log::warn!("could not run {shell} -ilc to resolve the login PATH ({e}) — keeping the GUI-launch PATH");
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn fix_macos_login_path() {
    // No-op on Windows/Linux — see module docs.
}

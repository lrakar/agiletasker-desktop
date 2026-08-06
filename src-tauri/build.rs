// Standard Tauri 2 build script, plus one non-default piece: `app_manifest`
// tells tauri-build about this crate's OWN #[tauri::command] functions (as
// opposed to a plugin's) so it autogenerates `allow-<command>`/
// `deny-<command>` ACL permission identifiers for them — without this,
// `capabilities/remote.json` listing e.g. `allow-pair-daemon` fails the
// build with "Permission allow-pair-daemon not found" (verified: cargo
// check's own error output enumerates every KNOWN permission when one
// doesn't resolve, and a fresh app's own commands are absent from that list
// until they're declared here). This also generates the ACL/capabilities
// schemas (under gen/schemas, gitignored — see repo-root .gitignore) and
// wires tauri.conf.json into the compiled binary's embedded context.
fn main() {
    load_oauth_env();

    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "desktop_info",
                "set_autostart",
                "set_close_behavior",
                "pair_daemon",
                "unpair_daemon",
                "list_daemons",
                "restart_daemon",
                "stop_daemon",
                "start_daemon",
                "daemon_log_tail",
                "pick_directory",
                "check_for_shell_update",
                "google_sign_in",
            ]),
        ),
    )
    .expect("tauri_build::try_build failed");
}

/// Makes the desktop Google OAuth client ID/secret visible to
/// `option_env!("GOOGLE_OAUTH_CLIENT_ID"/"_SECRET")` in `src/oauth.rs` at
/// compile time, without either ever landing in source control.
///
/// Local dev: reads `desktop/.env.oauth` (gitignored, `KEY=VALUE` per line
/// — see that file's own header comment) and re-exports both vars via
/// `cargo:rustc-env`, which cargo forwards to the rustc invocation that
/// compiles this crate — so a plain `cargo build`/`tauri dev` "just works"
/// once a developer has dropped that file in place.
///
/// CI: `desktop/.env.oauth` doesn't exist in the checked-out repo (it's
/// gitignored), so this function reads nothing and emits nothing — but
/// `option_env!` still sees the values, because rustc (spawned by cargo)
/// inherits the ordinary process environment it was run in, and
/// `release.yml`'s `tauri-action` step sets `GOOGLE_OAUTH_CLIENT_ID`/
/// `GOOGLE_OAUTH_CLIENT_SECRET` there directly from the
/// `GOOGLE_OAUTH_CLIENT_ID`/`GOOGLE_OAUTH_CLIENT_SECRET` repo secrets. No
/// extra plumbing needed on that path.
///
/// If NEITHER source has a value, this is silently a no-op: `option_env!`
/// then evaluates to `None` at compile time, and `oauth::google_sign_in`
/// turns that into a clean runtime error ("desktop Google sign-in isn't
/// configured in this build") rather than failing the build — a checkout
/// without the credential (e.g. building the public mirror repo) must
/// still compile.
fn load_oauth_env() {
    let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.env.oauth");
    println!("cargo:rerun-if-changed={}", env_path.display());

    let Ok(contents) = std::fs::read_to_string(&env_path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key == "GOOGLE_OAUTH_CLIENT_ID" || key == "GOOGLE_OAUTH_CLIENT_SECRET" {
            println!("cargo:rustc-env={key}={value}");
        }
    }
}

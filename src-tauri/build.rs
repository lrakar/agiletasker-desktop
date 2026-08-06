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
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "desktop_info",
                "set_autostart",
                "pair_daemon",
                "unpair_daemon",
                "list_daemons",
                "restart_daemon",
                "stop_daemon",
                "start_daemon",
                "daemon_log_tail",
                "pick_directory",
                "check_for_shell_update",
            ]),
        ),
    )
    .expect("tauri_build::try_build failed");
}

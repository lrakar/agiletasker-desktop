// Prevents an extra console window from opening on Windows in release
// builds (the log plugin already covers stdout/file logging in dev).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    agiletasker_desktop_lib::run();
}

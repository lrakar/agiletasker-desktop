//! Startup reaping of stray daemon processes.
//!
//! A daemon is supposed to die with its supervising shell (managed mode:
//! stdin EOF ⇒ graceful shutdown), but a shell that exits without dropping
//! its `CommandChild`ren — the pre-0.1.3 updater replacing the process
//! mid-install, a crash, a `TerminateProcess` — leaves the node process
//! orphaned. The orphan then keeps the daemon uid's Firestore heartbeat
//! alive, so the NEXT shell instance's own daemon exits with a twin
//! conflict on every spawn attempt (120s retry cadence) and the host looks
//! offline from every device, indefinitely, while a daemon "is running".
//!
//! Since `tauri-plugin-single-instance` guarantees at most one live shell,
//! any process executing a bundle out of OUR `<app_data_dir>/daemons/` dir
//! at startup is by definition stray. Kill it (hard — we hold no stdin
//! handle to close gracefully) before spawning our own daemons.

use crate::daemon::manager::hard_kill;
use crate::daemon::paths;
use std::path::Path;

/// Finds and kills every process whose command line references a bundle
/// under `<app_data_dir>/daemons/`. MUST complete before
/// `DaemonManager::load_and_spawn_all` runs — the match is by command line,
/// so a concurrently spawned legitimate child would be indistinguishable
/// from a stray (see the sequencing task in `lib.rs`'s `setup()`).
pub async fn reap_stray_daemons(app_data_dir: &Path) {
    let marker = paths::daemons_dir(app_data_dir).to_string_lossy().to_string();
    for pid in find_stray_pids(&marker).await {
        log::warn!("reaping stray daemon process {pid} (left behind by a previous shell instance)");
        hard_kill(pid).await;
    }
}

#[cfg(windows)]
async fn find_stray_pids(marker: &str) -> Vec<u32> {
    use base64::Engine as _;
    // -EncodedCommand (base64 of UTF-16LE) sidesteps every layer of
    // quote-mangling between Rust's arg array, the Win32 command-line
    // string, and PowerShell's own -Command parsing.
    const SCRIPT: &str = r#"Get-CimInstance Win32_Process -Filter "Name='node.exe'" | ForEach-Object { "$($_.ProcessId)|$($_.CommandLine)" }"#;
    let utf16le: Vec<u8> = SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16le);

    let mut cmd = tokio::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded]);
    // CREATE_NO_WINDOW — a GUI-subsystem parent would otherwise flash a
    // console window on every launch.
    cmd.creation_flags(0x0800_0000);
    match cmd.output().await {
        Ok(output) => parse_pid_lines(&String::from_utf8_lossy(&output.stdout), marker),
        Err(e) => {
            log::warn!("stray-daemon scan failed to run: {e}");
            Vec::new()
        }
    }
}

#[cfg(not(windows))]
async fn find_stray_pids(marker: &str) -> Vec<u32> {
    // `pgrep -f` matches against the full command line. Exit status 1 just
    // means "no match" — stdout is empty either way, so no special-casing.
    match tokio::process::Command::new("pgrep").args(["-f", marker]).output().await {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect(),
        Err(e) => {
            log::warn!("stray-daemon scan failed to run: {e}");
            Vec::new()
        }
    }
}

/// Parses `pid|command line` rows (the Windows scan's output shape) and
/// keeps the pids whose command line contains `marker`, case-insensitively
/// (NTFS paths compare case-insensitively, and the daemon may have been
/// launched with different casing than `app_data_dir` resolves to now).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_pid_lines(stdout: &str, marker: &str) -> Vec<u32> {
    let marker_lower = marker.to_lowercase();
    stdout
        .lines()
        .filter_map(|line| {
            let (pid, cmdline) = line.split_once('|')?;
            if cmdline.to_lowercase().contains(&marker_lower) {
                pid.trim().parse().ok()
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = r"C:\Users\U\AppData\Roaming\com.agiletasker.desktop\daemons";

    #[test]
    fn matches_only_lines_containing_the_marker() {
        let stdout = concat!(
            "1234|\"C:\\node.exe\" C:\\somewhere\\else\\script.mjs\n",
            "5678|\"C:\\Users\\U\\AppData\\Local\\AgileTasker\\node.exe\" \"C:\\Users\\U\\AppData\\Roaming\\com.agiletasker.desktop\\daemons\\agiletasker-host.mjs\" --pair host-x:y\n",
            "9012|\n",
        );
        assert_eq!(parse_pid_lines(stdout, MARKER), vec![5678]);
    }

    #[test]
    fn marker_match_is_case_insensitive() {
        let stdout = "42|node.exe c:\\users\\u\\appdata\\roaming\\COM.AGILETASKER.DESKTOP\\daemons\\agiletasker-agent.mjs\n";
        assert_eq!(parse_pid_lines(stdout, MARKER), vec![42]);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let stdout = format!("no-separator-here\nnot-a-pid|{MARKER}\n|{MARKER}\n77|{MARKER}\n");
        assert_eq!(parse_pid_lines(&stdout, MARKER), vec![77]);
    }

    #[test]
    fn empty_scan_yields_nothing() {
        assert!(parse_pid_lines("", MARKER).is_empty());
    }
}

use log::error;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

const LOG_FILE_PATH: &str = "/var/log/nginx/cashu_redemption.log";

/// Whether the "cannot open" error has been reported. The redemption loop calls
/// this many times a cycle, and one unreadable path should not fill the log.
static OPEN_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

/// Create the log file and give it to the user the workers run as.
///
/// Called from `init_module`, in the master: the log directory is root-owned,
/// but redemption writes from a worker. Ownership follows the Cashu data
/// directory, which the operator already sets — the same rule the database uses.
pub fn prepare_log_file(data_dir_owner: Option<(u32, u32)>) {
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE_PATH)
    {
        error!("Cannot create {}: {}", LOG_FILE_PATH, e);
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some((uid, gid)) = data_dir_owner {
            let _ = std::os::unix::fs::chown(LOG_FILE_PATH, Some(uid), Some(gid));
        }
        let _ = std::fs::set_permissions(LOG_FILE_PATH, std::fs::Permissions::from_mode(0o640));
    }
}

/// Helper function to log Cashu redemption task messages to a dedicated file.
/// If file logging fails, errors are logged to the nginx error log.
///
/// `msg` is sanitised before writing: newline and carriage-return characters
/// are stripped to prevent log-injection attacks.
pub fn log_redemption(msg: &str) {
    // Strip characters that could be used for log injection:
    // - CR/LF: inject new log lines
    // - NUL: truncate log entries in some parsers
    // - ESC (0x1b): ANSI escape sequences that corrupt terminal/log viewers
    // - TAB: column-injection in tab-delimited log parsers
    let sanitised: String = msg
        .chars()
        .filter(|&c| c != '\n' && c != '\r' && c != '\0' && c != '\x1b' && c != '\t')
        .collect();

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE_PATH)
    {
        Ok(mut file) => {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            if let Err(e) = writeln!(file, "[{}] {}", timestamp, sanitised) {
                error!("Failed to write to cashu redemption log: {}", e);
            }
        }
        Err(e) => {
            // Once only: this runs every redemption cycle.
            if !OPEN_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                error!(
                    "Failed to open cashu redemption log file {}: {} — redemption continues, \
                     progress is in the nginx error log",
                    LOG_FILE_PATH, e
                );
            }
        }
    }
}

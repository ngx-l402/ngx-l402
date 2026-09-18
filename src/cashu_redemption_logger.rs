use log::error;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

const LOG_FILE_PATH: &str = "/var/log/nginx/cashu_redemption.log";

/// Whether the "cannot open" error has been reported. The redemption loop calls
/// this many times a cycle, and one unreadable path should not fill the log.
static OPEN_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

/// Open the log for appending, refusing a symlink (`ELOOP`).
///
/// `O_NOFOLLOW` is load-bearing: `O_CREAT` follows an existing symlink unless
/// paired with `O_EXCL`, so a link planted here before startup would redirect
/// the master's root-privileged fchown/fchmod — and every write — onto its
/// target. Guards the final component only; the root-owned parent is trusted.
fn open_log_file() -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(LOG_FILE_PATH)
}

/// Create the log file and give it to the user the workers run as.
///
/// Called from `init_module`, in the master: the log directory is root-owned,
/// but redemption writes from a worker. Ownership follows the Cashu data
/// directory, which the operator already sets — the same rule the database uses,
/// group-writable too: a root-owned data directory names the workers by group.
pub fn prepare_log_file(data_dir_owner: Option<(u32, u32)>) {
    // fchown/fchmod on the descriptor, not the path — see `open_log_file`.
    let file = match open_log_file() {
        Ok(file) => file,
        Err(e) => {
            error!(
                "Cannot create {} (ELOOP = symlink, refused): {}",
                LOG_FILE_PATH, e
            );
            return;
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some((uid, gid)) = data_dir_owner {
            let _ = std::os::unix::fs::fchown(&file, Some(uid), Some(gid));
        }
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o660));
    }
}

/// Helper function to log Cashu redemption task messages to a dedicated file.
/// If file logging fails, errors are logged to the nginx error log.
///
/// `msg` is sanitised before writing: all control characters are stripped to
/// prevent log-injection attacks.
pub fn log_redemption(msg: &str) {
    // Whole control range, not a blocklist: CR/LF forge log lines, NUL truncates
    // entries, ESC injects ANSI, TAB breaks column parsers.
    let sanitised: String = msg.chars().filter(|c| !c.is_control()).collect();

    match open_log_file() {
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

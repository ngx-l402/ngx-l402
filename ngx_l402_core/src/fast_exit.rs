//! Ending the process at `exit()` before libraries tear down global state.
//!
//! nginx ends its processes with `exit()`, which runs the exit handlers
//! libraries registered, such as `OPENSSL_cleanup`. The module's threads (Tokio
//! runtimes, the Cashu redemption thread, r2d2's reaper) are never stopped, so
//! they can use that state while it is freed, and the exiting process dies with
//! SIGSEGV (issue #201).

use std::io;

/// Register an exit handler that calls `_exit` with the process's exit status.
///
/// Exit handlers run in reverse order of registration, so every handler
/// registered before this one is skipped. Call it once, before `fork()`, after
/// the libraries whose teardown should be skipped have been initialised.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn install_fast_exit() -> io::Result<()> {
    extern "C" {
        // glibc; not in the libc crate.
        fn on_exit(
            function: extern "C" fn(libc::c_int, *mut libc::c_void),
            arg: *mut libc::c_void,
        ) -> libc::c_int;
    }

    extern "C" fn exit_now(status: libc::c_int, _arg: *mut libc::c_void) {
        unsafe { libc::_exit(status) };
    }

    if unsafe { on_exit(exit_now, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::other("on_exit failed"));
    }
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn install_fast_exit() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fast exit needs glibc's on_exit",
    ))
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use std::process::Command;

    const CHILD_ENV: &str = "NGX_L402_FAST_EXIT_CHILD";

    // Runs itself as a child process, since the behaviour under test ends it.
    #[test]
    fn skips_earlier_handlers_and_keeps_status() {
        if std::env::var_os(CHILD_ENV).is_some() {
            extern "C" fn earlier_handler() {
                unsafe { libc::_exit(99) };
            }
            unsafe { libc::atexit(earlier_handler) };
            super::install_fast_exit().unwrap();
            std::process::exit(3);
        }

        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fast_exit::tests::skips_earlier_handlers_and_keeps_status",
            ])
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(3));
    }
}

//! A restart asked for from the UI or the API goes through the same door as a
//! SIGTERM: drain, flush resume data, then leave.
//!
//! 4.3 called `process::exit(0)` from the handler. Nothing was flushed, so up
//! to five minutes of piece progress and byte counters were lost, and exit
//! code 0 is a clean stop to systemd: a `Restart=on-failure` unit stayed down
//! while the UI said "restarting".

use std::sync::atomic::{AtomicBool, Ordering};

/// The exit code of a requested restart. Not 0, so `Restart=on-failure`
/// brings the service back; 75 is EX_TEMPFAIL, "try again".
pub const RESTART_EXIT_CODE: i32 = 75;

static RESTART: AtomicBool = AtomicBool::new(false);

/// Ask the running daemon to stop cleanly and come back.
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
    // notify_one, not notify_waiters: the permit is kept if the shutdown
    // future is between two polls, so the request cannot be lost.
    crate::tray::quit_notify().notify_one();
}

pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

static STOPPING: AtomicBool = AtomicBool::new(false);

/// The stop has begun. Set when the drain starts, so the streams that never
/// end on their own (the event stream, the log tail) end themselves instead
/// of holding the drain for its whole 5 s budget.
pub fn begin_stop() {
    STOPPING.store(true, Ordering::SeqCst);
}

pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Leave with the restart code. On Windows nothing supervises the process,
/// so it starts its successor itself first, with the same arguments.
pub fn exit_for_restart() -> ! {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            match std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn() {
                Ok(_) => tracing::info!("restart: successor started"),
                Err(e) => tracing::error!("restart: cannot start the successor: {e}"),
            }
        }
    }
    tracing::info!("restart: exiting with code {RESTART_EXIT_CODE} for the supervisor");
    std::process::exit(RESTART_EXIT_CODE)
}

/// The name the updater signals to ask for a clean stop. Shared with
/// `hydranos-update`, which has no API key and cannot use the restart route.
#[cfg(windows)]
pub const STOP_EVENT: &str = "Local\\HydranosStop";

/// Listen for the updater's stop request.
///
/// 4.3's updater ran `taskkill` without /F, which only posts WM_CLOSE to
/// top-level windows; the tray's window is message-only, so the request
/// never arrived, and the update gave up after 60 s. A named event is
/// something both processes can reach.
#[cfg(windows)]
pub fn spawn_stop_event_listener() {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    let name: Vec<u16> = STOP_EVENT.encode_utf16().chain(std::iter::once(0)).collect();
    // Manual reset, initially unset.
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if handle.is_null() {
        tracing::warn!("cannot create the stop event; the updater will not be able to stop this process cleanly");
        return;
    }
    let handle = handle as usize;
    let _ = std::thread::Builder::new()
        .name("stop-event".into())
        .spawn(move || {
            unsafe { WaitForSingleObject(handle as _, INFINITE) };
            tracing::info!("stop requested by the updater");
            crate::tray::quit_notify().notify_one();
        });
}

#[cfg(not(windows))]
pub fn spawn_stop_event_listener() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restart_code_is_a_failure_to_systemd() {
        assert_ne!(RESTART_EXIT_CODE, 0);
    }
}

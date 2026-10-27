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

/// The stop has begun. Set when the drain starts, so the streams that never
/// end on their own (the event stream, the log tail) end themselves instead
/// of holding the drain for its whole 5 s budget.
///
/// It is the signal of `typhon_engine::stop`, so the same call also ends the
/// background loops between two iterations and closes the peer listeners.
pub fn begin_stop() {
    typhon_engine::stop::global().begin();
}

pub fn stopping() -> bool {
    typhon_engine::stop::global().is_set()
}

/// Wait `d` on the process-wide signal: true when the whole wait went by,
/// false when the stop began and the loop must return. See
/// `typhon_engine::stop::Stop::sleep`.
pub async fn sleep(d: std::time::Duration) -> bool {
    typhon_engine::stop::global().sleep(d).await
}

/// `Interval::tick` on the process-wide signal, same answer as `sleep`.
pub async fn tick(interval: &mut tokio::time::Interval) -> bool {
    typhon_engine::stop::global().tick(interval).await
}

/// Leave with the restart code. On Windows nothing supervises the process,
/// so it starts its successor itself first, with the same arguments.
pub fn exit_for_restart() -> ! {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(std::env::args_os().skip(1));
            match spawn_successor(cmd) {
                Ok(_) => tracing::info!("restart: successor started"),
                Err(e) => tracing::error!("restart: cannot start the successor: {e}"),
            }
        }
    }
    tracing::info!("restart: exiting with code {RESTART_EXIT_CODE} for the supervisor");
    std::process::exit(RESTART_EXIT_CODE)
}

/// Start the successor outside the job object this process may run in.
///
/// A launcher that puts the app in a job with KILL_ON_JOB_CLOSE (an OpenSSH
/// session does) kills every process of the job when the old process exits:
/// a successor that stayed in the job died with it. CREATE_BREAKAWAY_FROM_JOB
/// starts it outside. When the job forbids breaking away (the creation is
/// refused with ERROR_ACCESS_DENIED), start it the plain way: it may not
/// survive, but a restart that cannot be done any better is still tried.
#[cfg(windows)]
fn spawn_successor(mut cmd: std::process::Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
    match cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            tracing::warn!(
                "restart: the job this process runs in forbids breaking away; \
                 the successor starts inside it and may end with it"
            );
            cmd.creation_flags(0).spawn()
        }
        other => other,
    }
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
    let name: Vec<u16> = STOP_EVENT
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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

/// Windows only: a successor started from inside a job that kills its
/// processes on close must not be in that job.
#[cfg(all(test, windows))]
mod job_tests {
    use super::spawn_successor;
    use std::process::Command;
    use windows_sys::Win32::Foundation::{BOOL, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn a_restart_from_inside_a_job_starts_the_successor_outside_it() {
        unsafe {
            let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null(), "CreateJobObjectW failed");
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            assert!(set != 0, "SetInformationJobObject failed");
            // The job handle is never closed: closing it would kill this test.
            let assigned = AssignProcessToJobObject(job, GetCurrentProcess());
            assert!(
                assigned != 0,
                "this process cannot be put in a job: {}",
                std::io::Error::last_os_error()
            );

            let mut cmd = Command::new("cmd");
            cmd.args(["/c", "ping -n 3 127.0.0.1 >nul"]);
            let mut child = spawn_successor(cmd).expect("the successor starts");
            let mut inside: BOOL = 0;
            let ok = IsProcessInJob(
                std::os::windows::io::AsRawHandle::as_raw_handle(&child) as HANDLE,
                job,
                &mut inside,
            );
            let _ = child.kill();
            let _ = child.wait();
            assert!(ok != 0, "IsProcessInJob failed");
            assert_eq!(inside, 0, "the successor stayed in the job of its parent");
        }
    }
}

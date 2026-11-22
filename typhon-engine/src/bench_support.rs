//! Test-only process measurements used by the opt-in live UI benchmarks.
pub static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn start() {
    RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(windows)]
pub fn cpu_ms() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    // SAFETY: FILETIME has no invalid bit patterns; all pointers refer to live
    // writable stack values, and the pseudo-handle belongs to this process.
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { std::mem::zeroed() };
    assert_ne!(
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        },
        0
    );
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    (ticks(kernel) + ticks(user)) as f64 / 10000.0
}

#[cfg(unix)]
pub fn cpu_ms() -> f64 {
    // SAFETY: the kernel writes one timespec to a valid, aligned stack value.
    let mut time: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1e6
}

#[cfg(windows)]
pub fn resident_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: this POD buffer has the size required by GetProcessMemoryInfo.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of_val(&counters) as u32;
    assert_ne!(
        unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) },
        0
    );
    counters.WorkingSetSize as u64
}

#[cfg(target_os = "linux")]
pub fn resident_bytes() -> u64 {
    let text = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    // SAFETY: sysconf reads a constant property and takes no pointer.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn resident_bytes() -> u64 {
    0
}

pub fn count() -> usize {
    std::env::var("HYDRANOS_BENCH_N")
        .ok()
        .map(|n| n.parse().unwrap())
        .unwrap_or(100_000)
}

pub fn emit(name: &str, value: serde_json::Value) {
    println!(
        "LIVE_BENCH {}",
        serde_json::json!({"scenario": name, "data": value})
    );
}

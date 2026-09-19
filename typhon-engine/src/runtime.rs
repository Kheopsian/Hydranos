//! How many tokio workers the process runs on.
//!
//! `#[tokio::main]` defaults to one worker per visible core. On the 128-core
//! host this engine runs on, that built 128 workers for a load measured at
//! 7-10 cores, and they spent the difference stealing work from each other.
//! Profiled on prod 2026-09-13 (`perf record -F 199 -g`, 4.27.0, 5.3k peers):
//!
//! | symbol                              | share of all CPU |
//! |-------------------------------------|------------------|
//! | `queue::Steal<T>::steal_into`       | 8.09%            |
//! | `context::thread_rng_n`             | 2.96%            |
//! | `worker::Context::run`              | 2.09%            |
//!
//! Plus the kernel scheduler traffic those threads caused (`update_curr`,
//! `pick_task_fair`, `__schedule`, queued spinlock: 4.08%). None of it is
//! BitTorrent work.
//!
//! This lives in the library rather than next to a `main` because there are two
//! binaries in this crate and only one of them is shipped -- putting the knob in
//! `src/main.rs` (the unbuilt `typhon-engine` bin) is exactly the mistake that
//! made the first version of this change a no-op.

/// Worker threads for the engine runtime.
///
/// `HYDRANOS_WORKER_THREADS` overrides it, so the number can follow the machine
/// without a rebuild. The default caps rather than scales: past this point extra
/// workers add stealing, not throughput. The engine is I/O bound, and blocking
/// work goes to the blocking pool, which is sized separately and left alone.
pub fn worker_threads() -> usize {
    if let Some(n) = std::env::var("HYDRANOS_WORKER_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    cores.min(32)
}

//! Allocator diagnostics for the unified daemon.
//!
//! 4.0.0 merged the Go front and the Rust engine into one binary and wrote a
//! new `main.rs` beside the old one. `#[global_allocator]` is per binary, not
//! per crate, so the attribute stayed on `typhon-engine` and `hydra` shipped on
//! the glibc allocator -- silently. The Dockerfile still set MALLOC_CONF and
//! the container still carried prof settings, both inert: glibc ignores them.
//!
//! Measured cost on the 300k-torrent node: V3 held a flat 14.4 GiB of RSS for
//! 36 hours; V4 climbed to 100 GiB in 7 hours. glibc gives each thread its own
//! arena (8 x nproc = up to 1024 here, 728 materialised across 630 threads) and
//! never returns a secondary arena's pages to the kernel -- there is no
//! equivalent of jemalloc's dirty_decay_ms.
//!
//! The handlers below are lifted from `src/main.rs`, so both binaries answer
//! the same signals.

use tracing;

/// Install the SIGUSR1 heap dump, the SIGUSR2 decay purge, and the five-minute
/// stats line. No-ops on Windows, which has no jemalloc and no signals.
pub fn spawn() {
    #[cfg(unix)]
    {
        spawn_dump();
        spawn_decay();
        spawn_stats();
    }
}

/// SIGUSR1 => dump a jemalloc heap profile to $prof_prefix (set via
/// MALLOC_CONF). This is the only way to get an allocation-site profile of a
/// running node, and it went missing for the whole of the 4.x line.
#[cfg(unix)]
fn spawn_dump() {
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::user_defined1()) {
            Ok(mut sig) => loop {
                sig.recv().await;
                let r = unsafe {
                    tikv_jemalloc_ctl::raw::write(
                        b"prof.dump\0",
                        std::ptr::null::<std::os::raw::c_char>(),
                    )
                };
                match r {
                    Ok(_) => tracing::warn!("jemalloc heap profile dumped (SIGUSR1)"),
                    Err(e) => tracing::error!("jemalloc prof.dump failed: {}", e),
                }
            },
            Err(e) => tracing::error!("SIGUSR1 handler setup failed: {}", e),
        }
    });
}

/// SIGUSR2 => hand the allocator's dirty pages back to the kernel, now.
///
/// `dirty_decay_ms`/`muzzy_decay_ms` are the RAM/CPU dial and they are set
/// through MALLOC_CONF -- an environment variable, so moving them normally
/// means recreating the container. Both are writable at runtime, so expose them
/// on a signal instead.
#[cfg(unix)]
fn spawn_decay() {
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        use tikv_jemalloc_ctl::{epoch, stats};
        let mb = |v: usize| v as f64 / (1024.0 * 1024.0);
        let snapshot = || {
            let _ = epoch::advance();
            (
                stats::allocated::read().unwrap_or(0),
                stats::resident::read().unwrap_or(0),
            )
        };
        match signal(SignalKind::user_defined2()) {
            Ok(mut sig) => loop {
                sig.recv().await;
                let (al0, re0) = snapshot();
                // NOT MALLCTL_ARENAS_ALL (4096). That sentinel is only accepted
                // by arena.<i>.{purge,decay,reset,destroy};
                // arena.<i>.dirty_decay_ms resolves the index through
                // arena_get(), which indexes the arena array unchecked in a
                // release build. Passing 4096 reads out of bounds -- verified
                // on an isolated engine, where it segfaulted the process.
                let narenas: u32 =
                    unsafe { tikv_jemalloc_ctl::raw::read(b"arenas.narenas\0").unwrap_or(0) };
                // Arenas are created lazily, so most of 0..narenas do not exist
                // yet and answer EFAULT. An arena that was never initialised
                // holds no dirty pages: skip it and keep going.
                let mut moved = 0u32;
                let mut err = None;
                for i in 0..narenas {
                    let d = format!("arena.{}.dirty_decay_ms\0", i);
                    let m = format!("arena.{}.muzzy_decay_ms\0", i);
                    let r = unsafe {
                        tikv_jemalloc_ctl::raw::write(d.as_bytes(), 0isize)
                            .and_then(|_| tikv_jemalloc_ctl::raw::write(m.as_bytes(), 0isize))
                    };
                    match r {
                        Ok(_) => moved += 1,
                        Err(e) => err = Some(e),
                    }
                }
                unsafe {
                    let _ = tikv_jemalloc_ctl::raw::write(b"arenas.dirty_decay_ms\0", 0isize);
                    let _ = tikv_jemalloc_ctl::raw::write(b"arenas.muzzy_decay_ms\0", 0isize);
                }
                if narenas == 0 {
                    tracing::error!("jemalloc: arenas.narenas unreadable, decay left alone");
                } else if moved == 0 {
                    tracing::error!(
                        "jemalloc decay moved on no arena out of {}: {}",
                        narenas,
                        err.map(|e| e.to_string()).unwrap_or_default()
                    );
                } else {
                    let (al1, re1) = snapshot();
                    tracing::warn!(
                        "jemalloc decay forced to 0 on {}/{} arenas (SIGUSR2): resident {:.0}MiB -> {:.0}MiB (freed {:.0}MiB), allocated {:.0}MiB -> {:.0}MiB",
                        moved, narenas, mb(re0), mb(re1), mb(re0.saturating_sub(re1)), mb(al0), mb(al1)
                    );
                }
            },
            Err(e) => tracing::error!("SIGUSR2 handler setup failed: {}", e),
        }
    });
}

/// Exact allocator accounting, every five minutes.
///
///   allocated          bytes the application actually holds
///   active - allocated allocator slop inside live pages
///   resident - active  dirty pages jemalloc kept instead of returning
///   retained           address space unmapped, costs no RSS
///
/// If resident tracks allocated, the memory is real and the fix is in the code.
/// If resident dwarfs it, the fix is in the decay settings. Unsampled, so it
/// separates the two where a heap profile cannot.
#[cfg(unix)]
fn spawn_stats() {
    tokio::spawn(async {
        use tikv_jemalloc_ctl::{epoch, stats};
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            tick.tick().await;
            // Stats are cached; advancing the epoch is what refreshes them.
            if epoch::advance().is_err() {
                continue;
            }
            let mb = |v: usize| v as f64 / (1024.0 * 1024.0);
            match (
                stats::allocated::read(),
                stats::active::read(),
                stats::resident::read(),
                stats::mapped::read(),
                stats::retained::read(),
            ) {
                (Ok(al), Ok(ac), Ok(re), Ok(ma), Ok(rt)) => tracing::info!(
                    "jemalloc allocated={:.0}MiB active={:.0}MiB resident={:.0}MiB mapped={:.0}MiB retained={:.0}MiB metadata={:.0}MiB slop={:.0}MiB dirty={:.0}MiB",
                    mb(al), mb(ac), mb(re), mb(ma), mb(rt),
                    mb(stats::metadata::read().unwrap_or(0)),
                    mb(ac.saturating_sub(al)), mb(re.saturating_sub(ac))
                ),
                _ => tracing::warn!("jemalloc stats unavailable"),
            }
        }
    });
}

//! Pinning every socket of this engine to ONE network device.
//!
//! `bind_interface` used to be honoured by resolving the interface to its IPv4
//! and using that as the socket's source address. ProtonVPN hands EVERY tunnel
//! the same `10.2.0.2`, so that pinned nothing: two engines asked for the same
//! source and the kernel routed both by destination, out whichever tunnel held
//! the default route. Measured on two Proton servers, a source-IP bind reported
//! one exit address for both tunnels while `SO_BINDTODEVICE` reported two.
//!
//! It matters in both directions. Outbound, a dial leaves by the wrong tunnel.
//! Inbound is worse: the listener accepts a peer arriving on the second tunnel
//! (both interfaces carry `10.2.0.2`, so the socket matches), and the reply then
//! leaves by the default route with a different public address. The peer sees
//! packets from an address it never connected to and drops them, so the
//! connection dies without either side logging anything.
//!
//! The device is process-wide, not per-binding: one engine is one process with
//! one `bind_interface`. Multi-tunnel WITHIN one engine stays steered by fwmark,
//! which is what that setup was built on.

use std::sync::OnceLock;

static BIND_DEVICE: OnceLock<String> = OnceLock::new();

/// Records the device every socket of this process must leave by. Empty names
/// are ignored, so an unset config keeps the kernel's default behaviour.
pub fn set_bind_device(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let _ = BIND_DEVICE.set(name.to_string());
}

/// The configured device, if any.
pub fn bind_device() -> Option<&'static str> {
    BIND_DEVICE.get().map(|s| s.as_str())
}

/// Pins one already-created socket to the configured device.
///
/// Returns Ok(()) when nothing is configured, so callers can apply it
/// unconditionally. An error here must FAIL the socket rather than be ignored:
/// carrying on means leaving by the default route, which is the exact leak this
/// exists to close.
#[cfg(target_os = "linux")]
pub fn pin_fd(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    let Some(dev) = bind_device() else {
        return Ok(());
    };
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            dev.as_ptr() as *const libc::c_void,
            dev.len() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// No SO_BINDTODEVICE outside Linux; the Windows agent keeps the source-address
/// pin the Go side still applies.
#[cfg(not(target_os = "linux"))]
pub fn pin_fd(_fd: i32) -> std::io::Result<()> {
    Ok(())
}

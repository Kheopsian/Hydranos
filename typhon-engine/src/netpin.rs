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
//! The device travels with the socket, not in a global. It used to be a
//! process-wide `OnceLock`, which was sound while one engine meant one process.
//! Hydra 4 runs race and hoard in the same process: a second `set` on a
//! `OnceLock` fails silently, so the second engine kept the first one's device
//! -- or none -- and its traffic left by the default route. That is a leak with
//! no error and no log, which is the exact failure this module exists to
//! prevent, so the device is now carried per egress alongside the fwmark.

/// Where one socket must leave by.
///
/// Both fields steer the same decision and are always chosen together: the
/// fwmark picks the routing table, the device pins the interface for setups
/// like Proton where every tunnel shares one source address and a mark alone
/// decides nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Egress {
    pub fwmark: u32,
    /// Outbound SOCKS5 for v6 dials, when the engine has one. It belongs here
    /// rather than in a global for the same reason as the device: it decides
    /// which address a peer sees, and two engines can be sent out different
    /// ways.
    pub socks5: Option<std::sync::Arc<crate::peer::Socks5Config>>,
    /// Interface name. Empty means "let the kernel decide", which is the
    /// correct behaviour for an engine with no `bind_interface`.
    pub device: String,
}

impl Egress {
    /// True when this egress asks for anything at all. A socket for an
    /// unconstrained engine can skip the whole pre-connect dance.
    pub fn is_steered(&self) -> bool {
        // Defined in terms of device(), not the raw field: a name of blanks is
        // not a device, and the two must never disagree -- taking the steered
        // path with nothing to apply is a socket set up for a pin that never
        // happens.
        self.fwmark != 0 || self.device().is_some()
    }

    pub fn device(&self) -> Option<&str> {
        let d = self.device.trim();
        if d.is_empty() {
            None
        } else {
            Some(d)
        }
    }
}

/// Pins one already-created socket to the configured device.
///
/// Returns Ok(()) when nothing is configured, so callers can apply it
/// unconditionally. An error here must FAIL the socket rather than be ignored:
/// carrying on means leaving by the default route, which is the exact leak this
/// exists to close.
#[cfg(target_os = "linux")]
pub fn pin_fd(fd: std::os::unix::io::RawFd, egress: &Egress) -> std::io::Result<()> {
    let Some(dev) = egress.device() else {
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

/// No SO_BINDTODEVICE outside Linux, so a socket asked to leave by a device
/// fails instead. This used to answer Ok and pin nothing: on Windows and
/// macOS an engine with `bind_interface` dialled every peer by the default
/// route while its config said tunnel.
#[cfg(not(target_os = "linux"))]
pub fn pin_fd(_fd: i32, egress: &Egress) -> std::io::Result<()> {
    if egress.device().is_none() {
        return Ok(());
    }
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, UNSUPPORTED))
}

/// Whether `bind_interface` can be honoured here. Linux only: a device pin is
/// `SO_BINDTODEVICE`. Windows has nothing that pins by interface NAME, and
/// macOS's `IP_BOUND_IF` reaches uTP and HTTP but not the TCP peer sockets,
/// so half an engine would follow the tunnel and half the default route --
/// the kind of leak nobody notices. The key is refused there instead
/// (maintainer's decision, 4.4).
pub const DEVICE_PIN_SUPPORTED: bool = cfg!(target_os = "linux");

/// Why, in the words the API, the log and the Network tab use.
pub const UNSUPPORTED: &str = "bind_interface is Linux-only: this platform cannot pin every socket of an engine to an interface (TCP peers would leave by the default route). Run the VPN client system-wide instead";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_device_steers_nothing() {
        let e = Egress::default();
        assert!(!e.is_steered());
        assert_eq!(e.device(), None);
        // Whitespace is not a device name; treating " " as one would hand
        // SO_BINDTODEVICE a name no interface has and fail every socket.
        let blank = Egress { fwmark: 0, device: "  ".into(), ..Default::default() };
        assert_eq!(blank.device(), None);
        assert!(!blank.is_steered());
    }

    /// ⭐ #62. Outside Linux a socket asked to leave by a device is refused,
    /// never opened unpinned (4.3 answered Ok and pinned nothing); a socket
    /// with no device is untouched everywhere.
    #[cfg(unix)]
    #[test]
    fn a_device_pin_that_cannot_be_applied_fails_closed() {
        use std::os::fd::AsRawFd;
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(pin_fd(sock.as_raw_fd(), &Egress::default()).is_ok(), "no device, nothing to refuse");
        let wg = Egress { device: "hydranos-test-nodev".into(), ..Default::default() };
        assert!(pin_fd(sock.as_raw_fd(), &wg).is_err(), "a missing (Linux) or unpinnable (elsewhere) device fails");
        assert_eq!(DEVICE_PIN_SUPPORTED, cfg!(target_os = "linux"));
        assert!(UNSUPPORTED.contains("Linux-only"));
    }

    #[test]
    fn a_fwmark_alone_still_counts_as_steered() {
        // The single-tunnel-with-mark setup has no device: skipping the
        // pre-connect socket setup for it would drop the mark.
        let e = Egress { fwmark: 42, device: String::new(), ..Default::default() };
        assert!(e.is_steered());
        assert_eq!(e.device(), None);
    }

    // The reason this type exists. Two engines in ONE process, each pinned to
    // its own tunnel: with the previous process-wide OnceLock the second `set`
    // was silently dropped and hoard's sockets left by the default route,
    // showing the home address to every tracker it announced to. Two
    // independent values in the same process is precisely what that could not
    // express, so this test fails against the design it replaced.
    #[test]
    fn two_engines_in_one_process_keep_their_own_device() {
        let race = Egress { fwmark: 0, device: "wg-race".into(), ..Default::default() };
        let hoard = Egress { fwmark: 0, device: "wg-hoard".into(), ..Default::default() };
        assert_eq!(race.device(), Some("wg-race"));
        assert_eq!(hoard.device(), Some("wg-hoard"));
        assert_ne!(race, hoard);
    }
}

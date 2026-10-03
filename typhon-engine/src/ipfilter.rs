//! The IP filter: addresses no connection is made with, in either direction.
//!
//! One list for the whole process. A peer is an address, not a torrent's
//! property: an address blocked for one engine and allowed for another would
//! be a filter with a hole in it.
//!
//! Checked at three places, all before anything costs:
//! - an inbound connection, before the handshake and before MSE;
//! - an outbound dial, before the TCP connect;
//! - a session already running, on its next turn once the list changes --
//!   a ban that only stopped NEW connections would leave the peer it was
//!   aimed at connected for hours.
//!
//! The list is sorted, non-overlapping ranges searched by bisection: a
//! level1-sized list (a quarter of a million ranges) is a few megabytes and
//! eighteen comparisons per check. When no list is installed the check is one
//! relaxed atomic load.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Sorted, merged, non-overlapping inclusive ranges.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct IpFilter {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

/// What a parse made of its input.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ParseStats {
    pub ranges: usize,
    /// Lines that were neither a range nor a comment.
    pub unreadable: usize,
    /// eMule entries whose access level lets the range through (128 and up).
    pub allowed: usize,
}

/// An IPv4-mapped IPv6 address is the IPv4 address: a peer reaching a dual
/// stack socket over v4 shows up as `::ffff:a.b.c.d`, and a v4 range must
/// still catch it.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// IPv4 with leading zeros allowed: eMule lists write `001.002.003.004`,
/// which the standard parser refuses (it reads a leading zero as octal).
fn parse_v4(s: &str) -> Option<u32> {
    let mut out: u32 = 0;
    let mut n = 0;
    for part in s.trim().split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = part.parse().ok()?;
        if v > 255 {
            return None;
        }
        out = (out << 8) | v;
        n += 1;
    }
    (n == 4).then_some(out)
}

fn parse_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim();
    if let Some(v4) = parse_v4(s) {
        return Some(IpAddr::V4(Ipv4Addr::from(v4)));
    }
    s.parse::<Ipv6Addr>().ok().map(|v6| canonical(IpAddr::V6(v6)))
}

enum Range {
    V4(u32, u32),
    V6(u128, u128),
}

fn range_of(a: IpAddr, b: IpAddr) -> Option<Range> {
    match (canonical(a), canonical(b)) {
        (IpAddr::V4(x), IpAddr::V4(y)) => {
            let (x, y) = (u32::from(x), u32::from(y));
            Some(Range::V4(x.min(y), x.max(y)))
        }
        (IpAddr::V6(x), IpAddr::V6(y)) => {
            let (x, y) = (u128::from(x), u128::from(y));
            Some(Range::V6(x.min(y), x.max(y)))
        }
        _ => None,
    }
}

/// `a.b.c.d/n` or `x::/n`.
fn cidr(s: &str) -> Option<Range> {
    let (ip, len) = s.split_once('/')?;
    let len: u32 = len.trim().parse().ok()?;
    match parse_ip(ip)? {
        IpAddr::V4(v4) if len <= 32 => {
            let base = u32::from(v4);
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            Some(Range::V4(base & mask, (base & mask) | !mask))
        }
        IpAddr::V6(v6) if len <= 128 => {
            let base = u128::from(v6);
            let mask = if len == 0 { 0 } else { u128::MAX << (128 - len) };
            Some(Range::V6(base & mask, (base & mask) | !mask))
        }
        _ => None,
    }
}

/// `a - b`, `a-b`, a CIDR, or a single address.
fn plain_range(s: &str) -> Option<Range> {
    let s = s.trim();
    if s.contains('/') {
        return cidr(s);
    }
    // IPv6 addresses contain no '-', so the first one splits a range.
    if let Some((a, b)) = s.split_once('-') {
        return range_of(parse_ip(a)?, parse_ip(b)?);
    }
    let ip = parse_ip(s)?;
    range_of(ip, ip)
}

/// One line of any of the formats in use.
///
/// - eMule `ipfilter.dat`: `start - end , level , description`. A level of
///   128 or more means *allowed* and is skipped, as eMule and qBittorrent do.
/// - PeerGuardian P2P: `description:start-end`. The description may itself
///   hold colons, so the range is what follows the LAST one -- unless that
///   leaves a fragment of an IPv6 address, in which case the line is tried
///   whole.
/// - A CIDR, a range, or a single address, which is what a hand-kept list is.
enum Line {
    Skip,
    Allowed,
    Unreadable,
    Block(Range),
}

fn parse_line(raw: &str) -> Line {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("//") || line.starts_with(';') {
        return Line::Skip;
    }
    if line.contains(',') {
        let mut f = line.split(',');
        let range = f.next().unwrap_or("");
        let level = f.next().and_then(|l| l.trim().parse::<u32>().ok());
        return match (plain_range(range), level) {
            (Some(_), Some(l)) if l >= 128 => Line::Allowed,
            (Some(r), _) => Line::Block(r),
            (None, _) => Line::Unreadable,
        };
    }
    if let Some(r) = plain_range(line) {
        return Line::Block(r);
    }
    if let Some((_, tail)) = line.rsplit_once(':') {
        if let Some(r) = plain_range(tail) {
            return Line::Block(r);
        }
    }
    Line::Unreadable
}

fn merge<T: Copy + Ord + num_like::Succ>(mut v: Vec<(T, T)>) -> Vec<(T, T)> {
    v.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            // Overlapping or touching: one range.
            Some(last) if a <= last.1.succ() => {
                if b > last.1 {
                    last.1 = b;
                }
            }
            _ => out.push((a, b)),
        }
    }
    out
}

mod num_like {
    pub trait Succ {
        fn succ(self) -> Self;
    }
    impl Succ for u32 {
        fn succ(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Succ for u128 {
        fn succ(self) -> Self {
            self.saturating_add(1)
        }
    }
}

impl IpFilter {
    /// Parse a list in any of the supported formats, mixed if need be.
    pub fn parse(text: &str) -> (IpFilter, ParseStats) {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        let mut st = ParseStats::default();
        for line in text.lines() {
            match parse_line(line) {
                Line::Skip => {}
                Line::Allowed => st.allowed += 1,
                Line::Unreadable => st.unreadable += 1,
                Line::Block(Range::V4(a, b)) => {
                    st.ranges += 1;
                    v4.push((a, b));
                }
                Line::Block(Range::V6(a, b)) => {
                    st.ranges += 1;
                    v6.push((a, b));
                }
            }
        }
        (IpFilter { v4: merge(v4), v6: merge(v6) }, st)
    }

    /// Several lists as one.
    pub fn union(parts: &[IpFilter]) -> IpFilter {
        IpFilter {
            v4: merge(parts.iter().flat_map(|p| p.v4.iter().copied()).collect()),
            v6: merge(parts.iter().flat_map(|p| p.v6.iter().copied()).collect()),
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        fn hit<T: Ord + Copy>(v: &[(T, T)], x: T) -> bool {
            // The last range starting at or before x is the only candidate.
            let i = v.partition_point(|r| r.0 <= x);
            i > 0 && v[i - 1].1 >= x
        }
        match canonical(ip) {
            IpAddr::V4(a) => hit(&self.v4, u32::from(a)),
            IpAddr::V6(a) => hit(&self.v6, u128::from(a)),
        }
    }

    /// Ranges after merging.
    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static FILTER: RwLock<Option<Arc<IpFilter>>> = RwLock::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Bumped at every install, so a running session re-checks its peer once
/// per change rather than once per message.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Connections refused by the filter, per direction.
pub static BLOCKED_IN: AtomicU64 = AtomicU64::new(0);
pub static BLOCKED_OUT: AtomicU64 = AtomicU64::new(0);
/// Running sessions ended because their peer became blocked.
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Replace the list. `None`, or an empty one, turns filtering off.
pub fn install(f: Option<IpFilter>) {
    let f = f.filter(|f| !f.is_empty()).map(Arc::new);
    let on = f.is_some();
    if let Ok(mut slot) = FILTER.write() {
        *slot = f;
    }
    ACTIVE.store(on, Ordering::Release);
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Is this address filtered? One atomic load when no list is installed.
pub fn blocked(ip: IpAddr) -> bool {
    if !ACTIVE.load(Ordering::Acquire) {
        return false;
    }
    let f = match FILTER.read() {
        Ok(g) => g.clone(),
        Err(_) => return false,
    };
    f.is_some_and(|f| f.contains(ip))
}

/// Ranges in the installed list.
pub fn installed_len() -> usize {
    FILTER.read().ok().and_then(|g| g.as_ref().map(|f| f.len())).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn every_format_in_use_is_read() {
        let text = "\
# a comment
// another
; and eMule's
Some ISP:203.0.113.10-203.0.113.20
Weird: name: with colons:10.0.0.1-10.0.0.9
198.051.100.000 - 198.051.100.255 , 000 , eMule, blocked
192.000.002.000 - 192.000.002.255 , 200 , eMule, allowed
192.168.50.0/24
203.0.113.200
2001:db8::/32
2001:db9::1-2001:db9::ff
not an address at all
";
        let (f, st) = IpFilter::parse(text);
        assert_eq!(st, ParseStats { ranges: 7, unreadable: 1, allowed: 1 });
        for yes in ["203.0.113.10", "203.0.113.20", "10.0.0.5", "198.51.100.7", "192.168.50.99", "203.0.113.200", "2001:db8:ffff::1", "2001:db9::80"] {
            assert!(f.contains(ip(yes)), "{yes} should be blocked");
        }
        for no in ["203.0.113.9", "203.0.113.21", "192.0.2.9", "192.168.51.0", "203.0.113.201", "2001:dba::1", "2001:db9::100"] {
            assert!(!f.contains(ip(no)), "{no} should pass");
        }
    }

    /// A peer on a dual-stack socket shows up as ::ffff:a.b.c.d; the v4
    /// range still has to catch it.
    #[test]
    fn a_v4_mapped_address_is_its_v4_address() {
        let (f, _) = IpFilter::parse("198.51.100.0/24");
        assert!(f.contains(ip("::ffff:198.51.100.8")));
    }

    #[test]
    fn overlapping_and_touching_ranges_merge() {
        let (f, _) = IpFilter::parse("203.0.113.0-203.0.113.10\n203.0.113.5-203.0.113.20\n203.0.113.21-203.0.113.30\n203.0.113.40-203.0.113.50");
        assert_eq!(f.len(), 2, "three that touch are one, the fourth stands apart");
        assert!(f.contains(ip("203.0.113.30")) && !f.contains(ip("203.0.113.35")) && f.contains(ip("203.0.113.40")));
        let (g, _) = IpFilter::parse("0.0.0.0-255.255.255.255\n255.255.255.255");
        assert!(g.contains(ip("255.255.255.255")), "no overflow at the top");
        let u = IpFilter::union(&[f, IpFilter::parse("203.0.113.31-203.0.113.39").0]);
        assert_eq!(u.len(), 1, "a union merges across its parts");
    }

    #[test]
    fn a_range_written_backwards_is_the_same_range() {
        let (f, _) = IpFilter::parse("203.0.113.20 - 203.0.113.10");
        assert!(f.contains(ip("203.0.113.15")));
    }

    /// Big enough to be a real list: a quarter of a million ranges, checked
    /// by bisection, with the answers a linear scan would give.
    #[test]
    fn a_level1_sized_list_answers_like_a_linear_scan() {
        let mut text = String::new();
        for i in 0..250_000u32 {
            let base = i * 16_000;
            text.push_str(&format!("{}-{}\n", Ipv4Addr::from(base), Ipv4Addr::from(base + 7_000)));
        }
        let (f, st) = IpFilter::parse(&text);
        assert_eq!(st.ranges, 250_000);
        for probe in [0u32, 6_999, 7_000, 7_001, 15_999, 16_000, 3_999_999_999, 3_999_993_001] {
            let linear = (0..250_000u32).any(|i| probe >= i * 16_000 && probe <= i * 16_000 + 7_000);
            assert_eq!(f.contains(IpAddr::V4(Ipv4Addr::from(probe))), linear, "{probe}");
        }
    }

    /// Installing turns the check on, an empty list turns it off, and each
    /// install moves the generation sessions watch.
    #[test]
    fn install_switches_the_global_check() {
        let before = generation();
        install(Some(IpFilter::parse("203.0.113.0/24").0));
        assert!(blocked(ip("203.0.113.9")));
        assert!(!blocked(ip("198.51.100.9")));
        assert!(generation() > before);
        install(Some(IpFilter::default()));
        assert!(!blocked(ip("203.0.113.9")), "an empty list filters nothing");
        install(None);
        assert!(!blocked(ip("203.0.113.9")));
    }
}

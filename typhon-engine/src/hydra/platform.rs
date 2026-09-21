//! The seven system primitives that differ between Unix and Windows.
//!
//! Each one keeps its Unix implementation byte for byte: the Linux daemon is
//! what runs in production, and a port is not a reason to change what it does.
//! The Windows side is written to the same *meaning*, not to the same call --
//! `dev_t` and an NTFS volume serial are not the same number, they are the same
//! question ("are these two paths on one filesystem?").

use std::path::Path;

// ---------------------------------------------------------------------------
// Volume identity
// ---------------------------------------------------------------------------

/// An opaque id shared by every path on one filesystem.
///
/// Only ever compared for equality -- never stored, never shown. On Unix that
/// is `st_dev`; on Windows, the volume serial number of the mount that holds
/// the path.
#[cfg(unix)]
pub fn volume_id(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

#[cfg(windows)]
pub fn volume_id(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    // ⚠ Existence first. GetVolumePathNameW is a LEXICAL operation: handed a
    // path that does not exist it still succeeds, walking up to the current
    // drive root and returning its serial. The Unix side goes through
    // metadata() and answers None for a path that is not there, and callers
    // lean on that -- volume_id_nearest walks up precisely because None means
    // "not here yet", and same_volume() answers false on None, which is what
    // makes a move fall back to copy instead of rename.
    std::fs::metadata(p).ok()?;
    let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    wide.push(0);
    // The mount root first: a serial can only be asked of a volume root, and
    // "C:\\Users\\x" is not one.
    let mut root = [0u16; 260];
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }
    let mut serial: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(serial as u64)
}

/// Volume of the nearest existing ancestor.
///
/// A save path can point at a directory that has not been created yet; that is
/// not a reason to lose the torrent from its volume.
pub fn volume_id_nearest(p: &Path) -> Option<u64> {
    let mut cur = p;
    loop {
        if let Some(id) = volume_id(cur) {
            return Some(id);
        }
        cur = cur.parent()?;
    }
}

/// Are these two paths on the same filesystem?
pub fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume_id_nearest(a), volume_id_nearest(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Hard links
// ---------------------------------------------------------------------------

/// How many names this file has on disk.
///
/// ⚠ A wrong answer here is destructive, which is why Windows gets a real
/// implementation rather than a `1`. A file with more than one name is
/// hardlinked into the media library; copying and unlinking it would break
/// that link and double the space it takes. Answering "1" for a file that
/// actually has two would silently turn a safe rename into exactly that.
/// Unreadable metadata answers 1 on both platforms -- the historical Unix
/// behaviour, and the conservative one, since 1 forbids nothing.
#[cfg(unix)]
pub fn link_count(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map(|m| m.nlink()).unwrap_or(1)
}

#[cfg(windows)]
pub fn link_count(p: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    wide.push(0);
    // No access rights requested (0): the link count lives in metadata, and
    // asking for READ would fail on a file another process holds exclusively.
    // BACKUP_SEMANTICS so a directory can be opened too.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return 1;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return 1;
    }
    info.nNumberOfLinks.max(1) as u64
}

// ---------------------------------------------------------------------------
// Free space
// ---------------------------------------------------------------------------

/// Bytes used, total and available on the filesystem holding `path`.
///
/// Used is what the filesystem counts as taken, NOT total minus available: the
/// reserved blocks are neither available to us nor used by us, and counting
/// them as used would drain a disk that is not full.
#[cfg(unix)]
pub fn usage(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    let used = (stat.f_blocks as u64 - stat.f_bfree as u64) * block;
    let free = stat.f_bavail as u64 * block;
    Some((used, total, free))
}

#[cfg(windows)]
pub fn usage(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // `avail` is what THIS user may write (quotas apply); `free` is the whole
    // volume's free space. The Unix pair is f_bavail / f_bfree and lines up:
    // available drives the decisions, free drives the "used" figure.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok == 0 {
        return None;
    }
    let used = total.saturating_sub(free);
    Some((used, total, avail))
}

/// Available bytes on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    usage(path).map(|(_, _, free)| free)
}

// ---------------------------------------------------------------------------
// Page size and resident set
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub fn page_size() -> u64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

#[cfg(windows)]
pub fn page_size() -> u64 {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as u64
}

/// Resident set size of this process, in bytes.
#[cfg(target_os = "linux")]
pub fn resident_bytes() -> Option<u64> {
    // From statm, whose second field is the resident page count. Not from
    // `VmRSS` in status: same number, more parsing.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size())
}

#[cfg(windows)]
pub fn resident_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok == 0 {
        return None;
    }
    // WorkingSetSize is the Windows name for the resident set.
    Some(pmc.WorkingSetSize as u64)
}

/// No /proc and no Win32: the caller treats None as "unknown", not as zero.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn resident_bytes() -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Local date
// ---------------------------------------------------------------------------

/// Today's date in the daemon's local zone, `YYYY-MM-DD`.
///
/// Local, not UTC: the operator's day ends at midnight where they are, and the
/// container is given TZ=Europe/Paris for exactly this.
#[cfg(unix)]
pub fn local_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now as *const i64, &mut tm) };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

#[cfg(windows)]
pub fn local_date() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

// ---------------------------------------------------------------------------
// Network filesystems
// ---------------------------------------------------------------------------

/// Is this path on a network filesystem?
///
/// SQLite's WAL needs shared memory the protocol cannot provide, so a database
/// on one of these has to be handled differently.
#[cfg(target_os = "linux")]
pub fn is_network_fs(path: &Path) -> bool {
    const NFS: i64 = 0x6969;
    const SMB: i64 = 0x517B;
    const CIFS: i64 = 0xFF534D42u32 as i64;
    const SMB2: i64 = 0xFE534D42u32 as i64;
    const FUSE: i64 = 0x65735546;

    let Ok(c_path) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    matches!(st.f_type as i64, NFS | SMB | CIFS | SMB2 | FUSE)
}

#[cfg(windows)]
pub fn is_network_fs(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    const DRIVE_REMOTE: u32 = 4;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // A UNC path (\\server\share) has no drive letter; GetDriveTypeW answers
    // DRIVE_NO_ROOT_DIR for it, so test the prefix directly first.
    if path.to_string_lossy().starts_with(r"\\") {
        return true;
    }
    let mut root = [0u16; 260];
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return false;
    }
    unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn is_network_fs(_path: &Path) -> bool {
    false
}

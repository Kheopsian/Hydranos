//go:build !windows

package fsinfo

import (
	"fmt"
	"net"
	"os"
	"path/filepath"
	"syscall"
)

// Superblock magics for the filesystems that break SQLite locking and cannot
// host a unix socket. Values from the kernel's magic.h. Deliberately a
// whitelist of known-bad families rather than a "not in known-good list"
// heuristic: an unknown local filesystem must not be demoted.
// FUSE is deliberately absent. It is not a filesystem but a transport: sshfs
// and rclone belong here, yet so does Unraid's shfs (/mnt/user), which handles
// POSIX locks and unix sockets perfectly well. Demoting every Unraid install
// that keeps appdata on /mnt/user would be a far worse bug than the one this
// guards against — the socket probe below catches the genuinely broken ones.
var netMagic = map[int64]Kind{
	0xFF534D42: "CIFS/SMB",
	0xFE534D42: "SMB2",
	0x6969:     "NFS",
	0x01021997: "9p",
	0x5346414F: "AFS",
	0x00C36400: "CephFS",
}

// detect asks the cheap question first (is this a filesystem family we know
// cannot do this?) and, failing that, the expensive but honest one: can we
// actually bind a unix socket here? SQLite's WAL and our engine sockets need
// the same underlying capabilities, so a share that refuses the socket is a
// share that will refuse the WAL.
func detect(path string) Kind {
	p := nearestExisting(path)
	if p == "" {
		return ""
	}
	var st syscall.Statfs_t
	if err := syscall.Statfs(p, &st); err != nil {
		return ""
	}
	if k, ok := netMagic[int64(st.Type)]; ok {
		return k
	}
	// The probe needs a directory to put its socket in, and callers ask about
	// files as often as directories -- store.Open asks about data_dir/hydra.db.
	// On an install that already has that file, nearestExisting returns the
	// file itself, and joining a socket name onto a file gives ENOTDIR. That is
	// not a share refusing a socket, it is us probing the wrong thing, so ask
	// the directory that would actually hold the socket.
	if fi, err := os.Stat(p); err == nil && !fi.IsDir() {
		p = filepath.Dir(p)
	}
	if !canBindUnixSocket(p) {
		return "remote/limited filesystem"
	}
	return ""
}

// canBindUnixSocket is the capability probe. A failure to create the socket
// file itself (as opposed to a permissions problem) means the backing store is
// not a real local filesystem.
func canBindUnixSocket(dir string) bool {
	// Socket paths are capped at ~108 bytes by sockaddr_un, so a deep data_dir
	// would fail the probe for a reason that has nothing to do with the share.
	name := filepath.Join(dir, fmt.Sprintf(".hydra-probe-%d.sock", os.Getpid()))
	if len(name) > 100 {
		return true // cannot tell — assume capable rather than demote wrongly
	}
	_ = os.Remove(name)
	l, err := net.Listen("unix", name)
	if err != nil {
		return false
	}
	l.Close()
	_ = os.Remove(name)
	return true
}

// nearestExisting walks up until it finds a path that exists, so a data_dir
// that has not been created yet still reports the filesystem it will land on.
func nearestExisting(path string) string {
	p, err := filepath.Abs(path)
	if err != nil {
		return ""
	}
	for {
		if _, err := os.Stat(p); err == nil {
			return p
		}
		parent := filepath.Dir(p)
		if parent == p {
			return ""
		}
		p = parent
	}
}

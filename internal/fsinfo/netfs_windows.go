//go:build windows

package fsinfo

import (
	"path/filepath"
	"strings"
	"syscall"
	"unsafe"
)

const driveRemote = 4 // DRIVE_REMOTE

// detect flags UNC paths (\\server\share) directly and asks the OS about
// mapped drive letters, which is the same question by another spelling.
func detect(path string) Kind {
	p, err := filepath.Abs(path)
	if err != nil {
		return ""
	}
	if strings.HasPrefix(p, `\\`) {
		return "SMB (UNC)"
	}
	root := filepath.VolumeName(p) + `\`
	rp, err := syscall.UTF16PtrFromString(root)
	if err != nil {
		return ""
	}
	proc := syscall.NewLazyDLL("kernel32.dll").NewProc("GetDriveTypeW")
	r, _, _ := proc.Call(uintptr(unsafe.Pointer(rp)))
	if r == driveRemote {
		return "network drive"
	}
	return ""
}

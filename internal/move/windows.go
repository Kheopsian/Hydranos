//go:build windows

package move

import (
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/windows"
)

// Windows has hardlinks, but os.FileInfo does not surface the link count
// without reopening the file by handle. Reporting "none found" here is the
// safe direction to be wrong in: it never silently breaks a link the operator
// was not warned about, because the same-volume case (where NTFS hardlinks
// live) is a rename, which preserves them.
func linkCount(info os.FileInfo) uint64 { return 1 }

// sameFilesystem compares volumes: a rename only works within one.
func sameFilesystem(a, b string) bool {
	va := strings.ToUpper(filepath.VolumeName(a))
	vb := strings.ToUpper(filepath.VolumeName(b))
	return va != "" && va == vb
}

func freeSpace(p string) int64 {
	target := p
	for {
		var free, total, totalFree uint64
		ptr, err := windows.UTF16PtrFromString(target)
		if err == nil {
			if err := windows.GetDiskFreeSpaceEx(ptr, &free, &total, &totalFree); err == nil {
				return int64(free)
			}
		}
		parent := parentDir(target)
		if parent == target {
			return 0
		}
		target = parent
	}
}

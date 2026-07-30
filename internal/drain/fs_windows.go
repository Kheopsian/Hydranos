//go:build windows

package drain

import (
	"syscall"
	"unsafe"
)

// fsTotalFree returns total and available bytes via GetDiskFreeSpaceExW.
func fsTotalFree(path string) (total, free int64, err error) {
	p, err := syscall.UTF16PtrFromString(path)
	if err != nil {
		return 0, 0, err
	}
	proc := syscall.NewLazyDLL("kernel32.dll").NewProc("GetDiskFreeSpaceExW")
	var freeToCaller, totalBytes, totalFree uint64
	r, _, e := proc.Call(
		uintptr(unsafe.Pointer(p)),
		uintptr(unsafe.Pointer(&freeToCaller)),
		uintptr(unsafe.Pointer(&totalBytes)),
		uintptr(unsafe.Pointer(&totalFree)),
	)
	if r == 0 {
		return 0, 0, e
	}
	return int64(totalBytes), int64(freeToCaller), nil
}

//go:build windows

package engine

import (
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"

	"golang.org/x/sys/windows"
)

const engineBinaryName = "hydra-engine.exe"
const engineBinaryDefault = "hydra-engine.exe"

// statDev returns a volume-level id (Windows has no dev/inode). Same-volume
// paths share an id so the cross-filesystem hardlink-move guard still works at
// volume granularity; derived from the volume name, never errors.
func statDev(p string) (uint64, error) {
	vol := strings.ToUpper(filepath.VolumeName(filepath.Clean(p)))
	var h uint64 = 1469598103934665603 // FNV-1a offset basis
	for _, c := range vol {
		h ^= uint64(c)
		h *= 1099511628211
	}
	return h, nil
}

// applyFwmark is a no-op on Windows: VPN binding is done by the system VPN
// client (whole-network / per-app), not per-socket fwmark (no netfilter).
func applyFwmark(d *net.Dialer, fwmark int) {}

// bindToDeviceSupported is false here: Windows has no SO_BINDTODEVICE, so a
// bind_interface is honoured by pinning the source address only.
const bindToDeviceSupported = false

// applyEgressControl is a no-op on Windows for the same reason applyFwmark is.
func applyEgressControl(d *net.Dialer, iface string, fwmark int) {}

// signalHeapDump is a no-op on Windows (no jemalloc SIGUSR1 handler).
func signalHeapDump(pid int) {}

// selfSIGTERM exits the process; a Windows service manager restarts it.
func selfSIGTERM() { os.Exit(1) }

// procStat reports whether the engine process is alive. Windows has no /proc,
// so liveness is checked via OpenProcess + GetExitCodeProcess (STILL_ACTIVE).
// RSS is not read here (the watchdog's RSS ceiling is a Linux OOM-prevention
// feature); returning 0 disables that check on Windows.
func procStat(pid int) (bool, int64) {
	if pid <= 0 {
		return false, 0
	}
	h, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		return false, 0 // process gone / not queryable
	}
	defer windows.CloseHandle(h)
	var code uint32
	if err := windows.GetExitCodeProcess(h, &code); err != nil {
		return false, 0
	}
	const stillActive = 259 // STILL_ACTIVE
	return code == stillActive, 0
}

// hideChildWindow keeps the engine subprocess from opening a console window of
// its own. hydra.exe is a GUI-subsystem binary, so it usually has no console
// to lend the child -- and a console app started without one gets a brand new
// window, which is exactly the cmd.exe box we set out to remove.
func hideChildWindow(cmd *exec.Cmd) {
	const createNoWindow = 0x08000000
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.CreationFlags |= createNoWindow
}

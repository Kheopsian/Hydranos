//go:build windows

package main

import (
	"os"
	"strings"

	"golang.org/x/sys/windows"
)

// Hydra links as a GUI-subsystem binary on Windows (-H windowsgui), so a
// double-click no longer parks a cmd.exe window on the desktop. That is the
// whole point -- but it would also silence the app for someone who launched it
// from a terminal on purpose.
//
// So instead of a --no-cmd flag the user has to know about, the behaviour is
// deduced from how Hydra was started: attach to the console we were launched
// from, if there is one. Run from PowerShell, you get the banner and the log
// stream in that window as before; double-click, you get nothing but the tray
// icon. --console forces a fresh console for the cases in between (started by
// a shortcut, by a scheduler, or while debugging).
//
// Nothing is lost either way: every line also goes to hydra.log next to the
// config, and to the UI's Logs tab.
func init() {
	const attachParentProcess = ^uint32(0) // (DWORD)-1

	r, _, _ := procAttachConsole.Call(uintptr(attachParentProcess))
	attached := r != 0
	if !attached && wantsConsole() {
		if r, _, _ := procAllocConsole.Call(); r != 0 {
			attached = true
		}
	}
	if !attached {
		return
	}
	bindStdHandles()
}

// x/sys/windows does not wrap the console-attach calls, so bind them directly.
var (
	modkernel32       = windows.NewLazySystemDLL("kernel32.dll")
	procAttachConsole = modkernel32.NewProc("AttachConsole")
	procAllocConsole  = modkernel32.NewProc("AllocConsole")
)

func wantsConsole() bool {
	for _, a := range os.Args[1:] {
		if a == "--console" || a == "-console" {
			return true
		}
		// Stop at the first non-flag so a path that happens to be named
		// "--console" as a subcommand argument is not mistaken for the flag.
		if !strings.HasPrefix(a, "-") {
			break
		}
	}
	return false
}

// bindStdHandles points the process's standard handles at the console we just
// attached to. A GUI-subsystem process starts with no valid std handles, and
// AttachConsole does not set them -- without this, every write to os.Stdout
// goes nowhere and the console stays empty.
func bindStdHandles() {
	out, err := openCon("CONOUT$", windows.GENERIC_READ|windows.GENERIC_WRITE)
	if err == nil {
		windows.SetStdHandle(windows.STD_OUTPUT_HANDLE, out)
		windows.SetStdHandle(windows.STD_ERROR_HANDLE, out)
		os.Stdout = os.NewFile(uintptr(out), "CONOUT$")
		os.Stderr = os.NewFile(uintptr(out), "CONOUT$")
	}
	if in, err := openCon("CONIN$", windows.GENERIC_READ|windows.GENERIC_WRITE); err == nil {
		windows.SetStdHandle(windows.STD_INPUT_HANDLE, in)
		os.Stdin = os.NewFile(uintptr(in), "CONIN$")
	}
}

func openCon(name string, access uint32) (windows.Handle, error) {
	return windows.CreateFile(
		windows.StringToUTF16Ptr(name),
		access,
		windows.FILE_SHARE_READ|windows.FILE_SHARE_WRITE,
		nil,
		windows.OPEN_EXISTING,
		0,
		0,
	)
}

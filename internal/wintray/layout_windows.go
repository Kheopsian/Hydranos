//go:build windows && (amd64 || arm64)

package wintray

import "unsafe"

// Shell_NotifyIcon dispatches on cbSize: it reads the struct as whichever
// NOTIFYICONDATAW version matches that number, and on a mismatch it fails --
// silently, with no icon and no error worth the name. We pass
// unsafe.Sizeof(notifyIconData{}), so the Go struct's layout has to land
// exactly on the Win32 one.
//
// 976 is sizeof(NOTIFYICONDATAW) for the Vista+ (V4) shape on 64-bit Windows.
// This constant is a compile-time assertion: if a field is ever added,
// reordered or resized, the subtraction underflows an unsigned constant and
// the build stops here instead of shipping an invisible tray icon.
const _ = uint(unsafe.Sizeof(notifyIconData{}) - 976)
const _ = uint(976 - unsafe.Sizeof(notifyIconData{}))

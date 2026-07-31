//go:build windows

package logs

import (
	"os"

	"golang.org/x/sys/windows"
)

// termWidth returns the console width in columns, or 0 if it cannot be read.
// It queries the real console buffer (CONOUT$) first so the width is detected
// even when stdout is redirected/piped; falls back to stdout's handle.
func termWidth() int {
	if h, err := windows.CreateFile(
		windows.StringToUTF16Ptr("CONOUT$"),
		windows.GENERIC_READ|windows.GENERIC_WRITE,
		windows.FILE_SHARE_READ|windows.FILE_SHARE_WRITE,
		nil, windows.OPEN_EXISTING, 0, 0,
	); err == nil {
		defer windows.CloseHandle(h)
		var info windows.ConsoleScreenBufferInfo
		if windows.GetConsoleScreenBufferInfo(h, &info) == nil {
			return int(info.Window.Right - info.Window.Left + 1)
		}
	}
	var info windows.ConsoleScreenBufferInfo
	if err := windows.GetConsoleScreenBufferInfo(windows.Handle(os.Stdout.Fd()), &info); err != nil {
		return 0
	}
	return int(info.Window.Right - info.Window.Left + 1)
}

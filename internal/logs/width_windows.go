//go:build windows

package logs

import (
	"os"

	"golang.org/x/sys/windows"
)

// termWidth returns the console width in columns, or 0 if it cannot be read
// (e.g. output is redirected).
func termWidth() int {
	var info windows.ConsoleScreenBufferInfo
	if err := windows.GetConsoleScreenBufferInfo(windows.Handle(os.Stdout.Fd()), &info); err != nil {
		return 0
	}
	return int(info.Window.Right - info.Window.Left + 1)
}

//go:build !windows

package logs

import (
	"os"

	"golang.org/x/sys/unix"
)

// termWidth returns the terminal width in columns, or 0 if it cannot be read
// (e.g. output is redirected / not a tty).
func termWidth() int {
	ws, err := unix.IoctlGetWinsize(int(os.Stdout.Fd()), unix.TIOCGWINSZ)
	if err != nil || ws == nil {
		return 0
	}
	return int(ws.Col)
}

//go:build windows

package api

import "os"

// selfTerminate exits the process; a Windows service manager restarts it.
func selfTerminate() { os.Exit(1) }

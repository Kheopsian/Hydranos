//go:build !windows

package main

// startTray is a no-op off Windows: there is a console, Ctrl+C works and
// SIGTERM is delivered, so the shutdown path needs no help. Returning a nil
// channel makes the caller's select ignore this case entirely (a receive on a
// nil channel blocks forever).
func startTray(url, version string, stats func() (up, down float64, torrents int)) (<-chan struct{}, func()) {
	return nil, func() {}
}

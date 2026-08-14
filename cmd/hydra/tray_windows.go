//go:build windows

package main

import (
	"log/slog"

	hydra "github.com/Kheopsian/hydra"
	"github.com/Kheopsian/hydra/internal/wintray"
)

// startTray puts Hydra in the notification area and returns a channel that
// closes when the user picks "Quit".
//
// On Windows the tray is not decoration: the GUI-subsystem build has no
// console, so there is no Ctrl+C, and SIGTERM is never delivered. The menu's
// Quit is the only clean shutdown path -- the one that reaches the resume
// flush. If the tray fails to come up we say so loudly and keep running, but
// that instance can then only be stopped the hard way.
func startTray(url, version string, stats func() (up, down float64, torrents int)) (<-chan struct{}, func()) {
	icon, err := hydra.WebAssets.ReadFile("web/static/hydra-logo.png")
	if err != nil {
		slog.Warn("tray: logo not found in embedded assets, using the stock icon", "error", err)
	}

	t, err := wintray.Start(wintray.Config{
		URL:     url,
		Version: version,
		IconPNG: icon,
		Stats: func() wintray.Stats {
			if stats == nil {
				return wintray.Stats{}
			}
			up, down, n := stats()
			return wintray.Stats{Up: up, Down: down, Active: n}
		},
	})
	if err != nil {
		slog.Error("tray: could not create the notification-area icon; "+
			"Hydra is running but has no visible control and no clean stop "+
			"(use --console, or stop it from Task Manager)", "error", err)
		return nil, func() {}
	}
	slog.Info("Notification-area icon ready", "url", url)
	return t.Done(), t.Stop
}

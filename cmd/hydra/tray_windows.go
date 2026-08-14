//go:build windows

package main

import (
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"syscall"

	hydra "github.com/Kheopsian/hydra"
	version_pkg "github.com/Kheopsian/hydra/internal/version"
	"github.com/Kheopsian/hydra/internal/wintray"
	"golang.org/x/sys/windows"
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
		OnUpdate: startUpdater,
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

// startUpdater launches hydra-update.exe and leaves it to get on with it.
//
// The updater is a separate executable for one reason: Windows locks a running
// image, so hydra.exe cannot overwrite itself. It is handed our PID, and when
// it is ready to swap the files it closes the tray window -- which runs the
// ordinary Quit path, the only one on Windows that flushes resume data -- then
// waits for this process to actually exit. Hydra is therefore never stopped
// for an update that turns out not to exist or that the user declines.
func startUpdater() {
	self, err := os.Executable()
	if err != nil {
		slog.Error("update: cannot locate the running executable", "error", err)
		messageBox("Hydra could not work out where it is installed, so it cannot update itself.")
		return
	}
	dir := filepath.Dir(self)
	updater := filepath.Join(dir, "hydra-update.exe")
	if _, err := os.Stat(updater); err != nil {
		slog.Error("update: hydra-update.exe is missing", "path", updater, "error", err)
		messageBox("hydra-update.exe was not found next to Hydra.\n\n" +
			"It ships in the same archive; download the release again, or update by " +
			"replacing hydra.exe and hydra-engine.exe by hand.")
		return
	}

	cmd := exec.Command(updater,
		"--pid", strconv.Itoa(os.Getpid()),
		"--dir", dir,
		"--current", version_pkg.Version,
	)
	cmd.Dir = dir
	// Without this a console child of a console-less parent creates a console
	// of its own, which is the cmd window the GUI-subsystem build exists to
	// avoid. Same reasoning as the engine subprocess.
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: windows.CREATE_NO_WINDOW}
	if err := cmd.Start(); err != nil {
		slog.Error("update: could not start hydra-update.exe", "error", err)
		messageBox("Hydra could not start the updater.\n\n" + err.Error())
		return
	}
	slog.Info("update: updater started; it will close Hydra if it has something to install", "pid", cmd.Process.Pid)
}

// messageBox puts a message on screen. There is no console in this build, so a
// message box is the only way to explain a failure the user asked for.
func messageBox(text string) {
	t, _ := windows.UTF16PtrFromString(text)
	c, _ := windows.UTF16PtrFromString("Hydra")
	// MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST
	windows.MessageBox(0, t, c, 0x30|0x00010000|0x00040000)
}

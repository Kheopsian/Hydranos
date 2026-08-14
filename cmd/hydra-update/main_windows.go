//go:build windows

// hydra-update replaces hydra.exe and hydra-engine.exe in place with the
// newest published release.
//
// It exists because a running .exe holds a lock on its own file: Hydra cannot
// overwrite itself. So the tray spawns this, then quits through the menu path
// -- the only one on Windows that flushes resume data, since a GUI-subsystem
// build gets neither Ctrl+C nor SIGTERM. We wait for that process to actually
// die before touching anything.
//
// Everything the user sees is a message box: this is a GUI-subsystem binary
// with no console to print to.
package main

import (
	"archive/zip"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
)

const (
	// Must match what internal/wintray registers, and it is a message-only
	// window: HWND_MESSAGE is its parent, -3 as an unsigned value.
	trayWindowClass = "HydraTrayWindow"
	hwndMessage     = ^uintptr(2)
	wmClose         = 0x0010

	releasesURL = "https://api.github.com/repos/Kheopsian/Hydra/releases/latest"
	// Long enough for a slow mirror, short enough that a hung request does not
	// leave the user staring at nothing after they already quit Hydra.
	httpTimeout = 5 * time.Minute
)

// The files we own. Anything else in the archive (README) is left alone:
// replacing a document is not worth the chance of clobbering a local edit.
//
// required must be present or the update is refused: a new front end against an
// old engine is the outcome worth the most effort to avoid.
var required = []string{"hydra.exe", "hydra-engine.exe"}

// optional is replaced when the archive carries it and skipped when it does
// not, which is what lets us update ourselves. Windows locks a running image
// against writes but still allows renaming it, and swap renames before it
// copies -- so this executable can be replaced while it is the one doing the
// replacing. Without this a bug in the updater could only ever be fixed by
// downloading the whole release by hand. Releases before v3.65.0 have no
// updater in them, hence optional rather than required.
var optional = []string{"hydra-update.exe"}

func main() {
	var pid int
	var dir, current string
	flag.IntVar(&pid, "pid", 0, "wait for this process to exit before replacing files")
	flag.StringVar(&dir, "dir", "", "install directory (default: this executable's own)")
	flag.StringVar(&current, "current", "", "currently running version, to compare against the latest")
	flag.Parse()

	if dir == "" {
		self, err := os.Executable()
		if err != nil {
			fatal("Cannot work out where Hydra is installed.\n\n" + err.Error())
		}
		dir = filepath.Dir(self)
	}

	rel, err := latest()
	if err != nil {
		fatal("Could not check for updates.\n\n" + err.Error())
	}
	latestVer := strings.TrimPrefix(rel.TagName, "v")

	if current != "" && !newer(latestVer, current) {
		info(fmt.Sprintf("Hydra %s is already the latest version.", clean(current)))
		return
	}
	if !confirm(fmt.Sprintf("Hydra %s is available (you have %s).\n\nDownload it and restart Hydra?",
		latestVer, clean(current))) {
		return
	}

	zipAsset, shaAsset := pickAssets(rel)
	if zipAsset == "" {
		fatal("Release " + rel.TagName + " has no Windows archive.\n\n" +
			"It may still be publishing; try again in a few minutes.")
	}

	tmp, err := os.MkdirTemp("", "hydra-update-")
	if err != nil {
		fatal("Cannot create a temporary folder.\n\n" + err.Error())
	}
	defer os.RemoveAll(tmp)

	archive := filepath.Join(tmp, "hydra.zip")
	if err := download(zipAsset, archive); err != nil {
		fatal("Download failed.\n\n" + err.Error())
	}

	// Verify before anything is unpacked. A truncated download is the ordinary
	// failure here, and it would otherwise surface as a corrupt exe that only
	// fails at the next start, long after this window is gone.
	if shaAsset != "" {
		if err := verify(archive, shaAsset); err != nil {
			fatal("The download does not match its published checksum, so it was discarded.\n\n" + err.Error())
		}
	}

	staged := filepath.Join(tmp, "staged")
	if err := unpack(archive, staged); err != nil {
		fatal("Could not unpack the archive.\n\n" + err.Error())
	}
	for _, name := range required {
		if _, err := os.Stat(filepath.Join(staged, name)); err != nil {
			fatal("The archive is missing " + name + ", so nothing was changed.")
		}
	}

	// Only now is Hydra asked to stop: everything above can fail without having
	// interrupted anything. Closing the tray window runs the same path as the
	// menu's Quit, which is the only one on Windows that flushes resume data --
	// killing the process here would cost the user a re-check of every torrent
	// that had moved since the last save.
	if pid > 0 {
		if !stopHydra() {
			fatal("Hydra did not respond to the request to close.\n\n" +
				"Quit it from the notification area, then run this again.")
		}
		if err := waitForExit(pid, 2*time.Minute); err != nil {
			fatal("Hydra is still running, so its files cannot be replaced.\n\n" +
				"Quit it from the notification area and run this again.")
		}
	}

	if err := swap(staged, dir); err != nil {
		fatal("Update failed and the previous version was put back.\n\n" + err.Error())
	}

	if err := launch(filepath.Join(dir, "hydra.exe")); err != nil {
		info("Hydra was updated to " + latestVer + ", but could not be started again.\n\n" +
			"Start it yourself from " + dir + ".")
		return
	}
	info("Hydra was updated to " + latestVer + " and restarted.")
}

func latest() (*release, error) {
	c := &http.Client{Timeout: httpTimeout}
	req, err := http.NewRequest("GET", releasesURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("User-Agent", "hydra-update")
	resp, err := c.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("GitHub answered %s", resp.Status)
	}
	var r release
	if err := json.NewDecoder(resp.Body).Decode(&r); err != nil {
		return nil, err
	}
	return &r, nil
}

func download(url, dest string) error {
	c := &http.Client{Timeout: httpTimeout}
	req, _ := http.NewRequest("GET", url, nil)
	req.Header.Set("User-Agent", "hydra-update")
	resp, err := c.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return fmt.Errorf("server answered %s", resp.Status)
	}
	f, err := os.Create(dest)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = io.Copy(f, resp.Body)
	return err
}

func verify(path, shaURL string) error {
	c := &http.Client{Timeout: httpTimeout}
	req, _ := http.NewRequest("GET", shaURL, nil)
	req.Header.Set("User-Agent", "hydra-update")
	resp, err := c.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 4096))
	if err != nil {
		return err
	}
	// Format is "<hex>  <filename>", as produced by the release workflow.
	want := strings.ToLower(strings.TrimSpace(string(raw)))
	if i := strings.IndexAny(want, " \t"); i > 0 {
		want = want[:i]
	}
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return err
	}
	got := hex.EncodeToString(h.Sum(nil))
	if got != want {
		return fmt.Errorf("expected %s, got %s", want, got)
	}
	return nil
}

// unpack flattens the archive: the published zip wraps everything in a
// versioned folder, and we want the files themselves.
func unpack(archive, dest string) error {
	r, err := zip.OpenReader(archive)
	if err != nil {
		return err
	}
	defer r.Close()
	if err := os.MkdirAll(dest, 0o755); err != nil {
		return err
	}
	for _, f := range r.File {
		if f.FileInfo().IsDir() {
			continue
		}
		name := filepath.Base(f.Name)
		rc, err := f.Open()
		if err != nil {
			return err
		}
		out, err := os.Create(filepath.Join(dest, name))
		if err != nil {
			rc.Close()
			return err
		}
		_, err = io.Copy(out, rc)
		rc.Close()
		out.Close()
		if err != nil {
			return err
		}
	}
	return nil
}

// swap moves the new binaries into place, keeping the old ones aside until all
// of them have landed. A half-applied update -- a new front end against an old
// engine -- is the one outcome worth real effort to avoid.
func swap(staged, dir string) error {
	var moved []string
	restore := func() {
		for _, name := range moved {
			live := filepath.Join(dir, name)
			os.Remove(live)
			os.Rename(live+".old", live)
		}
	}
	todo := append([]string{}, required...)
	for _, name := range optional {
		if _, err := os.Stat(filepath.Join(staged, name)); err == nil {
			todo = append(todo, name)
		}
	}
	for _, name := range todo {
		live := filepath.Join(dir, name)
		if _, err := os.Stat(live); err == nil {
			if err := os.Rename(live, live+".old"); err != nil {
				restore()
				return fmt.Errorf("cannot set aside %s: %w", name, err)
			}
		}
		moved = append(moved, name)
		if err := copyFile(filepath.Join(staged, name), live); err != nil {
			restore()
			return fmt.Errorf("cannot install %s: %w", name, err)
		}
	}
	// Best effort, and expected to fail for our own .old: this process still
	// has that image open. A leftover .old is harmless, and on a locked file we
	// would rather finish the update than fail it.
	for _, name := range moved {
		os.Remove(filepath.Join(dir, name+".old"))
	}
	return nil
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o755)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

// stopHydra asks the running daemon to shut down the clean way, by closing the
// tray's window. That window is message-only, so it is invisible to a plain
// FindWindow and has to be looked for under HWND_MESSAGE specifically.
func stopHydra() bool {
	class, err := windows.UTF16PtrFromString(trayWindowClass)
	if err != nil {
		return false
	}
	user32 := windows.NewLazySystemDLL("user32.dll")
	findWindowEx := user32.NewProc("FindWindowExW")
	postMessage := user32.NewProc("PostMessageW")
	hwnd, _, _ := findWindowEx.Call(hwndMessage, 0, uintptr(unsafe.Pointer(class)), 0)
	if hwnd == 0 {
		// No window: either Hydra already stopped, or this is a bare install
		// with no tray. Either way there is nothing to wait for and nothing to
		// ask, so let the caller carry on.
		return true
	}
	r, _, _ := postMessage.Call(hwnd, wmClose, 0, 0)
	return r != 0
}

// waitForExit blocks on the process handle rather than polling: a PID can be
// recycled, and a poll loop would happily declare victory on a new process
// that inherited the number.
func waitForExit(pid int, timeout time.Duration) error {
	h, err := windows.OpenProcess(windows.SYNCHRONIZE, false, uint32(pid))
	if err != nil {
		return nil // already gone
	}
	defer windows.CloseHandle(h)
	ev, err := windows.WaitForSingleObject(h, uint32(timeout.Milliseconds()))
	if err != nil {
		return err
	}
	if ev != windows.WAIT_OBJECT_0 {
		return fmt.Errorf("still running after %s", timeout)
	}
	return nil
}

func launch(exe string) error {
	cmd := exec.Command(exe)
	cmd.Dir = filepath.Dir(exe)
	return cmd.Start()
}

const (
	mbOK            = 0x00000000
	mbOKCancel      = 0x00000001
	mbIconError     = 0x00000010
	mbIconQuestion  = 0x00000020
	mbIconInfo      = 0x00000040
	mbSetForeground = 0x00010000
	mbTopmost       = 0x00040000
	idOK            = 1
	messageBoxTitle = "Hydra Update"
)

func box(text string, flags uint32) int32 {
	t, _ := windows.UTF16PtrFromString(text)
	c, _ := windows.UTF16PtrFromString(messageBoxTitle)
	r, _ := windows.MessageBox(0, t, c, flags|mbSetForeground|mbTopmost)
	return int32(r)
}

func info(text string)         { box(text, mbOK|mbIconInfo) }
func confirm(text string) bool { return box(text, mbOKCancel|mbIconQuestion) == idOK }
func fatal(text string) {
	box(text, mbOK|mbIconError)
	os.Exit(1)
}

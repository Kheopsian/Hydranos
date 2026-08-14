//go:build windows

// Package wintray puts Hydra in the Windows notification area.
//
// It exists because the Windows build has no console to speak through: the
// app is linked as a GUI-subsystem binary so a double-click does not leave a
// cmd.exe window sitting on the desktop. That removes the only place the user
// could see Hydra running -- and, more importantly, the only way to stop it
// cleanly (Ctrl+C). The tray icon is what gives both back: it is Hydra's
// visible presence, and its "Quit" is the replacement for Ctrl+C, so the
// shutdown path that flushes resume data is still reachable.
//
// Deliberately hand-rolled on golang.org/x/sys/windows (already vendored)
// rather than pulling a tray library: the whole surface is one hidden window,
// one icon and one popup menu, and a dependency here would be a cgo risk on a
// build that must stay CGO_ENABLED=0.
package wintray

import (
	"fmt"
	"runtime"
	"sync"
	"unsafe"

	"golang.org/x/sys/windows"
)

// Stats is the live snapshot shown in the tooltip.
type Stats struct {
	Up     float64 // bytes/s
	Down   float64 // bytes/s
	Active int     // torrents currently moving data
}

// Config wires the tray to the running daemon.
type Config struct {
	URL     string       // web UI, e.g. http://127.0.0.1:8199
	Version string       // shown in the menu header
	IconPNG []byte       // any size; scaled to the small-icon metric
	Stats   func() Stats // polled every tick for the tooltip; may be nil
	OnQuit  func()       // called once when the user picks Quit; may be nil
	// OnUpdate is called when the user picks "Check for updates". It only
	// starts the updater; it does not stop Hydra. The updater decides whether
	// there is anything to do, asks, downloads and verifies first, and closes
	// this window itself when it is finally ready to replace the files. That
	// ordering is the point: checking for an update you turn out not to want
	// must not leave Hydra stopped. May be nil.
	OnUpdate func()
}

const (
	trayCallbackMsg = 0x0400 + 1 // WM_APP+1
	timerID         = 1
	tooltipEveryMS  = 2000

	idOpen   = 1001
	idQuit   = 1002
	idUpdate = 1003
)

var (
	moduser32   = windows.NewLazySystemDLL("user32.dll")
	modshell32  = windows.NewLazySystemDLL("shell32.dll")
	modgdi32    = windows.NewLazySystemDLL("gdi32.dll")
	modkernel32 = windows.NewLazySystemDLL("kernel32.dll")

	procRegisterClassExW      = moduser32.NewProc("RegisterClassExW")
	procCreateWindowExW       = moduser32.NewProc("CreateWindowExW")
	procDefWindowProcW        = moduser32.NewProc("DefWindowProcW")
	procDestroyWindow         = moduser32.NewProc("DestroyWindow")
	procGetMessageW           = moduser32.NewProc("GetMessageW")
	procTranslateMessage      = moduser32.NewProc("TranslateMessage")
	procDispatchMessageW      = moduser32.NewProc("DispatchMessageW")
	procPostQuitMessage       = moduser32.NewProc("PostQuitMessage")
	procPostMessageW          = moduser32.NewProc("PostMessageW")
	procCreatePopupMenu       = moduser32.NewProc("CreatePopupMenu")
	procAppendMenuW           = moduser32.NewProc("AppendMenuW")
	procTrackPopupMenu        = moduser32.NewProc("TrackPopupMenu")
	procDestroyMenu           = moduser32.NewProc("DestroyMenu")
	procSetForegroundWindow   = moduser32.NewProc("SetForegroundWindow")
	procGetCursorPos          = moduser32.NewProc("GetCursorPos")
	procSetTimer              = moduser32.NewProc("SetTimer")
	procKillTimer             = moduser32.NewProc("KillTimer")
	procGetSystemMetrics      = moduser32.NewProc("GetSystemMetrics")
	procCreateIconIndirect    = moduser32.NewProc("CreateIconIndirect")
	procDestroyIcon           = moduser32.NewProc("DestroyIcon")
	procLoadIconW             = moduser32.NewProc("LoadIconW")
	procRegisterWindowMessage = moduser32.NewProc("RegisterWindowMessageW")

	procShellNotifyIconW = modshell32.NewProc("Shell_NotifyIconW")
	procShellExecuteW    = modshell32.NewProc("ShellExecuteW")

	procCreateDIBSection = modgdi32.NewProc("CreateDIBSection")
	procCreateBitmap     = modgdi32.NewProc("CreateBitmap")
	procDeleteObject     = modgdi32.NewProc("DeleteObject")

	procGetModuleHandleW = modkernel32.NewProc("GetModuleHandleW")
)

type point struct{ X, Y int32 }

type msg struct {
	HWnd     windows.HWND
	Message  uint32
	WParam   uintptr
	LParam   uintptr
	Time     uint32
	Pt       point
	LPrivate uint32
}

type wndClassEx struct {
	CbSize        uint32
	Style         uint32
	LpfnWndProc   uintptr
	CbClsExtra    int32
	CbWndExtra    int32
	HInstance     windows.Handle
	HIcon         windows.Handle
	HCursor       windows.Handle
	HbrBackground windows.Handle
	LpszMenuName  *uint16
	LpszClassName *uint16
	HIconSm       windows.Handle
}

// notifyIconData is NOTIFYICONDATAW. The Go compiler inserts the same padding
// MSVC does (a pointer-aligned field after CbSize), so unsafe.Sizeof is the
// correct cbSize for the Vista+ (V4) shape.
type notifyIconData struct {
	CbSize           uint32
	HWnd             windows.HWND
	UID              uint32
	UFlags           uint32
	UCallbackMessage uint32
	HIcon            windows.Handle
	SzTip            [128]uint16
	DwState          uint32
	DwStateMask      uint32
	SzInfo           [256]uint16
	UVersion         uint32
	SzInfoTitle      [64]uint16
	DwInfoFlags      uint32
	GuidItem         windows.GUID
	HBalloonIcon     windows.Handle
}

type iconInfo struct {
	FIcon    int32
	XHotspot uint32
	YHotspot uint32
	HbmMask  windows.Handle
	HbmColor windows.Handle
}

type bitmapInfoHeader struct {
	Size          uint32
	Width         int32
	Height        int32
	Planes        uint16
	BitCount      uint16
	Compression   uint32
	SizeImage     uint32
	XPelsPerMeter int32
	YPelsPerMeter int32
	ClrUsed       uint32
	ClrImportant  uint32
}

const (
	nimAdd    = 0
	nimModify = 1
	nimDelete = 2

	nifMessage = 0x01
	nifIcon    = 0x02
	nifTip     = 0x04

	wmDestroy     = 0x0002
	wmClose       = 0x0010
	wmCommand     = 0x0111
	wmTimer       = 0x0113
	wmRBUTTONUP   = 0x0205
	wmLBUTTONDBLC = 0x0203

	mfString    = 0x0000
	mfSeparator = 0x0800
	mfGrayed    = 0x0001

	tpmLeftAlign  = 0x0000
	tpmRightBtn   = 0x0002
	tpmReturncmd  = 0x0100
	smCXSmallIcon = 49
	smCYSmallIcon = 50
)

// Tray is a running notification-area presence.
type Tray struct {
	cfg  Config
	hwnd windows.HWND
	icon windows.Handle
	nid  notifyIconData

	// taskbarCreated is broadcast when Explorer restarts; the icon must be
	// re-added or it silently disappears for the rest of the session.
	taskbarCreated uint32

	quitOnce sync.Once
	stopped  chan struct{}
}

// Start puts the icon in the notification area and runs its message loop on a
// dedicated locked OS thread. It returns once the icon is up; the returned
// Tray stays alive until Stop or the user picks Quit.
func Start(cfg Config) (*Tray, error) {
	t := &Tray{cfg: cfg, stopped: make(chan struct{})}
	ready := make(chan error, 1)
	go t.run(ready)
	if err := <-ready; err != nil {
		return nil, err
	}
	return t, nil
}

// Stop removes the icon and ends the message loop. Safe to call twice, and
// safe to call when the user already quit through the menu.
func (t *Tray) Stop() {
	if t == nil {
		return
	}
	select {
	case <-t.stopped:
		return
	default:
	}
	if t.hwnd != 0 {
		procPostMessageW.Call(uintptr(t.hwnd), wmClose, 0, 0)
	}
	<-t.stopped
}

// Done is closed once the tray is gone, whichever way it went.
func (t *Tray) Done() <-chan struct{} { return t.stopped }

func (t *Tray) run(ready chan<- error) {
	// A Win32 message loop belongs to the thread that created the window;
	// without this the Go scheduler could dispatch messages from elsewhere.
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	defer close(t.stopped)

	hinst, _, _ := procGetModuleHandleW.Call(0)
	className := windows.StringToUTF16Ptr("HydraTrayWindow")

	wc := wndClassEx{
		CbSize:        uint32(unsafe.Sizeof(wndClassEx{})),
		LpfnWndProc:   windows.NewCallback(t.wndProc),
		HInstance:     windows.Handle(hinst),
		LpszClassName: className,
	}
	if r, _, err := procRegisterClassExW.Call(uintptr(unsafe.Pointer(&wc))); r == 0 {
		ready <- fmt.Errorf("RegisterClassEx: %w", err)
		return
	}

	// A message-only window (HWND_MESSAGE parent, -3) never paints and never
	// appears on the taskbar; it exists purely to receive the icon callbacks.
	hwnd, _, err := procCreateWindowExW.Call(
		0,
		uintptr(unsafe.Pointer(className)),
		uintptr(unsafe.Pointer(windows.StringToUTF16Ptr("Hydra"))),
		0, 0, 0, 0, 0,
		^uintptr(2), // HWND_MESSAGE
		0, hinst, 0,
	)
	if hwnd == 0 {
		ready <- fmt.Errorf("CreateWindowEx: %w", err)
		return
	}
	t.hwnd = windows.HWND(hwnd)

	m, _, _ := procRegisterWindowMessage.Call(uintptr(unsafe.Pointer(windows.StringToUTF16Ptr("TaskbarCreated"))))
	t.taskbarCreated = uint32(m)

	t.icon = t.makeIcon()
	t.nid = notifyIconData{
		CbSize:           uint32(unsafe.Sizeof(notifyIconData{})),
		HWnd:             t.hwnd,
		UID:              1,
		UFlags:           nifMessage | nifIcon | nifTip,
		UCallbackMessage: trayCallbackMsg,
		HIcon:            t.icon,
	}
	t.setTip(t.tooltip())

	if r, _, err := procShellNotifyIconW.Call(nimAdd, uintptr(unsafe.Pointer(&t.nid))); r == 0 {
		procDestroyWindow.Call(hwnd)
		ready <- fmt.Errorf("Shell_NotifyIcon(NIM_ADD): %w", err)
		return
	}
	procSetTimer.Call(hwnd, timerID, tooltipEveryMS, 0)
	ready <- nil

	var m2 msg
	for {
		r, _, _ := procGetMessageW.Call(uintptr(unsafe.Pointer(&m2)), 0, 0, 0)
		if int32(r) <= 0 { // 0 = WM_QUIT, -1 = error
			break
		}
		procTranslateMessage.Call(uintptr(unsafe.Pointer(&m2)))
		procDispatchMessageW.Call(uintptr(unsafe.Pointer(&m2)))
	}
}

func (t *Tray) wndProc(hwnd windows.HWND, message uint32, wParam, lParam uintptr) uintptr {
	switch {
	case message == t.taskbarCreated && t.taskbarCreated != 0:
		procShellNotifyIconW.Call(nimAdd, uintptr(unsafe.Pointer(&t.nid)))
		return 0

	case message == wmTimer:
		t.setTip(t.tooltip())
		procShellNotifyIconW.Call(nimModify, uintptr(unsafe.Pointer(&t.nid)))
		return 0

	case message == trayCallbackMsg:
		switch uint32(lParam) {
		case wmLBUTTONDBLC:
			t.openUI()
		case wmRBUTTONUP:
			t.showMenu()
		}
		return 0

	case message == wmCommand:
		switch uint32(wParam) & 0xFFFF {
		case idOpen:
			t.openUI()
		case idUpdate:
			// Started, not awaited: the updater runs on its own and will post
			// WM_CLOSE here when it needs Hydra gone.
			if t.cfg.OnUpdate != nil {
				go t.cfg.OnUpdate()
			}
		case idQuit:
			procPostMessageW.Call(uintptr(t.hwnd), wmClose, 0, 0)
		}
		return 0

	case message == wmClose:
		procKillTimer.Call(uintptr(t.hwnd), timerID)
		procShellNotifyIconW.Call(nimDelete, uintptr(unsafe.Pointer(&t.nid)))
		if t.icon != 0 {
			procDestroyIcon.Call(uintptr(t.icon))
		}
		procDestroyWindow.Call(uintptr(t.hwnd))
		return 0

	case message == wmDestroy:
		procPostQuitMessage.Call(0)
		return 0
	}
	r, _, _ := procDefWindowProcW.Call(uintptr(hwnd), uintptr(message), wParam, lParam)
	return r
}

func (t *Tray) showMenu() {
	menu, _, _ := procCreatePopupMenu.Call()
	if menu == 0 {
		return
	}
	defer procDestroyMenu.Call(menu)

	s := t.stats()
	header := fmt.Sprintf("Hydra v%s", t.cfg.Version)
	rates := fmt.Sprintf("D %s   U %s   %d active",
		rate(s.Down), rate(s.Up), s.Active)

	appendItem(menu, mfString|mfGrayed, 0, header)
	appendItem(menu, mfString|mfGrayed, 0, rates)
	appendItem(menu, mfSeparator, 0, "")
	appendItem(menu, mfString, idOpen, "Open Hydra")
	appendItem(menu, mfSeparator, 0, "")
	appendItem(menu, mfString, idUpdate, "Check for updates")
	appendItem(menu, mfSeparator, 0, "")
	appendItem(menu, mfString, idQuit, "Quit Hydra")

	var pt point
	procGetCursorPos.Call(uintptr(unsafe.Pointer(&pt)))
	// Required by the docs: without it the menu does not dismiss when the
	// user clicks away from it.
	procSetForegroundWindow.Call(uintptr(t.hwnd))
	cmd, _, _ := procTrackPopupMenu.Call(
		menu, tpmLeftAlign|tpmRightBtn|tpmReturncmd,
		uintptr(pt.X), uintptr(pt.Y), 0, uintptr(t.hwnd), 0,
	)
	if cmd != 0 {
		procPostMessageW.Call(uintptr(t.hwnd), wmCommand, cmd, 0)
	}
}

func appendItem(menu uintptr, flags uint32, id uint32, text string) {
	var p uintptr
	if text != "" {
		p = uintptr(unsafe.Pointer(windows.StringToUTF16Ptr(text)))
	}
	procAppendMenuW.Call(menu, uintptr(flags), uintptr(id), p)
}

func (t *Tray) openUI() {
	if t.cfg.URL == "" {
		return
	}
	procShellExecuteW.Call(0,
		uintptr(unsafe.Pointer(windows.StringToUTF16Ptr("open"))),
		uintptr(unsafe.Pointer(windows.StringToUTF16Ptr(t.cfg.URL))),
		0, 0, 1 /* SW_SHOWNORMAL */)
}

func (t *Tray) stats() Stats {
	if t.cfg.Stats == nil {
		return Stats{}
	}
	return t.cfg.Stats()
}

// tooltip is capped at 127 chars by the shell; keep it to two short lines.
func (t *Tray) tooltip() string {
	s := t.stats()
	return fmt.Sprintf("Hydra v%s\nD %s  U %s\n%d active",
		t.cfg.Version, rate(s.Down), rate(s.Up), s.Active)
}

func (t *Tray) setTip(s string) {
	u := windows.StringToUTF16(s)
	if len(u) > len(t.nid.SzTip) {
		u = u[:len(t.nid.SzTip)-1]
		u = append(u, 0)
	}
	t.nid.SzTip = [128]uint16{}
	copy(t.nid.SzTip[:], u)
}

func rate(bps float64) string {
	const k = 1024.0
	switch {
	case bps >= k*k*k:
		return fmt.Sprintf("%.1f GB/s", bps/(k*k*k))
	case bps >= k*k:
		return fmt.Sprintf("%.1f MB/s", bps/(k*k))
	case bps >= k:
		return fmt.Sprintf("%.0f kB/s", bps/k)
	default:
		return fmt.Sprintf("%.0f B/s", bps)
	}
}

// makeIcon turns the embedded logo into an HICON at the shell's small-icon
// size. On any failure it falls back to the stock application icon rather
// than leaving the tray with a blank slot.
func (t *Tray) makeIcon() windows.Handle {
	if h := iconFromPNG(t.cfg.IconPNG); h != 0 {
		return h
	}
	// IDI_APPLICATION
	h, _, _ := procLoadIconW.Call(0, 32512)
	return windows.Handle(h)
}

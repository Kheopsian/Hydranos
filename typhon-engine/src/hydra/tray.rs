//! The notification-area icon, as the 3.x package had one.
//!
//! Windows only, and a no-op everywhere else. It runs on its OWN thread with
//! its own message loop: a Win32 window belongs to the thread that created it
//! and only that thread may pump its messages, so this cannot live inside the
//! tokio runtime.
//!
//! | Action | Result |
//! | --- | --- |
//! | Hover | version, rates, torrent count |
//! | Double-click | opens the web UI |
//! | Open Hydranos | the same |
//! | Check for updates | runs hydranos-update beside us |
//! | Quit Hydranos | a CLEAN stop, the one that flushes resume data |

use std::sync::OnceLock;

/// Raised when the tray's Quit is chosen, awaited by `shutdown_signal`.
///
/// A separate path from Ctrl+C on purpose: both have to end in the same
/// flush, and a tray that killed the process would skip it -- which is exactly
/// the "Task Manager" mistake the 3.x README warned about.
pub static QUIT: OnceLock<tokio::sync::Notify> = OnceLock::new();

pub fn quit_notify() -> &'static tokio::sync::Notify {
    QUIT.get_or_init(tokio::sync::Notify::new)
}

/// Text shown on hover. Rebuilt on a timer by the tray thread.
pub type Tooltip = std::sync::Arc<dyn Fn() -> String + Send + Sync>;

#[cfg(not(windows))]
pub fn spawn(_port: u16, _tooltip: Tooltip) {}

#[cfg(windows)]
pub fn spawn(port: u16, tooltip: Tooltip) {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || win::run(port, tooltip))
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!("no tray icon: {e}"));
}

#[cfg(windows)]
mod win {
    use super::Tooltip;
    use std::cell::RefCell;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// Our own message id for the icon's mouse events. WM_APP is the first
    /// value Windows promises never to use itself.
    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_UPDATE: usize = 2;
    const ID_QUIT: usize = 3;
    /// Tooltip refresh.
    const TIMER_ID: usize = 1;

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    struct State {
        port: u16,
        tooltip: Tooltip,
        data: NOTIFYICONDATAW,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run(port: u16, tooltip: Tooltip) {
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class = wide("HydranosTray");

            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(wndproc);
            wc.hInstance = hinstance;
            wc.lpszClassName = class.as_ptr();
            if RegisterClassW(&wc) == 0 {
                tracing::warn!("no tray icon: the window class would not register");
                return;
            }

            // HWND_MESSAGE: a message-only window. It is never shown, never
            // appears in the taskbar, and exists purely to receive the icon's
            // callbacks.
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("Hydranos").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                tracing::warn!("no tray icon: the message window would not open");
                return;
            }

            let mut data: NOTIFYICONDATAW = std::mem::zeroed();
            data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = hwnd;
            data.uID = 1;
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = WM_TRAY;
            // The logo, decoded and scaled at run time exactly as 3.x did --
            // there never was a .ico to ship. Falls back to the stock icon if
            // anything about the PNG surprises us, because a tray with a
            // generic icon still works and one with no icon does not.
            data.hIcon = match crate::trayicon::from_logo() {
                Some(h) => h,
                None => {
                    tracing::warn!("tray: the logo would not build an icon, using the stock one");
                    LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
                }
            };
            set_tip(&mut data, &tooltip());

            if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
                tracing::warn!("no tray icon: the shell refused it");
                return;
            }
            tracing::info!("tray icon added; right-click it to quit cleanly");

            STATE.with(|s| *s.borrow_mut() = Some(State { port, tooltip, data }));
            // Rates move; the tooltip follows them.
            SetTimer(hwnd, TIMER_ID, 2000, None);

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            STATE.with(|s| {
                if let Some(st) = s.borrow().as_ref() {
                    Shell_NotifyIconW(NIM_DELETE, &st.data);
                }
            });
        }
    }

    fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
        // szTip is a fixed 128-wchar buffer including its NUL: anything longer
        // is truncated rather than written past the end.
        let w = wide(text);
        let n = w.len().min(data.szTip.len());
        data.szTip[..n].copy_from_slice(&w[..n]);
        if n == data.szTip.len() {
            data.szTip[n - 1] = 0;
        }
    }

    unsafe fn open_ui(port: u16) {
        let url = wide(&format!("http://127.0.0.1:{port}"));
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }

    /// Run the updater that sits beside us.
    ///
    /// In its own console: it asks before replacing anything, and a
    /// confirmation prompt with nowhere to be typed would hang forever.
    unsafe fn run_updater() {
        let Ok(exe) = std::env::current_exe() else { return };
        let Some(dir) = exe.parent() else { return };
        let updater = dir.join("hydranos-update.exe");
        if !updater.exists() {
            tracing::warn!("no hydranos-update.exe beside {}", exe.display());
            return;
        }
        let args = format!("/K \"\"{}\" --dir \"{}\"\"", updater.display(), dir.display());
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide("cmd.exe").as_ptr(),
            wide(&args).as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }

    unsafe fn show_menu(hwnd: HWND) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, ID_OPEN, wide("Open Hydranos").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_UPDATE, wide("Check for updates").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit Hydranos").as_ptr());

        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        // ⚠ SetForegroundWindow before, and the dummy PostMessage after, are
        // both required: without them the menu never closes when the user
        // clicks elsewhere. This is documented Win32 behaviour for menus owned
        // by a window that is not in the foreground.
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TRAY => {
                match lparam as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => {
                        STATE.with(|s| {
                            if let Some(st) = s.borrow().as_ref() {
                                open_ui(st.port);
                            }
                        });
                    }
                    _ => {}
                }
                0
            }
            WM_TIMER => {
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        let text = (st.tooltip)();
                        set_tip(&mut st.data, &text);
                        Shell_NotifyIconW(NIM_MODIFY, &st.data);
                    }
                });
                0
            }
            WM_COMMAND => {
                match (wparam & 0xFFFF) as usize {
                    ID_OPEN => STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            open_ui(st.port);
                        }
                    }),
                    ID_UPDATE => run_updater(),
                    ID_QUIT => {
                        // Ask the runtime to stop; do NOT exit here. The flush
                        // is what makes this different from Task Manager.
                        tracing::info!("tray: quit chosen, stopping cleanly");
                        super::quit_notify().notify_waiters();
                        PostQuitMessage(0);
                    }
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

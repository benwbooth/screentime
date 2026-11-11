//! Runs in the kid's session (started by the service). Shows time warnings as a small toast in
//! the lower-right corner that never takes focus, and a tray icon to see the time left and ask for
//! more. A parent approves time on the spot only at the sign-in screen's tile: anything typed in
//! the kid's own session could be read by a program the kid runs.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod tray {
    use screentime_common::{CLIENT_PIPE, Decision, DenyReason, SocketRequest, SocketResponse};
    use screentime_windows::win::{from_wide, pcwstr, wide};
    use anyhow::{Context, Result};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::time::Duration;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
        EndPaint, FillRect, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
        FW_NORMAL, FW_SEMIBOLD, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_VALUE_TYPE, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
        GetMessageW, HWND_TOPMOST, IDI_APPLICATION, KillTimer, LoadIconW, MF_GRAYED, MF_SEPARATOR,
        MF_STRING, MSG, PostMessageW, RegisterClassW, RegisterWindowMessageW, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_NOACTIVATE, SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow,
        SystemParametersInfoW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        TrackPopupMenu, TranslateMessage, WM_APP, WM_LBUTTONUP, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        LWA_ALPHA, WINDOW_EX_STYLE, WINDOW_STYLE,
    };
    use windows::core::{PCWSTR, PWSTR};

    const WM_TRAY: u32 = WM_APP + 1;
    const WM_WARNING: u32 = WM_APP + 2;
    const TOAST_W: i32 = 340;
    const TOAST_H: i32 = 76;
    const TOAST_MARGIN: i32 = 16;
    const TOAST_TIMER: usize = 1;
    const TOAST_SECONDS: u32 = 8;
    const CMD_REQUEST: usize = 100;

    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static TOAST_HWND: AtomicIsize = AtomicIsize::new(0);
    static STATUS: Mutex<String> = Mutex::new(String::new());
    /// Explorer's "the taskbar was recreated" message, to put the icon back after Explorer restarts.
    static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static TOAST_TEXT: Mutex<String> = Mutex::new(String::new());

    fn username() -> String {
        std::env::var("USERNAME").unwrap_or_default()
    }

    /// One request to the service, one reply.
    fn ask(request: &SocketRequest) -> Result<SocketResponse> {
        let mut pipe = std::fs::OpenOptions::new().read(true).write(true).open(CLIENT_PIPE)
            .context("the screen time service isn't running")?;
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        pipe.write_all(&line)?;
        let mut reply = String::new();
        BufReader::new(pipe).read_line(&mut reply)?;
        Ok(serde_json::from_str(&reply)?)
    }

    /// Remember the status for the menu and show it as the icon's hover text.
    fn set_status(text: String) {
        let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as _);
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_TIP;
        copy_into(&mut data.szTip, if text.is_empty() { "Screen time" } else { &text });
        *STATUS.lock().unwrap() = text;
        if !hwnd.is_invalid() {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
            }
        }
    }

    /// Keep the remaining time current, and the icon pinned to the taskbar rather than tucked away
    /// in the hidden-icons menu (Windows 11 keeps that choice per user, so a kid could hide it).
    fn keep_status_and_icon_fresh() {
        loop {
            if let Ok(response) = ask(&SocketRequest::Status { username: username() }) {
                let text = status_text(&response);
                if !text.is_empty() {
                    set_status(text);
                }
            }
            promote_icon();
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// Set IsPromoted on this app's entry under HKCU\Control Panel\NotifyIconSettings.
    fn promote_icon() {
        let Ok(exe) = std::env::current_exe() else { return };
        let exe = exe.to_string_lossy().to_lowercase();
        let root = wide(r"Control Panel\NotifyIconSettings");
        let mut key = HKEY::default();
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, pcwstr(&root), None, KEY_READ, &mut key).is_err() {
                return;
            }
            for i in 0.. {
                let mut name = [0u16; 64];
                let mut len = name.len() as u32;
                if RegEnumKeyExW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None).is_err() {
                    break;
                }
                let mut sub = HKEY::default();
                if RegOpenKeyExW(key, PCWSTR(name.as_ptr()), None, KEY_READ | KEY_SET_VALUE, &mut sub).is_err() {
                    continue;
                }
                let mut path = [0u16; 520];
                let mut size = (path.len() * 2) as u32;
                let mut kind = REG_VALUE_TYPE::default();
                let value = wide("ExecutablePath");
                let found = RegQueryValueExW(sub, pcwstr(&value), None, Some(&mut kind), Some(path.as_mut_ptr() as *mut u8), Some(&mut size)).is_ok()
                    && from_wide(&path).to_lowercase() == exe;
                if found {
                    let promoted = wide("IsPromoted");
                    let _ = RegSetValueExW(sub, pcwstr(&promoted), None, REG_DWORD, Some(&1u32.to_le_bytes()));
                }
                let _ = RegCloseKey(sub);
            }
            let _ = RegCloseKey(key);
        }
    }

    fn fmt_minutes(m: u32) -> String {
        match (m / 60, m % 60) {
            (0, m) => format!("{m} min"),
            (h, 0) => format!("{h} h"),
            (h, m) => format!("{h} h {m} min"),
        }
    }

    fn status_text(response: &SocketResponse) -> String {
        match response {
            SocketResponse::Decision { decision: Decision::Allowed { remaining_minutes: Some(m) }, .. } => {
                format!("{} of screen time left", fmt_minutes(*m))
            }
            SocketResponse::Decision { decision: Decision::Allowed { remaining_minutes: None }, .. } => {
                "No screen time limit right now".into()
            }
            SocketResponse::Decision { decision: Decision::Denied { reason }, .. } => match reason {
                DenyReason::Downtime { until } => format!("Downtime until {}", until.format("%-I:%M %p")),
                other => other.to_string(),
            },
            SocketResponse::Unmanaged => "No screen time limits on this account".into(),
            _ => String::new(),
        }
    }

    /// Follow the service's warnings for this user, reconnecting if the service restarts.
    fn follow_warnings() {
        loop {
            let result = (|| -> Result<()> {
                let mut pipe = std::fs::OpenOptions::new().read(true).write(true).open(CLIENT_PIPE)?;
                let mut line = serde_json::to_vec(&SocketRequest::Subscribe { username: username() })?;
                line.push(b'\n');
                pipe.write_all(&line)?;
                let warn = |minutes: u32| {
                    set_status(format!("{} of screen time left", fmt_minutes(minutes)));
                    let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as _);
                    unsafe {
                        let _ = PostMessageW(Some(hwnd), WM_WARNING, WPARAM(minutes as usize), LPARAM(0));
                    }
                };
                for line in BufReader::new(pipe).lines() {
                    match serde_json::from_str::<SocketResponse>(&line?)? {
                        SocketResponse::Warning { remaining_minutes } => warn(remaining_minutes),
                        // The first reply is the current status. If time is already short, the
                        // service's warning may have gone out before this app started.
                        SocketResponse::Decision { decision: Decision::Allowed { remaining_minutes: Some(m) }, .. }
                            if m <= screentime_common::WARNING_THRESHOLDS[0] =>
                        {
                            warn(m)
                        }
                        other => set_status(status_text(&other)),
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                set_status(format!("Screen time service unavailable ({e})"));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn balloon(title: &str, text: &str, warning: bool) {
        let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as _);
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_INFO;
        copy_into(&mut data.szInfoTitle, title);
        copy_into(&mut data.szInfo, text);
        data.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
    }

    fn copy_into(dst: &mut [u16], s: &str) {
        let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
        dst[..w.len()].copy_from_slice(&w);
        dst[w.len()] = 0;
    }

    fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
    }

    fn report(result: Result<SocketResponse>) {
        match result {
            Ok(SocketResponse::RequestSent) => balloon("Request sent", "Your parent will get a notification.", false),
            Ok(SocketResponse::RequestQueued) => {
                balloon("Request saved", "It will be sent when the computer reaches the server.", false)
            }
            Ok(SocketResponse::Granted { minutes }) => {
                balloon("Time added", &format!("{} more minutes.", minutes), false)
            }
            Ok(SocketResponse::Error { message }) => balloon("Couldn't do that", &message, true),
            Ok(SocketResponse::Unmanaged) => balloon("Screen time", "This account has no screen time limits.", false),
            Ok(_) => {}
            Err(e) => balloon("Screen time", &e.to_string(), true),
        }
    }

    fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let status = wide(&STATUS.lock().unwrap().clone());
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(&status));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let request = wide("Request more time");
            let _ = AppendMenuW(menu, MF_STRING, CMD_REQUEST, pcwstr(&request));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu closes when clicking elsewhere; only happens on a deliberate click.
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None).0 as usize;
            let _ = DestroyMenu(menu);
            match cmd {
                CMD_REQUEST => {
                    std::thread::spawn(move || {
                        report(ask(&SocketRequest::RequestTime { username: username(), minutes: None, reason: None }))
                    });
                }
                _ => {}
            }
        }
    }

    fn show_toast(minutes: u32) {
        let hwnd = HWND(TOAST_HWND.load(Ordering::Relaxed) as _);
        *TOAST_TEXT.lock().unwrap() = match minutes {
            1 => "1 minute of screen time left".into(),
            m => format!("{m} minutes of screen time left"),
        };
        unsafe {
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                work.right - TOAST_W - TOAST_MARGIN,
                work.bottom - TOAST_H - TOAST_MARGIN,
                TOAST_W,
                TOAST_H,
                SWP_NOACTIVATE,
            );
            let _ = InvalidateRect(Some(hwnd), None, true);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetTimer(Some(hwnd), TOAST_TIMER, TOAST_SECONDS * 1000, None);
        }
    }

    fn paint_toast(hwnd: HWND) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let rect = RECT { left: 0, top: 0, right: TOAST_W, bottom: TOAST_H };
            let bg = CreateSolidBrush(COLORREF(0x00302a26)); // BGR
            FillRect(hdc, &rect, bg);
            let accent = CreateSolidBrush(COLORREF(0x00c3592b));
            FillRect(hdc, &RECT { left: 0, top: 0, right: 5, bottom: TOAST_H }, accent);
            SetBkMode(hdc, TRANSPARENT);
            let face = wide("Segoe UI");
            let title_font = CreateFontW(-18, 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, pcwstr(&face));
            let small_font = CreateFontW(-13, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, pcwstr(&face));
            let old = SelectObject(hdc, title_font.into());
            SetTextColor(hdc, COLORREF(0x00f4f4f4));
            // The window is painted once at startup, before there's any warning to show, and
            // DrawTextW reads through an empty slice's dangling pointer and crashes.
            let mut title = TOAST_TEXT.lock().unwrap().encode_utf16().collect::<Vec<u16>>();
            if !title.is_empty() {
                let mut r = RECT { left: 20, top: 10, right: TOAST_W - 12, bottom: 42 };
                DrawTextW(hdc, &mut title, &mut r, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            }
            SelectObject(hdc, small_font.into());
            SetTextColor(hdc, COLORREF(0x00b9b4ae));
            let mut hint = "Click the hourglass icon to request more time".encode_utf16().collect::<Vec<u16>>();
            let mut r = RECT { left: 20, top: 42, right: TOAST_W - 12, bottom: 66 };
            DrawTextW(hdc, &mut hint, &mut r, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            SelectObject(hdc, old);
            let _ = DeleteObject(title_font.into());
            let _ = DeleteObject(small_font.into());
            let _ = DeleteObject(bg.into());
            let _ = DeleteObject(accent.into());
            let _ = EndPaint(hwnd, &ps);
        }
    }

    extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_TRAY => {
                let event = (lparam.0 & 0xffff) as u32;
                if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                    show_menu(hwnd);
                }
                LRESULT(0)
            }
            WM_WARNING => {
                show_toast(wparam.0 as u32);
                LRESULT(0)
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                add_icon(hwnd);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn add_icon(hwnd: HWND) {
        let instance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        // The hourglass embedded as resource 1, falling back to the generic application icon.
        data.hIcon = unsafe { LoadIconW(Some(instance.into()), windows::core::PCWSTR(1 as _)) }
            .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
            .unwrap_or_default();
        let status = STATUS.lock().unwrap().clone();
        copy_into(&mut data.szTip, if status.is_empty() { "Screen time" } else { &status });
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
        }
    }

    extern "system" fn toast_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                paint_toast(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TOAST_TIMER);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn register(name: &[u16], proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) -> Result<()> {
        let instance = unsafe { GetModuleHandleW(None) }?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(proc_),
            hInstance: instance.into(),
            lpszClassName: pcwstr(name),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            anyhow::bail!("RegisterClassW failed");
        }
        Ok(())
    }

    pub fn main() -> Result<()> {
        // `--lock`: the service runs this as the kid to lock their session (out of time, or
        // locked by a parent), since a service can't lock someone else's session itself.
        if std::env::args().any(|a| a == "--lock") {
            return unsafe { windows::Win32::System::Shutdown::LockWorkStation() }.context("LockWorkStation");
        }
        // There's no console, so leave a note where a parent can find it if this ever panics.
        std::panic::set_hook(Box::new(|info| {
            let path = std::env::temp_dir().join("screentime-tray-panic.txt");
            let _ = std::fs::write(path, format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
        }));
        let instance = unsafe { GetModuleHandleW(None) }?;
        let (tray_class, toast_class) = (wide("ScreenTimeTray"), wide("ScreenTimeToast"));
        register(&tray_class, tray_proc)?;
        register(&toast_class, toast_proc)?;
        let title = wide("Screen time");
        let tray = unsafe {
            CreateWindowExW(WINDOW_EX_STYLE(0), pcwstr(&tray_class), pcwstr(&title), WINDOW_STYLE(0),
                0, 0, 0, 0, None, None, Some(instance.into()), None)
        }?;
        // Never focusable, click-through, above fullscreen windows, not in the taskbar.
        let toast = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                pcwstr(&toast_class), pcwstr(&title), WS_POPUP,
                0, 0, TOAST_W, TOAST_H, None, None, Some(instance.into()), None,
            )
        }?;
        unsafe { SetLayeredWindowAttributes(toast, COLORREF(0), 235, LWA_ALPHA) }?;
        TRAY_HWND.store(tray.0 as isize, Ordering::Relaxed);
        TOAST_HWND.store(toast.0 as isize, Ordering::Relaxed);

        let created = wide("TaskbarCreated");
        TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(pcwstr(&created)) }, Ordering::Relaxed);
        if let Ok(response) = ask(&SocketRequest::Status { username: username() }) {
            *STATUS.lock().unwrap() = status_text(&response);
        }
        add_icon(tray);
        // `--preview` shows what a warning looks like, for parents setting this up.
        if std::env::args().any(|a| a == "--preview") {
            show_toast(10);
        }
        std::thread::spawn(follow_warnings);
        std::thread::spawn(keep_status_and_icon_fresh);

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tray::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("screentime-tray only runs on Windows.");
    std::process::exit(1);
}

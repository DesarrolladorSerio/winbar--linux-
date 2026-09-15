#![windows_subsystem = "windows"]

mod search;

use std::mem;
use std::sync::Mutex;
use std::thread;
use std::io::{BufRead, BufReader};
use lazy_static::lazy_static;

use std::cell::RefCell;

use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::Graphics::Gdi::*,
    Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator},
    Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED},
    Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
    Win32::UI::Shell::*,
    Win32::UI::WindowsAndMessaging::*,
    Win32::UI::Accessibility::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
};

lazy_static! {
    static ref MAIN_HWND: Mutex<isize> = Mutex::new(0);
    static ref FOCUSED_WINDOW_TITLE: Mutex<String> = Mutex::new(String::new());
    static ref ACTIVE_WORKSPACE: Mutex<usize> = Mutex::new(0);
    // (volume percent 0-100, is_muted) — refreshed on the 1s timer tick.
    static ref VOLUME_STATE: Mutex<(u32, bool)> = Mutex::new((0, false));
}

// COM interfaces aren't Send; the endpoint volume handle is only ever touched
// from the main UI thread, so it lives in thread-local storage instead of a
// shared Mutex.
thread_local! {
    static ENDPOINT_VOLUME: RefCell<Option<IAudioEndpointVolume>> = RefCell::new(None);
}

const VOLUME_RECT_WIDTH: i32 = 100;
const CLOCK_WIDTH: i32 = 140;

fn volume_rect(width: i32, height: i32) -> RECT {
    RECT {
        left: width - VOLUME_RECT_WIDTH - 20,
        top: 0,
        right: width - 20,
        bottom: height,
    }
}

fn clock_rect(width: i32, height: i32) -> RECT {
    RECT {
        left: width / 2 - CLOCK_WIDTH / 2,
        top: 0,
        right: width / 2 + CLOCK_WIDTH / 2,
        bottom: height,
    }
}

fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

fn get_endpoint_volume() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

fn with_endpoint_volume<R>(f: impl FnOnce(&IAudioEndpointVolume) -> windows::core::Result<R>) -> Option<R> {
    ENDPOINT_VOLUME.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = get_endpoint_volume().ok();
        }
        slot.as_ref().and_then(|ev| f(ev).ok())
    })
}

fn refresh_volume_state() {
    if let Some((scalar, muted)) = with_endpoint_volume(|ev| unsafe {
        Ok((ev.GetMasterVolumeLevelScalar()?, ev.GetMute()?.as_bool()))
    }) {
        *VOLUME_STATE.lock().unwrap() = ((scalar * 100.0).round() as u32, muted);
    }
}

fn toggle_mute() {
    with_endpoint_volume(|ev| unsafe {
        let muted = ev.GetMute()?.as_bool();
        ev.SetMute(!muted, std::ptr::null())
    });
    refresh_volume_state();
}

fn change_volume(delta: f32) {
    with_endpoint_volume(|ev| unsafe {
        let current = ev.GetMasterVolumeLevelScalar()?;
        let new_level = (current + delta).clamp(0.0, 1.0);
        ev.SetMasterVolumeLevelScalar(new_level, std::ptr::null())
    });
    refresh_volume_state();
}

fn register_appbar(hwnd: HWND) {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: mem::size_of::<APPBARDATA>() as u32,
            hWnd: hwnd,
            uEdge: ABE_TOP,
            ..Default::default()
        };

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        abd.rc = RECT {
            left: 0,
            top: 0,
            right: screen_w,
            bottom: 32,
        };

        SHAppBarMessage(ABM_NEW, &mut abd);
        SHAppBarMessage(ABM_SETPOS, &mut abd);

        SetWindowPos(
            hwnd,
            HWND::default(),
            abd.rc.left,
            abd.rc.top,
            abd.rc.right - abd.rc.left,
            abd.rc.bottom - abd.rc.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        ).unwrap();
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            *MAIN_HWND.lock().unwrap() = hwnd.0 as isize;
            register_appbar(hwnd);
            init_com();
            refresh_volume_state();
            search::register_hotkey(hwnd);
            SetTimer(hwnd, 1, 1000, None);
            LRESULT(0)
        }
        WM_HOTKEY => {
            if wparam.0 as i32 == search::HOTKEY_ID {
                search::toggle_search(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut abd = APPBARDATA {
                cbSize: mem::size_of::<APPBARDATA>() as u32,
                hWnd: hwnd,
                ..Default::default()
            };
            SHAppBarMessage(ABM_REMOVE, &mut abd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TIMER => {
            refresh_volume_state();
            InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);
            let vol_rc = volume_rect(rc.right - rc.left, rc.bottom);
            if x >= vol_rc.left && x < vol_rc.right && y >= vol_rc.top && y < vol_rc.bottom {
                toggle_mute();
                InvalidateRect(hwnd, Some(&vol_rc as *const _), false);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 as i32) >> 16) as i16;
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);
            let vol_rc = volume_rect(rc.right - rc.left, rc.bottom);
            if pt.x >= vol_rc.left && pt.x < vol_rc.right && pt.y >= vol_rc.top && pt.y < vol_rc.bottom {
                let step = if delta > 0 { 0.05 } else { -0.05 };
                change_volume(step);
                InvalidateRect(hwnd, Some(&vol_rc as *const _), false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            draw(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn draw(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;

    let mem_dc = CreateCompatibleDC(hdc);
    let mem_bm = CreateCompatibleBitmap(hdc, width, height);
    let old_bm = SelectObject(mem_dc, HGDIOBJ(mem_bm.0 as _));

    // Colors
    let bg_color = 0x001B1111; // #11111b
    let text_color = 0x00F4D6CD; // #cdd6f4
    let mauve_color = 0x0085D256; // #56d285 — acento extraído del wallpaper activo (theming/colors.json)

    let bg_brush = CreateSolidBrush(COLORREF(bg_color));
    FillRect(mem_dc, &rc, bg_brush);
    DeleteObject(HGDIOBJ(bg_brush.0 as _));

    SetBkMode(mem_dc, TRANSPARENT);
    SetTextColor(mem_dc, COLORREF(text_color));
    
    let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let hfont = CreateFontW(
        16, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0,
        DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32, VARIABLE_PITCH.0 as u32, PCWSTR(font_name.as_ptr())
    );
    let old_font = SelectObject(mem_dc, HGDIOBJ(hfont.0 as _));

    // 1. Draw Workspaces
    let active_ws = *ACTIVE_WORKSPACE.lock().unwrap();
    let mut x_offset = 10;
    let ws_labels = ["I", "II", "III", "IV", "V", "VI", "VII"];
    let ws_width = 36;
    
    let active_bg_brush = CreateSolidBrush(COLORREF(mauve_color));
    
    for (i, label) in ws_labels.iter().enumerate() {
        let mut ws_rc = RECT {
            left: x_offset,
            top: 4,
            right: x_offset + ws_width,
            bottom: height - 4,
        };
        
        if i == active_ws {
            FillRect(mem_dc, &ws_rc, active_bg_brush);
            SetTextColor(mem_dc, COLORREF(bg_color));
        } else {
            SetTextColor(mem_dc, COLORREF(text_color));
        }
        
        let mut label_w: Vec<u16> = label.encode_utf16().collect();
        DrawTextW(mem_dc, &mut label_w, &mut ws_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        
        x_offset += ws_width + 8;
    }
    DeleteObject(HGDIOBJ(active_bg_brush.0 as _));

    // 2. Draw Title
    SetTextColor(mem_dc, COLORREF(text_color));
    let title = FOCUSED_WINDOW_TITLE.lock().unwrap().clone();
    if !title.is_empty() {
        let mut title_rc = RECT {
            left: x_offset + 20,
            top: 0,
            right: clock_rect(width, height).left - 10,
            bottom: height,
        };
        let mut title_w: Vec<u16> = title.encode_utf16().collect();
        DrawTextW(mem_dc, &mut title_w, &mut title_rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }

    // 3. Draw Volume
    SetTextColor(mem_dc, COLORREF(text_color));
    let (volume_pct, is_muted) = *VOLUME_STATE.lock().unwrap();
    let volume_str = if is_muted {
        "Mute".to_string()
    } else {
        format!("Vol {}%", volume_pct)
    };
    let mut volume_rc = volume_rect(width, height);
    let mut volume_w: Vec<u16> = volume_str.encode_utf16().collect();
    DrawTextW(mem_dc, &mut volume_w, &mut volume_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    // 4. Draw Clock
    let now = chrono::Local::now();
    let time_str = now.format("%H:%M:%S").to_string();
    let mut time_w: Vec<u16> = time_str.encode_utf16().collect();
    let mut clock_rc = clock_rect(width, height);
    DrawTextW(mem_dc, &mut time_w, &mut clock_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_font);
    DeleteObject(HGDIOBJ(hfont.0 as _));
    SelectObject(mem_dc, old_bm);
    DeleteObject(HGDIOBJ(mem_bm.0 as _));
    DeleteDC(mem_dc);
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _iddeventthread: u32,
    _dwmseventtime: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND {
        let len = GetWindowTextLengthW(hwnd);
        if len > 0 {
            let mut buf = vec![0u16; (len + 1) as usize];
            GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf);
            *FOCUSED_WINDOW_TITLE.lock().unwrap() = title.trim_end_matches('\0').to_string();
        } else {
            *FOCUSED_WINDOW_TITLE.lock().unwrap() = String::new();
        }
        
        let main_hwnd = *MAIN_HWND.lock().unwrap();
        if main_hwnd != 0 {
            InvalidateRect(HWND(main_hwnd as *mut _), None, false);
        }
    }
}

fn komorebi_thread() {
    let socket_name = "winbar_subscriber";
    let listener = match komorebi_client::subscribe(socket_name) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to subscribe to komorebi: {}", e);
            return;
        }
    };
    
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let reader = BufReader::new(s);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                            if let Some(state) = v.get("state") {
                                if let Some(monitors) = state.get("monitors") {
                                    if let Some(focused_monitor_idx) = monitors.get("focused").and_then(|i| i.as_u64()) {
                                        if let Some(elements) = monitors.get("elements").and_then(|e| e.as_array()) {
                                            if let Some(monitor) = elements.get(focused_monitor_idx as usize) {
                                                if let Some(workspaces) = monitor.get("workspaces") {
                                                    if let Some(focused_ws) = workspaces.get("focused").and_then(|i| i.as_u64()) {
                                                        let mut current = ACTIVE_WORKSPACE.lock().unwrap();
                                                        if *current != focused_ws as usize {
                                                            *current = focused_ws as usize;
                                                            let hwnd = *MAIN_HWND.lock().unwrap();
                                                            if hwnd != 0 {
                                                                unsafe {
                                                                    InvalidateRect(HWND(hwnd as *mut _), None, false);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => continue,
        }
    }
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance = GetModuleHandleW(None).unwrap();
        
        let class_name: Vec<u16> = "WinBarClass\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        
        let _hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(std::ptr::null()),
            WS_POPUP | WS_VISIBLE,
            0, 0, 100, 32,
            None,
            None,
            instance,
            None,
        ).unwrap();
        
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        
        let fg_hwnd = GetForegroundWindow();
        if fg_hwnd != HWND::default() {
            let len = GetWindowTextLengthW(fg_hwnd);
            if len > 0 {
                let mut buf = vec![0u16; (len + 1) as usize];
                GetWindowTextW(fg_hwnd, &mut buf);
                let title = String::from_utf16_lossy(&buf);
                *FOCUSED_WINDOW_TITLE.lock().unwrap() = title.trim_end_matches('\0').to_string();
            }
        }

        thread::spawn(|| {
            komorebi_thread();
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    
    Ok(())
}

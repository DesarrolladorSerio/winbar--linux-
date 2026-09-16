// winbar — barra nativa minimalista para Windows, dibujada a mano con GDI.
// Reemplazo liviano de Yasb: workspaces de komorebi, título de la ventana en
// foco, reloj, volumen y un buscador de apps, todo en un solo binario sin
// frameworks de UI (nada de Qt/egui/webview) para mantener el uso de RAM bajo.
//
// Este archivo maneja la ventana principal (la barra en sí). El buscador de
// apps (popup de Alt+Space) vive en su propio módulo, `search.rs`.

#![windows_subsystem = "windows"] // sin consola visible al ejecutar el .exe

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

// Estado global compartido entre la ventana principal, el hook de foreground
// (win_event_proc) y el hilo que escucha a komorebi (komorebi_thread). Todos
// estos corren en threads distintos, por eso todo va detrás de un Mutex.
lazy_static! {
    // HWND de la barra, guardado como isize porque HWND (un puntero) no es
    // Send y no puede vivir directamente dentro de un Mutex compartido entre
    // threads. Se reconstruye con HWND(val as *mut _) cuando hace falta.
    static ref MAIN_HWND: Mutex<isize> = Mutex::new(0);
    // Título de la ventana actualmente en foco, mostrado en la barra.
    static ref FOCUSED_WINDOW_TITLE: Mutex<String> = Mutex::new(String::new());
    // Índice (0-6) del workspace de komorebi actualmente activo.
    static ref ACTIVE_WORKSPACE: Mutex<usize> = Mutex::new(0);
    // (volumen en %, silenciado) — se refresca en el timer de 1s.
    static ref VOLUME_STATE: Mutex<(u32, bool)> = Mutex::new((0, false));
}

// COM interfaces aren't Send; the endpoint volume handle is only ever touched
// from the main UI thread, so it lives in thread-local storage instead of a
// shared Mutex.
thread_local! {
    static ENDPOINT_VOLUME: RefCell<Option<IAudioEndpointVolume>> = RefCell::new(None);
}

// Ancho reservado para el texto del volumen y del reloj, en píxeles.
const VOLUME_RECT_WIDTH: i32 = 100;
const CLOCK_WIDTH: i32 = 140;

/// Rectángulo del widget de volumen: pegado al borde derecho de la barra.
fn volume_rect(width: i32, height: i32) -> RECT {
    RECT {
        left: width - VOLUME_RECT_WIDTH - 20,
        top: 0,
        right: width - 20,
        bottom: height,
    }
}

/// Rectángulo del reloj: centrado horizontalmente en toda la barra.
fn clock_rect(width: i32, height: i32) -> RECT {
    RECT {
        left: width / 2 - CLOCK_WIDTH / 2,
        top: 0,
        right: width / 2 + CLOCK_WIDTH / 2,
        bottom: height,
    }
}

/// Inicializa COM en este thread (apartment-threaded). Necesario antes de
/// poder crear cualquier interfaz de Core Audio (IMMDeviceEnumerator, etc).
fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Obtiene la interfaz de volumen del dispositivo de audio de salida por
/// defecto, vía Core Audio (IMMDeviceEnumerator -> IMMDevice -> Activate).
fn get_endpoint_volume() -> windows::core::Result<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

/// Ejecuta `f` con la interfaz de volumen ya cacheada (thread-local), pidiéndola
/// de nuevo si todavía no existe o si la llamada anterior falló. Devuelve
/// `None` si Core Audio no está disponible por algún motivo.
fn with_endpoint_volume<R>(f: impl FnOnce(&IAudioEndpointVolume) -> windows::core::Result<R>) -> Option<R> {
    ENDPOINT_VOLUME.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = get_endpoint_volume().ok();
        }
        slot.as_ref().and_then(|ev| f(ev).ok())
    })
}

/// Relee el volumen/mute actual del sistema y actualiza VOLUME_STATE, que es
/// lo que draw() usa para pintar el texto "Vol NN%" / "Mute".
fn refresh_volume_state() {
    if let Some((scalar, muted)) = with_endpoint_volume(|ev| unsafe {
        Ok((ev.GetMasterVolumeLevelScalar()?, ev.GetMute()?.as_bool()))
    }) {
        *VOLUME_STATE.lock().unwrap() = ((scalar * 100.0).round() as u32, muted);
    }
}

/// Togglea mute (click sobre el widget de volumen).
fn toggle_mute() {
    with_endpoint_volume(|ev| unsafe {
        let muted = ev.GetMute()?.as_bool();
        ev.SetMute(!muted, std::ptr::null())
    });
    refresh_volume_state();
}

/// Sube/baja el volumen en `delta` (scroll sobre el widget), clampeado a 0-1.
fn change_volume(delta: f32) {
    with_endpoint_volume(|ev| unsafe {
        let current = ev.GetMasterVolumeLevelScalar()?;
        let new_level = (current + delta).clamp(0.0, 1.0);
        ev.SetMasterVolumeLevelScalar(new_level, std::ptr::null())
    });
    refresh_volume_state();
}

/// Registra la ventana como un Windows AppBar anclado arriba de la pantalla
/// (SHAppBarMessage), igual que hace Yasb con `windows_app_bar: true`. Esto
/// reserva el espacio para que las demás ventanas no queden tapadas debajo
/// de la barra (el "work area" del escritorio se achica en consecuencia).
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
        // El shell puede ajustar abd.rc (por ejemplo si hay otro appbar); hay
        // que usar el rect que devuelve, no el que mandamos.
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

/// Procedimiento de ventana de la barra principal: acá llegan todos los
/// mensajes de Windows (creación, pintado, timers, click/scroll, etc).
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
            search::warm_index(); // precarga el índice de apps para que el primer Alt+Space ya lo encuentre cacheado
            SetTimer(hwnd, 1, 1000, None); // timer 1: refresca volumen + reloj cada 1s
            SetTimer(hwnd, 2, 150, None);  // timer 2: chequeo de fullscreen, rápido para que
                                            // ocultar/mostrar la barra se sienta instantáneo
            LRESULT(0)
        }
        WM_HOTKEY => {
            // Alt+Space (registrado por search::register_hotkey) abre/cierra
            // el buscador de apps.
            if wparam.0 as i32 == search::HOTKEY_ID {
                search::toggle_search(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Hay que avisarle al shell que liberamos el espacio del appbar,
            // si no el "work area" del escritorio queda mal calculado.
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
            if wparam.0 == 1 {
                // Timer lento: releer volumen del sistema (por si cambió con
                // las teclas de multimedia) y repintar todo.
                refresh_volume_state();
                InvalidateRect(hwnd, None, false);
            } else if wparam.0 == 2 {
                // Timer rápido: si la ventana en foco pasó a cubrir toda la
                // pantalla (video/juego en fullscreen), ocultar la barra para
                // no taparlo; si no, asegurarse de que esté visible. Corre en
                // un timer separado (no en el de foreground) porque una app
                // puede pasar de fullscreen a ventana normal sin que cambie
                // cuál ventana tiene el foco (ej. F11 en un reproductor), y
                // en ese caso EVENT_SYSTEM_FOREGROUND no se dispara.
                let fg = GetForegroundWindow();
                if fg != hwnd {
                    let cmd = if is_fullscreen_window(fg) { SW_HIDE } else { SW_SHOWNOACTIVATE };
                    ShowWindow(hwnd, cmd);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Click con el mouse: si cae dentro del widget de volumen, mutear/desmutear.
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
            // Scroll del mouse: si cae dentro del widget de volumen, subir/bajar 5%.
            let delta = ((wparam.0 as i32) >> 16) as i16;
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            // WM_MOUSEWHEEL llega con coordenadas de PANTALLA, no de cliente
            // (a diferencia de los demás mensajes de mouse), por eso hace
            // falta convertir con ScreenToClient antes de comparar contra vol_rc.
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

/// Dibuja toda la barra: fondo, workspaces, título de la ventana en foco,
/// volumen y reloj. Usa double buffering manual (se dibuja todo en un
/// bitmap intermedio en memoria y se copia de una sola vez a la pantalla
/// con BitBlt) para evitar parpadeo.
unsafe fn draw(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;

    // --- Setup del bitmap intermedio (double buffer) ---
    let mem_dc = CreateCompatibleDC(hdc);
    let mem_bm = CreateCompatibleBitmap(hdc, width, height);
    let old_bm = SelectObject(mem_dc, HGDIOBJ(mem_bm.0 as _));

    // Paleta: fondo/texto son Catppuccin Mocha fijos; el acento (mauve_color)
    // sale de theming/colors.json, generado por extract_palette.py a partir
    // del wallpaper activo de Wallpaper Engine (ver README, sección Theming).
    let bg_color = 0x001B1111; // #11111b
    let text_color = 0x00F4D6CD; // #cdd6f4
    let mauve_color = 0x0085D256; // #56d285 — acento extraído del wallpaper activo (theming/colors.json)

    let bg_brush = CreateSolidBrush(COLORREF(bg_color));
    FillRect(mem_dc, &rc, bg_brush);
    DeleteObject(HGDIOBJ(bg_brush.0 as _));

    SetBkMode(mem_dc, TRANSPARENT);
    SetTextColor(mem_dc, COLORREF(text_color));

    // ANTIALIASED_QUALITY (no CLEARTYPE_QUALITY): ClearType asume que dibuja
    // directo a la pantalla real; al componer sobre este bitmap intermedio y
    // copiarlo con BitBlt, el subpixel-rendering de ClearType se ve con
    // fringing rojo/azul. El antialiasing en escala de grises no tiene ese problema.
    let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let hfont = CreateFontW(
        16, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0,
        DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32, VARIABLE_PITCH.0 as u32, PCWSTR(font_name.as_ptr())
    );
    let old_font = SelectObject(mem_dc, HGDIOBJ(hfont.0 as _));

    // 1. Workspaces (I..VII): un rectángulo fijo por cada uno, resaltando el
    //    activo con el color de acento.
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

        // Importante: NO se le agrega un '\0' final al buffer. DrawTextW en
        // este crate usa exactamente slice.len() como cantidad de caracteres
        // a dibujar/medir (cchText), así que un '\0' de más se cuenta como un
        // carácter real, corriendo el centrado y rompiendo el clipping.
        let mut label_w: Vec<u16> = label.encode_utf16().collect();
        DrawTextW(mem_dc, &mut label_w, &mut ws_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

        x_offset += ws_width + 8;
    }
    DeleteObject(HGDIOBJ(active_bg_brush.0 as _));

    // 2. Título de la ventana en foco (actualizado por win_event_proc). Se
    //    trunca con "..." si no entra antes de llegar al reloj centrado.
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

    // 3. Volumen: "Vol NN%" o "Mute", pegado al borde derecho.
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

    // 4. Reloj: centrado horizontalmente en toda la barra.
    let now = chrono::Local::now();
    let time_str = now.format("%H:%M:%S").to_string();
    let mut time_w: Vec<u16> = time_str.encode_utf16().collect();
    let mut clock_rc = clock_rect(width, height);
    DrawTextW(mem_dc, &mut time_w, &mut clock_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    // Copiamos todo el bitmap intermedio a la pantalla de una sola vez.
    BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

    // Liberar todos los recursos GDI creados en esta pasada.
    SelectObject(mem_dc, old_font);
    DeleteObject(HGDIOBJ(hfont.0 as _));
    SelectObject(mem_dc, old_bm);
    DeleteObject(HGDIOBJ(mem_bm.0 as _));
    DeleteDC(mem_dc);
    EndPaint(hwnd, &ps);
}

/// Hook global de Windows (SetWinEventHook) que se dispara cada vez que
/// cambia la ventana en foco. Actualiza el título mostrado en la barra y
/// decide si hay que ocultar la barra porque la nueva ventana está en
/// fullscreen.
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

        let main_hwnd_val = *MAIN_HWND.lock().unwrap();
        if main_hwnd_val != 0 {
            let main_hwnd = HWND(main_hwnd_val as *mut _);
            if hwnd != main_hwnd {
                let cmd = if is_fullscreen_window(hwnd) { SW_HIDE } else { SW_SHOWNOACTIVATE };
                ShowWindow(main_hwnd, cmd);
            }
            InvalidateRect(main_hwnd, None, false);
        }
    }
}

/// True si `hwnd` cubre por completo el monitor en el que está (fullscreen
/// real o borderless-fullscreen, como un video o un juego). Se usa para
/// decidir cuándo ocultar la barra. Excluye el escritorio (Progman/WorkerW),
/// que técnicamente también "cubre toda la pantalla" pero no cuenta como
/// contenido en fullscreen.
unsafe fn is_fullscreen_window(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut class_buf);
    let class_name = String::from_utf16_lossy(&class_buf[..len.max(0) as usize]);
    if class_name == "Progman" || class_name.starts_with("WorkerW") {
        return false;
    }

    let mut wnd_rc = RECT::default();
    if GetWindowRect(hwnd, &mut wnd_rc).is_err() {
        return false;
    }

    let hmonitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
        return false;
    }

    wnd_rc.left <= mi.rcMonitor.left
        && wnd_rc.top <= mi.rcMonitor.top
        && wnd_rc.right >= mi.rcMonitor.right
        && wnd_rc.bottom >= mi.rcMonitor.bottom
}

/// Hilo aparte que se queda escuchando los eventos de komorebi (workspace
/// activo, etc) por named pipe, usando el crate oficial `komorebi-client`
/// (el mismo que usa `komorebi-bar.exe`) en vez de reimplementar su
/// protocolo IPC a mano. Corre para siempre mientras el proceso viva,
/// resuscribiéndose si la suscripción falla o si komorebi se reinicia
/// (ambos casos son comunes: winbar puede arrancar antes que komorebi en el
/// login, o komorebi puede reiniciarse manualmente más tarde).
fn komorebi_thread() {
    let socket_name = "winbar_subscriber";
    loop {
        let listener = match komorebi_client::subscribe(socket_name) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to subscribe to komorebi: {}", e);
                thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
        };

        komorebi_listen_loop(&listener);
        // Si llegamos acá, komorebi cerró la conexión (reinicio, crash,
        // etc). Esperar un toque y volver a suscribirse.
        thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Procesa las notificaciones de un listener ya suscrito hasta que komorebi
/// cierra la conexión.
fn komorebi_listen_loop(listener: &komorebi_client::UnixListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let reader = BufReader::new(s);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        // komorebi manda un JSON grande por línea con TODO su
                        // estado; sólo nos interesa el workspace activo del
                        // monitor que también está en foco, así que navegamos
                        // el árbol a mano en vez de definir structs de serde
                        // para el estado completo (que cambia entre versiones).
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
        // Sin esto, en un monitor con escalado != 100% Windows "virtualiza"
        // las coordenadas que ve nuestro proceso (le muestra una resolución
        // más chica) y después estira el contenido dibujado para que cubra
        // el tamaño físico real. El resultado es que todo lo que dibujamos
        // (workspaces, reloj, volumen) aparece corrido/mal alineado en la
        // pantalla real aunque nuestras cuentas de píxeles sean correctas.
        // Declarándonos Per-Monitor-V2 DPI aware, GetSystemMetrics/
        // GetClientRect nos devuelven directamente la resolución física real
        // y todo se dibuja 1:1, sin que Windows reescale nada por su cuenta.
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

        // Ventana sin bordes ni barra de título (WS_POPUP), que no aparece en
        // la barra de tareas ni roba el foco al mostrarse (WS_EX_TOOLWINDOW),
        // y siempre por encima de las demás (WS_EX_TOPMOST). El tamaño real
        // lo termina fijando register_appbar() más abajo.
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

        // Hook global: nos avisa cada vez que cambia la ventana en foco en
        // todo el sistema (no sólo en nuestro proceso), para actualizar el
        // título mostrado y el chequeo de fullscreen.
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );

        // Tomar el título de la ventana en foco ya existente al arrancar
        // (si no, la barra queda con el título vacío hasta el próximo
        // cambio de foco).
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

        // Message loop estándar de Win32: sin esto la ventana no procesa
        // ningún mensaje (clicks, pintado, timers, etc) y queda congelada.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}

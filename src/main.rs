// winbar — barra nativa minimalista para Windows, dibujada a mano con GDI.
// Reemplazo liviano de Yasb: workspaces de komorebi, título de la ventana en
// foco, reloj, volumen, brillo, estado de red, Bloq Mayús/Num y un buscador
// de apps, todo en un solo binario sin frameworks de UI (nada de
// Qt/egui/webview) para mantener el uso de RAM bajo.
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
    Win32::Devices::Display::*,
    Win32::Foundation::*,
    Win32::Graphics::Gdi::*,
    Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator},
    Win32::NetworkManagement::IpHelper::{GetBestInterface, GetIfEntry2, MIB_IF_ROW2},
    Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED},
    Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
    Win32::UI::Shell::*,
    Win32::UI::WindowsAndMessaging::*,
    Win32::UI::Accessibility::*,
    Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL, VK_NUMLOCK},
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
    // (Bloq Mayús activo, Bloq Num activo) — se refresca en el timer de 1s.
    static ref KEYBOARD_STATE: Mutex<(bool, bool)> = Mutex::new((false, false));
    // (bytes/seg de bajada, bytes/seg de subida) de la interfaz que Windows
    // usa para salir a Internet, o None si no hay ninguna. Se refresca en el
    // timer de 1s.
    static ref NETWORK_STATE: Mutex<Option<(f64, f64)>> = Mutex::new(None);
    // Última lectura de contadores acumulados de bytes de esa interfaz
    // (índice, bytes entrantes, bytes salientes, instante), para poder
    // calcular la velocidad instantánea como delta contra la lectura
    // anterior. Se reinicia solo si cambia la interfaz activa (por ejemplo,
    // pasar de WiFi a Ethernet).
    static ref LAST_IF_COUNTERS: Mutex<Option<(u32, u64, u64, std::time::Instant)>> = Mutex::new(None);
    // Brillo del monitor primario en % (0-100), o None si no se pudo leer
    // (monitor sin DDC/CI, por ejemplo un panel interno de notebook). Lo
    // actualiza brightness_thread, nunca el hilo de UI directamente, porque
    // la llamada DDC/CI es una transacción I2C con el monitor y puede tardar
    // sensiblemente más que el resto de los refrescos.
    static ref BRIGHTNESS_STATE: Mutex<Option<u32>> = Mutex::new(None);
    // Extremo emisor del canal hacia brightness_thread, para pedirle "poné
    // el brillo en X%" desde el click sin bloquear el hilo de UI esperando
    // la respuesta del monitor.
    static ref BRIGHTNESS_TX: Mutex<Option<crossbeam_channel::Sender<u32>>> = Mutex::new(None);
}

// COM interfaces aren't Send; the endpoint volume handle is only ever touched
// from the main UI thread, so it lives in thread-local storage instead of a
// shared Mutex.
thread_local! {
    static ENDPOINT_VOLUME: RefCell<Option<IAudioEndpointVolume>> = RefCell::new(None);
}

// Ancho reservado para cada widget, en píxeles, y separación entre los que
// se apilan contra el borde derecho.
const VOLUME_RECT_WIDTH: i32 = 100;
const BRIGHTNESS_RECT_WIDTH: i32 = 100;
const NETWORK_RECT_WIDTH: i32 = 120;
const KEYBOARD_RECT_WIDTH: i32 = 90;
const CLOCK_WIDTH: i32 = 140;
const WIDGET_GAP: i32 = 8;

// Los widgets de la derecha se apilan de afuera hacia adentro (volumen
// pegado al borde, brillo a su izquierda, etc); cada offset es la distancia
// desde el borde derecho hasta el borde derecho de ESE widget, así que
// depende del ancho + separación de todos los que quedan más a la derecha.
const VOLUME_OFFSET: i32 = 20;
const BRIGHTNESS_OFFSET: i32 = VOLUME_OFFSET + VOLUME_RECT_WIDTH + WIDGET_GAP;
const NETWORK_OFFSET: i32 = BRIGHTNESS_OFFSET + BRIGHTNESS_RECT_WIDTH + WIDGET_GAP;
const KEYBOARD_OFFSET: i32 = NETWORK_OFFSET + NETWORK_RECT_WIDTH + WIDGET_GAP;

/// Rectángulo de un widget apilado contra el borde derecho de la barra.
/// `right_offset` es cuánto lugar dejan libre a la derecha los widgets que
/// van después de este (ver VOLUME_OFFSET y compañía).
fn right_widget_rect(width: i32, height: i32, right_offset: i32, rect_width: i32) -> RECT {
    RECT {
        left: width - right_offset - rect_width,
        top: 0,
        right: width - right_offset,
        bottom: height,
    }
}

/// Rectángulo del widget de volumen: el más pegado al borde derecho.
fn volume_rect(width: i32, height: i32) -> RECT {
    right_widget_rect(width, height, VOLUME_OFFSET, VOLUME_RECT_WIDTH)
}

/// Rectángulo del widget de brillo, a la izquierda del de volumen.
fn brightness_rect(width: i32, height: i32) -> RECT {
    right_widget_rect(width, height, BRIGHTNESS_OFFSET, BRIGHTNESS_RECT_WIDTH)
}

/// Rectángulo del widget de red, a la izquierda del de brillo.
fn network_rect(width: i32, height: i32) -> RECT {
    right_widget_rect(width, height, NETWORK_OFFSET, NETWORK_RECT_WIDTH)
}

/// Rectángulo del widget de Bloq Mayús/Bloq Num, el más a la izquierda de
/// la franja derecha.
fn keyboard_rect(width: i32, height: i32) -> RECT {
    right_widget_rect(width, height, KEYBOARD_OFFSET, KEYBOARD_RECT_WIDTH)
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

/// Fija el volumen a un nivel absoluto (click sobre el widget, tratado como
/// un mini slider: la posición horizontal del click dentro del widget es el
/// nuevo nivel), clampeado a 0-1.
fn set_volume_absolute(level: f32) {
    with_endpoint_volume(|ev| unsafe { ev.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null()) });
    refresh_volume_state();
}

/// Le pide a la barra que se repinte entera, desde cualquier thread (todos
/// comparten MAIN_HWND vía Mutex; HWND no es Send así que se guarda como
/// isize). No hace nada si la barra todavía no terminó de crearse.
fn invalidate_bar() {
    let hwnd_val = *MAIN_HWND.lock().unwrap();
    if hwnd_val != 0 {
        unsafe {
            InvalidateRect(HWND(hwnd_val as *mut _), None, false);
        }
    }
}

/// Ejecuta `f` con el handle del monitor físico primario, obtenido vía
/// DDC/CI (Dxva2), y lo destruye después sin importar el resultado. `None`
/// si el monitor no expone esta API (paneles internos de notebook, algunos
/// monitores por USB-C/DisplayLink) o si algo falla en el camino.
fn with_physical_monitor<R>(f: impl FnOnce(HANDLE) -> windows::core::Result<R>) -> Option<R> {
    unsafe {
        let hmonitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut count: u32 = 0;
        GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count).ok()?;
        if count == 0 {
            return None;
        }
        let mut monitors = vec![PHYSICAL_MONITOR::default(); count as usize];
        GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut monitors).ok()?;
        let result = f(monitors[0].hPhysicalMonitor).ok();
        let _ = DestroyPhysicalMonitors(&monitors);
        result
    }
}

/// Lee el brillo actual del monitor primario como porcentaje 0-100, o `None`
/// si no se pudo leer (ver `with_physical_monitor`). Se llama desde
/// `brightness_thread`, nunca desde el hilo de UI: es una transacción DDC/CI
/// por I2C con el monitor y puede tardar bastante más que el resto de los
/// refrescos de la barra.
fn get_brightness() -> Option<u32> {
    with_physical_monitor(|handle| unsafe {
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        if GetMonitorBrightness(handle, &mut min, &mut cur, &mut max) != 0 && max > min {
            Ok((cur - min) * 100 / (max - min))
        } else {
            Err(windows::core::Error::from_win32())
        }
    })
}

/// Fija el brillo del monitor primario a `pct` (0-100) vía DDC/CI. Igual que
/// `get_brightness`, solo se llama desde `brightness_thread`.
fn set_brightness(pct: u32) {
    with_physical_monitor(|handle| unsafe {
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        if GetMonitorBrightness(handle, &mut min, &mut cur, &mut max) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let target = min + (pct.min(100) * (max - min)) / 100;
        if SetMonitorBrightness(handle, target) != 0 {
            Ok(())
        } else {
            Err(windows::core::Error::from_win32())
        }
    });
}

/// Hilo aparte dedicado al brillo: aplica los cambios pedidos por click
/// (llegan por `rx`) y relee el valor real cada pocos segundos, todo fuera
/// del hilo de UI porque el DDC/CI puede tardar decenas o cientos de ms y no
/// queremos que la barra se sienta trabada mientras tanto.
fn brightness_thread(rx: crossbeam_channel::Receiver<u32>) {
    *BRIGHTNESS_STATE.lock().unwrap() = get_brightness();
    invalidate_bar();
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(pct) => set_brightness(pct),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        *BRIGHTNESS_STATE.lock().unwrap() = get_brightness();
        invalidate_bar();
    }
}

/// Relee el estado de Bloq Mayús / Bloq Num y actualiza KEYBOARD_STATE. El
/// bit bajo de GetKeyState es el de "toggled" (activo hasta la próxima vez
/// que se presione la tecla), a diferencia del bit alto que es "apretada
/// ahora mismo".
fn refresh_keyboard_state() {
    unsafe {
        let caps = (GetKeyState(VK_CAPITAL.0 as i32) & 0x0001) != 0;
        let num = (GetKeyState(VK_NUMLOCK.0 as i32) & 0x0001) != 0;
        *KEYBOARD_STATE.lock().unwrap() = (caps, num);
    }
}

/// Relee la velocidad de bajada/subida actual y actualiza NETWORK_STATE.
/// Primero le pregunta a la tabla de ruteo cuál interfaz usaría Windows
/// para salir a Internet ahora mismo (GetBestInterface con una IP pública
/// cualquiera como destino: no hace ningún request real, solo consulta la
/// tabla local) — evita el error común de mirar qué adaptador está "Up",
/// ya que adaptadores virtuales (Hyper-V, VPNs, VMware) suelen quedar "Up"
/// sin ser la conexión real. Con esa interfaz identificada, lee sus
/// contadores acumulados de bytes (GetIfEntry2) y calcula el delta contra
/// la lectura anterior (LAST_IF_COUNTERS) para obtener bytes/seg. Todo esto
/// es local (tabla de ruteo + contadores del driver), así que es seguro de
/// llamar en el hilo de UI igual que el resto de los refrescos del timer.
fn refresh_network_state() {
    let speed = unsafe {
        // 8.8.8.8 en network byte order: como los 4 octetos son iguales, da
        // lo mismo el endianness de la máquina.
        let mut best_ifindex: u32 = 0;
        if GetBestInterface(0x08080808, &mut best_ifindex) != 0 {
            None
        } else {
            let mut row = MIB_IF_ROW2::default();
            row.InterfaceIndex = best_ifindex;
            if GetIfEntry2(&mut row) != WIN32_ERROR(0) {
                None
            } else {
                let now = std::time::Instant::now();
                let mut last = LAST_IF_COUNTERS.lock().unwrap();
                let speed = match *last {
                    Some((idx, in_bytes, out_bytes, ts)) if idx == best_ifindex => {
                        let elapsed = now.duration_since(ts).as_secs_f64();
                        if elapsed > 0.0 {
                            (
                                row.InOctets.saturating_sub(in_bytes) as f64 / elapsed,
                                row.OutOctets.saturating_sub(out_bytes) as f64 / elapsed,
                            )
                        } else {
                            (0.0, 0.0)
                        }
                    }
                    // Primera lectura o cambio de interfaz: todavía no hay
                    // una lectura previa contra la cual calcular el delta.
                    _ => (0.0, 0.0),
                };
                *last = Some((best_ifindex, row.InOctets, row.OutOctets, now));
                Some(speed)
            }
        }
    };
    *NETWORK_STATE.lock().unwrap() = speed;
}

/// Da formato compacto a una velocidad en bytes/seg para que entre en el
/// ancho del widget de red: "0K" para prácticamente nada, "128K" en
/// kilobytes/seg, "1.2M" en megabytes/seg.
fn format_bps(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.1}M", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.0}K", bytes_per_sec / 1024.0)
    } else {
        "0K".to_string()
    }
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
            refresh_keyboard_state();
            refresh_network_state();
            search::register_hotkey(hwnd);
            search::warm_index(); // precarga el índice de apps para que el primer Alt+Space ya lo encuentre cacheado

            let (tx, rx) = crossbeam_channel::unbounded();
            *BRIGHTNESS_TX.lock().unwrap() = Some(tx);
            thread::spawn(move || brightness_thread(rx));

            SetTimer(hwnd, 1, 1000, None); // timer 1: refresca volumen/teclado/red + reloj cada 1s
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
                // Timer lento: releer volumen (por si cambió con las teclas
                // de multimedia), Bloq Mayús/Num y estado de red, y repintar
                // todo. El brillo NO se lee acá: vive en su propio thread
                // porque el DDC/CI puede tardar mucho más que esto (ver
                // brightness_thread).
                refresh_volume_state();
                refresh_keyboard_state();
                refresh_network_state();
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
            // Click izquierdo: si cae dentro del widget de volumen o de
            // brillo, tratarlo como un mini slider — la posición horizontal
            // del click dentro del widget pasa a ser el nuevo nivel (0% en
            // el borde izquierdo, 100% en el derecho), así se puede subir o
            // bajar de un solo click sin depender del scroll.
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);

            let vol_rc = volume_rect(rc.right - rc.left, rc.bottom);
            if x >= vol_rc.left && x < vol_rc.right && y >= vol_rc.top && y < vol_rc.bottom {
                let level = (x - vol_rc.left) as f32 / (vol_rc.right - vol_rc.left) as f32;
                set_volume_absolute(level);
                InvalidateRect(hwnd, Some(&vol_rc as *const _), false);
            }

            let bright_rc = brightness_rect(rc.right - rc.left, rc.bottom);
            if x >= bright_rc.left && x < bright_rc.right && y >= bright_rc.top && y < bright_rc.bottom {
                let pct = ((x - bright_rc.left) * 100 / (bright_rc.right - bright_rc.left)).clamp(0, 100) as u32;
                if let Some(tx) = BRIGHTNESS_TX.lock().unwrap().as_ref() {
                    let _ = tx.send(pct);
                }
                // Mostrar el nuevo valor de una vez, sin esperar a que
                // brightness_thread confirme el valor real por DDC/CI (que
                // puede tardar); el thread lo corrige solo apenas responde.
                *BRIGHTNESS_STATE.lock().unwrap() = Some(pct);
                InvalidateRect(hwnd, Some(&bright_rc as *const _), false);
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            // Click derecho sobre el widget de volumen: mutear/desmutear
            // (el izquierdo ahora se usa para fijar el nivel, ver arriba).
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

/// Dibuja una franja fina (2px) pegada al borde inferior de `rc`: un track
/// atenuado de fondo y, encima, un relleno de acento hasta `pct`%. Es lo que
/// le da a volumen y brillo pinta de mini slider, para saber de un vistazo
/// el nivel actual y dónde conviene clickear para subirlo o bajarlo.
unsafe fn draw_level_bar(mem_dc: HDC, rc: &RECT, pct: u32, dim_color: u32, accent_color: u32) {
    const BAR_HEIGHT: i32 = 2;
    const SIDE_MARGIN: i32 = 6;
    let track_rc = RECT {
        left: rc.left + SIDE_MARGIN,
        top: rc.bottom - BAR_HEIGHT - 3,
        right: rc.right - SIDE_MARGIN,
        bottom: rc.bottom - 3,
    };
    let track_brush = CreateSolidBrush(COLORREF(dim_color));
    FillRect(mem_dc, &track_rc, track_brush);
    DeleteObject(HGDIOBJ(track_brush.0 as _));

    let fill_width = (track_rc.right - track_rc.left) * pct.min(100) as i32 / 100;
    if fill_width > 0 {
        let fill_rc = RECT { right: track_rc.left + fill_width, ..track_rc };
        let fill_brush = CreateSolidBrush(COLORREF(accent_color));
        FillRect(mem_dc, &fill_rc, fill_brush);
        DeleteObject(HGDIOBJ(fill_brush.0 as _));
    }
}

/// Dibuja toda la barra: fondo, workspaces, título de la ventana en foco,
/// volumen, brillo, red, Bloq Mayús/Num y reloj. Usa double buffering manual
/// (se dibuja todo en un bitmap intermedio en memoria y se copia de una sola
/// vez a la pantalla con BitBlt) para evitar parpadeo.
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
    let dim_color = 0x0086706C; // #6c7086 (gris apagado, Catppuccin Mocha overlay0)

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

    // 3. Volumen: "Vol NN%" o "Mute", pegado al borde derecho. Click
    //    izquierdo fija el nivel según la posición, click derecho mutea.
    SetTextColor(mem_dc, COLORREF(text_color));
    let (volume_pct, is_muted) = *VOLUME_STATE.lock().unwrap();
    let volume_str = if is_muted {
        "Mute".to_string()
    } else {
        format!("Vol {}%", volume_pct)
    };
    let mut volume_rc = volume_rect(width, height);
    draw_level_bar(mem_dc, &volume_rc, volume_pct, dim_color, mauve_color);
    let mut volume_w: Vec<u16> = volume_str.encode_utf16().collect();
    DrawTextW(mem_dc, &mut volume_w, &mut volume_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    // 4. Brillo: "Brillo NN%", o "Brillo N/D" si el monitor no expone DDC/CI
    //    (paneles internos de notebook, algunos monitores por USB-C). Click
    //    izquierdo fija el nivel según la posición, igual que el volumen.
    SetTextColor(mem_dc, COLORREF(text_color));
    let brightness_str = match *BRIGHTNESS_STATE.lock().unwrap() {
        Some(pct) => format!("Brillo {}%", pct),
        None => "Brillo N/D".to_string(),
    };
    let mut brightness_rc = brightness_rect(width, height);
    if let Some(pct) = *BRIGHTNESS_STATE.lock().unwrap() {
        draw_level_bar(mem_dc, &brightness_rc, pct, dim_color, mauve_color);
    }
    let mut brightness_w: Vec<u16> = brightness_str.encode_utf16().collect();
    DrawTextW(mem_dc, &mut brightness_w, &mut brightness_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    // 5. Red: velocidad de bajada/subida de la interfaz activa, cada una en
    //    su mitad del widget con una flechita. "Sin red" resaltado en rojo,
    //    ocupando el widget entero, si no hay ninguna interfaz con salida a
    //    Internet.
    let disconnected_color = 0x00A88BF3; // #f38ba8 (rojo Catppuccin Mocha)
    let network_rc = network_rect(width, height);
    match *NETWORK_STATE.lock().unwrap() {
        Some((down_bps, up_bps)) => {
            SetTextColor(mem_dc, COLORREF(text_color));
            let half_w = (network_rc.right - network_rc.left) / 2;
            let mut down_rc = RECT { left: network_rc.left, top: 0, right: network_rc.left + half_w, bottom: height };
            let mut up_rc = RECT { left: network_rc.left + half_w, top: 0, right: network_rc.right, bottom: height };
            let mut down_w: Vec<u16> = format!("\u{2193}{}", format_bps(down_bps)).encode_utf16().collect();
            DrawTextW(mem_dc, &mut down_w, &mut down_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            let mut up_w: Vec<u16> = format!("\u{2191}{}", format_bps(up_bps)).encode_utf16().collect();
            DrawTextW(mem_dc, &mut up_w, &mut up_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
        None => {
            SetTextColor(mem_dc, COLORREF(disconnected_color));
            let mut no_net_rc = network_rc;
            let mut label_w: Vec<u16> = "Sin red".encode_utf16().collect();
            DrawTextW(mem_dc, &mut label_w, &mut no_net_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
    }

    // 6. Bloq Mayús / Bloq Num: cada uno en su mitad del widget, resaltado
    //    con el color de acento cuando está activo y atenuado si no.
    let (caps_on, num_on) = *KEYBOARD_STATE.lock().unwrap();
    let kb_rc = keyboard_rect(width, height);
    let half_w = (kb_rc.right - kb_rc.left) / 2;
    let mut caps_rc = RECT { left: kb_rc.left, top: 0, right: kb_rc.left + half_w, bottom: height };
    let mut num_rc = RECT { left: kb_rc.left + half_w, top: 0, right: kb_rc.right, bottom: height };
    SetTextColor(mem_dc, COLORREF(if caps_on { mauve_color } else { dim_color }));
    let mut caps_w: Vec<u16> = "CAPS".encode_utf16().collect();
    DrawTextW(mem_dc, &mut caps_w, &mut caps_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SetTextColor(mem_dc, COLORREF(if num_on { mauve_color } else { dim_color }));
    let mut num_w: Vec<u16> = "NUM".encode_utf16().collect();
    DrawTextW(mem_dc, &mut num_w, &mut num_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

    // 7. Reloj: centrado horizontalmente en toda la barra.
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

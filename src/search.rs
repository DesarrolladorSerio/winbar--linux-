// Buscador de apps (Fase 3 del plan): popup que se abre con Alt+Space,
// equivalente simple al Alt+Space/quick_launch que traía Yasb.
//
// Indexa los accesos directos (.lnk) del Start Menu, deja filtrar por
// substring escribiendo, y lanza el seleccionado con Enter o click. Todo el
// popup (campo de texto + lista) se dibuja a mano con GDI, salvo el propio
// cuadro de texto que es un control EDIT nativo de Windows (subclaseado para
// poder interceptar flechas/Enter/Escape antes de que los use para mover el
// cursor de texto).

use std::mem;
use std::path::Path;
use std::sync::Mutex;

use lazy_static::lazy_static;

use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::Graphics::Gdi::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::UI::Input::KeyboardAndMouse::*,
    Win32::UI::Shell::*,
    Win32::UI::WindowsAndMessaging::*,
};

const SEARCH_CLASS_NAME: PCWSTR = w!("WinBarSearchClass");
const EDIT_CONTROL_ID: i32 = 101;
pub const HOTKEY_ID: i32 = 1;
const POPUP_WIDTH: i32 = 480;
const POPUP_HEIGHT: i32 = 360;
const ROW_HEIGHT: i32 = 28;
const EDIT_HEIGHT: i32 = 32;
const MAX_RESULTS: usize = 10;

lazy_static! {
    // Lista completa de apps indexadas: (nombre a mostrar, path al .lnk).
    static ref APPS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    // Índices dentro de APPS que matchean el filtro actual (lo que se ve en pantalla).
    static ref FILTERED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    // Fila resaltada dentro de FILTERED (se mueve con las flechas).
    static ref SELECTED: Mutex<usize> = Mutex::new(0);
    // HWND del popup, 0 si está cerrado. Mismo truco que MAIN_HWND en main.rs
    // (HWND no es Send, se guarda como isize).
    static ref SEARCH_HWND: Mutex<isize> = Mutex::new(0);
    // HWND del control EDIT hijo del popup.
    static ref EDIT_HWND: Mutex<isize> = Mutex::new(0);
    // WNDPROC original del control EDIT, para poder reenviarle los mensajes
    // que no nos interesa interceptar (subclassing manual, sin comctl32).
    static ref ORIG_EDIT_PROC: Mutex<isize> = Mutex::new(0);
    // Brush cacheado para el fondo oscuro del EDIT (WM_CTLCOLOREDIT); se crea
    // una sola vez y se reusa, en vez de crear uno nuevo en cada repintado.
    static ref EDIT_BG_BRUSH: Mutex<isize> = Mutex::new(0);
}

/// Recorre las carpetas del Start Menu (la de todo el sistema y la del
/// usuario actual) buscando accesos directos (.lnk), y arma la lista de
/// resultados ordenada alfabéticamente. Se llama de nuevo cada vez que se
/// abre el popup, así que si instalás/desinstalás algo se refleja solo.
fn index_apps() -> Vec<(String, String)> {
    let mut results = Vec::new();
    for var in ["ProgramData", "AppData"] {
        if let Ok(base) = std::env::var(var) {
            let dir = Path::new(&base).join("Microsoft\\Windows\\Start Menu\\Programs");
            walk_dir(&dir, &mut results);
        }
    }
    results.sort_by(|a: &(String, String), b: &(String, String)| {
        a.0.to_lowercase().cmp(&b.0.to_lowercase())
    });
    results
}

/// Recorrida recursiva de directorios buscando archivos .lnk. Guarda el
/// nombre del archivo sin extensión como nombre para mostrar, y el path
/// completo al .lnk (que es lo que se le pasa a ShellExecuteW para lanzarlo,
/// sin necesidad de resolver el acceso directo a su target real).
fn walk_dir(dir: &Path, out: &mut Vec<(String, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, out);
        } else if path
            .extension()
            .map_or(false, |ext| ext.eq_ignore_ascii_case("lnk"))
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((stem.to_string(), path.to_string_lossy().to_string()));
            }
        }
    }
}

/// Recalcula FILTERED según `query` (substring case-insensitive sobre el
/// nombre de cada app) y resetea la selección a la primera fila. Con el
/// campo vacío muestra las primeras MAX_RESULTS apps sin filtrar.
fn apply_filter(query: &str) {
    let apps = APPS.lock().unwrap();
    let query_lower = query.to_lowercase();
    let mut filtered = FILTERED.lock().unwrap();
    filtered.clear();
    if query_lower.is_empty() {
        filtered.extend(0..apps.len().min(MAX_RESULTS));
    } else {
        filtered.extend(
            apps.iter()
                .enumerate()
                .filter(|(_, (name, _))| name.to_lowercase().contains(&query_lower))
                .map(|(i, _)| i)
                .take(MAX_RESULTS),
        );
    }
    *SELECTED.lock().unwrap() = 0;
}

/// Lanza la app actualmente seleccionada (Enter o click) vía ShellExecuteW
/// sobre su .lnk, como si se le hubiera hecho doble click desde el Explorador.
fn launch_selected() {
    let idx = *SELECTED.lock().unwrap();
    let filtered = FILTERED.lock().unwrap();
    let apps = APPS.lock().unwrap();
    if let Some(&app_idx) = filtered.get(idx) {
        if let Some((_, path)) = apps.get(app_idx) {
            // Acá sí hace falta el '\0' final: ShellExecuteW espera un
            // PCWSTR (string null-terminated de estilo C), no un slice con
            // longitud explícita como DrawTextW.
            let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                ShellExecuteW(
                    None,
                    w!("open"),
                    PCWSTR(path_w.as_ptr()),
                    None,
                    None,
                    SW_SHOWNORMAL,
                );
            }
        }
    }
}

/// Punto de entrada llamado desde main.rs cuando se presiona Alt+Space:
/// cierra el popup si ya está abierto, o lo abre si no.
pub fn toggle_search(main_hwnd: HWND) {
    let hwnd_val = *SEARCH_HWND.lock().unwrap();
    if hwnd_val != 0 {
        unsafe {
            close_search(HWND(hwnd_val as *mut _));
        }
    } else {
        unsafe {
            open_search(main_hwnd);
        }
    }
}

/// Crea (o recrea) la ventana del popup, centrada en la pantalla, e indexa
/// las apps de nuevo. El popup se destruye por completo al cerrarse (no se
/// oculta y reusa), así que cada apertura vuelve a pasar por acá.
unsafe fn open_search(_main_hwnd: HWND) {
    *APPS.lock().unwrap() = index_apps();
    apply_filter("");

    let instance = GetModuleHandleW(None).unwrap();

    let wc = WNDCLASSW {
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
        hInstance: instance.into(),
        lpszClassName: SEARCH_CLASS_NAME,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(search_window_proc),
        ..Default::default()
    };
    // Ignore the result: RegisterClassW fails harmlessly if already registered
    // from a previous time the popup was opened.
    RegisterClassW(&wc);

    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);
    let x = (screen_w - POPUP_WIDTH) / 2;
    let y = (screen_h - POPUP_HEIGHT) / 3;

    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        SEARCH_CLASS_NAME,
        PCWSTR(std::ptr::null()),
        WS_POPUP | WS_VISIBLE | WS_BORDER,
        x,
        y,
        POPUP_WIDTH,
        POPUP_HEIGHT,
        None,
        None,
        instance,
        None,
    )
    .unwrap();

    *SEARCH_HWND.lock().unwrap() = hwnd.0 as isize;
    let _ = SetForegroundWindow(hwnd);
}

/// Destruye el popup y limpia el estado asociado. Se llama al lanzar una
/// app, apretar Escape, o al perder el foco (click afuera del popup).
unsafe fn close_search(hwnd: HWND) {
    let _ = DestroyWindow(hwnd);
    *SEARCH_HWND.lock().unwrap() = 0;
    *EDIT_HWND.lock().unwrap() = 0;
}

/// WNDPROC de reemplazo para el control EDIT (subclassing manual vía
/// SetWindowLongPtrW/GWLP_WNDPROC). El EDIT nativo de Windows por defecto usa
/// las flechas para mover el cursor de texto y no hace nada con Enter/Escape;
/// acá se interceptan esas teclas para navegar la lista de resultados y
/// lanzar/cerrar, y todo lo demás (letras, backspace, etc) se reenvía sin
/// tocar al procedimiento original guardado en ORIG_EDIT_PROC.
unsafe extern "system" fn edit_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_KEYDOWN {
        let vk = wparam.0 as u16;
        let parent = GetParent(hwnd).ok();
        if vk == VK_DOWN.0 {
            let filtered_len = FILTERED.lock().unwrap().len();
            if filtered_len > 0 {
                let mut sel = SELECTED.lock().unwrap();
                *sel = (*sel + 1).min(filtered_len - 1);
            }
            if let Some(p) = parent {
                InvalidateRect(p, None, false);
            }
            return LRESULT(0);
        } else if vk == VK_UP.0 {
            let mut sel = SELECTED.lock().unwrap();
            if *sel > 0 {
                *sel -= 1;
            }
            drop(sel);
            if let Some(p) = parent {
                InvalidateRect(p, None, false);
            }
            return LRESULT(0);
        } else if vk == VK_RETURN.0 {
            launch_selected();
            if let Some(p) = parent {
                close_search(p);
            }
            return LRESULT(0);
        } else if vk == VK_ESCAPE.0 {
            if let Some(p) = parent {
                close_search(p);
            }
            return LRESULT(0);
        }
    }
    let orig = *ORIG_EDIT_PROC.lock().unwrap();
    let orig_proc: WNDPROC = mem::transmute(orig);
    CallWindowProcW(orig_proc, hwnd, msg, wparam, lparam)
}

/// Procedimiento de ventana del popup del buscador.
unsafe extern "system" fn search_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            // Crear el control EDIT nativo donde se escribe el filtro, y
            // subclasearlo para poder capturar flechas/Enter/Escape (ver
            // edit_subclass_proc).
            let instance = GetModuleHandleW(None).unwrap();
            let edit_hwnd = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                PCWSTR(std::ptr::null()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                8,
                8,
                POPUP_WIDTH - 16,
                EDIT_HEIGHT - 12,
                hwnd,
                HMENU(EDIT_CONTROL_ID as isize as *mut _),
                instance,
                None,
            )
            .unwrap();

            let old_proc =
                SetWindowLongPtrW(edit_hwnd, GWLP_WNDPROC, edit_subclass_proc as usize as isize);
            *ORIG_EDIT_PROC.lock().unwrap() = old_proc;
            *EDIT_HWND.lock().unwrap() = edit_hwnd.0 as isize;

            let _ = SetFocus(edit_hwnd);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            // El EDIT nativo se pintaría blanco por defecto; acá se le fuerza
            // el mismo esquema de colores oscuro que el resto de la barra.
            let hdc = HDC(wparam.0 as *mut _);
            SetTextColor(hdc, COLORREF(0x00F4D6CD));
            SetBkColor(hdc, COLORREF(0x00291B1B));
            let mut brush = EDIT_BG_BRUSH.lock().unwrap();
            if *brush == 0 {
                *brush = CreateSolidBrush(COLORREF(0x00291B1B)).0 as isize;
            }
            LRESULT(*brush)
        }
        WM_COMMAND => {
            // Notificación EN_CHANGE del EDIT: el texto cambió, re-filtrar.
            let notify_code = (wparam.0 >> 16) as u32;
            if notify_code == EN_CHANGE {
                let edit_val = *EDIT_HWND.lock().unwrap();
                let edit_hwnd = HWND(edit_val as *mut _);
                let len = GetWindowTextLengthW(edit_hwnd);
                let mut buf = vec![0u16; (len + 1) as usize];
                GetWindowTextW(edit_hwnd, &mut buf);
                let text = String::from_utf16_lossy(&buf[..len as usize]);
                apply_filter(&text);
                InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Click sobre una fila de la lista: seleccionarla y lanzarla.
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if y > EDIT_HEIGHT {
                let row = (y - EDIT_HEIGHT) / ROW_HEIGHT;
                let filtered_len = FILTERED.lock().unwrap().len();
                if row >= 0 && (row as usize) < filtered_len {
                    *SELECTED.lock().unwrap() = row as usize;
                    launch_selected();
                    close_search(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // El popup se cierra solo al perder el foco (click afuera),
            // como cualquier launcher tipo Spotlight/Alt+Space.
            let active = (wparam.0 & 0xFFFF) as u32;
            if active == WA_INACTIVE {
                close_search(hwnd);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            draw_search(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Dibuja la lista de resultados filtrados debajo del campo de texto (el
/// campo en sí es un control nativo, no se dibuja acá). Mismo patrón de
/// double buffering que draw() en main.rs.
unsafe fn draw_search(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    GetClientRect(hwnd, &mut rc);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;

    let mem_dc = CreateCompatibleDC(hdc);
    let mem_bm = CreateCompatibleBitmap(hdc, width, height);
    let old_bm = SelectObject(mem_dc, HGDIOBJ(mem_bm.0 as _));

    let bg_color = 0x001B1111; // #11111b
    let text_color = 0x00F4D6CD; // #cdd6f4
    let mauve_color = 0x0085D256; // #56d285 — acento extraído del wallpaper activo (theming/colors.json)

    let bg_brush = CreateSolidBrush(COLORREF(bg_color));
    FillRect(mem_dc, &rc, bg_brush);
    DeleteObject(HGDIOBJ(bg_brush.0 as _));

    SetBkMode(mem_dc, TRANSPARENT);

    let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let hfont = CreateFontW(
        15,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        VARIABLE_PITCH.0 as u32,
        PCWSTR(font_name.as_ptr()),
    );
    let old_font = SelectObject(mem_dc, HGDIOBJ(hfont.0 as _));

    let apps = APPS.lock().unwrap();
    let filtered = FILTERED.lock().unwrap();
    let selected = *SELECTED.lock().unwrap();
    let active_bg_brush = CreateSolidBrush(COLORREF(mauve_color));

    for (row, &app_idx) in filtered.iter().enumerate() {
        let mut row_rc = RECT {
            left: 8,
            top: EDIT_HEIGHT + row as i32 * ROW_HEIGHT,
            right: width - 8,
            bottom: EDIT_HEIGHT + (row as i32 + 1) * ROW_HEIGHT,
        };
        if row == selected {
            FillRect(mem_dc, &row_rc, active_bg_brush);
            SetTextColor(mem_dc, COLORREF(bg_color));
        } else {
            SetTextColor(mem_dc, COLORREF(text_color));
        }
        if let Some((name, _)) = apps.get(app_idx) {
            row_rc.left += 8;
            // Sin '\0' al final por el mismo motivo que en main.rs: DrawTextW
            // usa slice.len() como cchText.
            let mut name_w: Vec<u16> = name.encode_utf16().collect();
            DrawTextW(
                mem_dc,
                &mut name_w,
                &mut row_rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
    }
    DeleteObject(HGDIOBJ(active_bg_brush.0 as _));

    BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

    SelectObject(mem_dc, old_font);
    DeleteObject(HGDIOBJ(hfont.0 as _));
    SelectObject(mem_dc, old_bm);
    DeleteObject(HGDIOBJ(mem_bm.0 as _));
    DeleteDC(mem_dc);
    EndPaint(hwnd, &ps);
}

/// Registra Alt+Space como hotkey global (RegisterHotKey): funciona sin
/// importar qué ventana tenga el foco en ese momento. Si Yasb (u otra app)
/// ya tiene Alt+Space registrado, esta llamada falla silenciosamente y el
/// atajo simplemente no hace nada hasta que se libere.
pub fn register_hotkey(main_hwnd: HWND) {
    unsafe {
        let _ = RegisterHotKey(main_hwnd, HOTKEY_ID, MOD_ALT, VK_SPACE.0 as u32);
    }
}

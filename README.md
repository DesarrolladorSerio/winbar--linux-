# winbar

Barra nativa mínima para Windows, hecha a mano en Rust + Win32/GDI, pensada
como reemplazo liviano de [Yasb](https://github.com/AmN23/yasb): mismo lugar
en pantalla, mismas cosas básicas, pero bajando el uso de RAM de ~180 MB a
~15-20 MB. No usa Qt, egui ni webview — todo se dibuja directo con GDI sobre
una ventana registrada como Windows AppBar.

## Qué hace

Solo estas 5 cosas, nada más (a propósito — ver [Fuera de alcance](#fuera-de-alcance)):

1. **Indicador de workspace activo** (I–VII), sincronizado en vivo con
   [komorebi](https://github.com/LGUG2Z/komorebi) vía su crate oficial.
2. **Nombre de la ventana en foco**, actualizado en tiempo real.
3. **Reloj**, centrado en la barra.
4. **Volumen**: nivel + estado de mute. Click sobre el widget togglea mute,
   scroll sube/baja el volumen en pasos de 5%.
5. **Buscador de apps**: `Alt+Space` abre un popup que indexa los accesos
   directos del Start Menu, filtra en vivo mientras escribís, y lanza la app
   seleccionada con Enter o click.

Además, la barra se oculta sola cuando hay una ventana en pantalla completa
(video, juego, etc.) y vuelve a aparecer al salir, sin taparla.

## Capturas

*(no incluidas en el repo — corré `cargo run` y mirá la franja superior de
tu pantalla principal)*

## Por qué existe

[Yasb](https://github.com/AmN23/yasb) hace todo esto y más, pero corre sobre
Python + Qt y usa ~180 MB de RAM solo para mostrar texto e íconos en una
franja de 32px. `winbar` cubre el subconjunto de funciones que realmente se
usan, dibujando todo a mano con GDI (el mismo tipo de render de bajo nivel
que usa `whkd`), sin ningún framework de UI de por medio.

## Requisitos

- **Windows 10/11**, con [Rust](https://rustup.rs/) instalado (toolchain
  `stable-msvc`; hace falta tener las Build Tools de Visual Studio, que trae
  el propio instalador de Rust si no las detecta).
- [komorebi](https://github.com/LGUG2Z/komorebi) corriendo, para que el
  indicador de workspaces tenga algo de qué mostrar. Sin komorebi corriendo,
  ese indicador simplemente se queda marcando siempre "I" (no rompe nada).
- Nada más — el volumen usa Core Audio (built-in de Windows), el buscador
  lee directamente las carpetas del Start Menu.

## Compilar

```powershell
git clone <este-repo>
cd winbar
cargo build --release
```

El binario queda en `target\release\winbar.exe`. Es un único `.exe`
standalone (~570 KB), no necesita instalar nada más para correr.

## Instalar

No hay instalador; es copiar el `.exe` a algún lado estable y (opcionalmente)
darle autostart. Por ejemplo:

```powershell
# 1. Copiar el binario a una ubicación fija
New-Item -ItemType Directory -Force -Path "$env:LocalAppData\winbar"
Copy-Item target\release\winbar.exe "$env:LocalAppData\winbar\winbar.exe"

# 2. Arrancarlo
& "$env:LocalAppData\winbar\winbar.exe"

# 3. (Opcional) que arranque solo con Windows
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
  -Name "winbar" -Value "$env:LocalAppData\winbar\winbar.exe"
```

### Migrar desde Yasb

Si venís de Yasb y querés que `winbar` lo reemplace del todo:

```powershell
# Sacar a Yasb del autostart y cerrarlo
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "YASB"
Stop-Process -Name yasb -Force
```

`Alt+Space` es un hotkey global: si Yasb (o cualquier otra app) ya lo tiene
registrado, el de `winbar` simplemente no hace nada hasta que se libere —
andá a cerrar/desregistrar el que lo tenga tomado primero si el buscador no
abre.

## Uso

| Acción | Resultado |
|---|---|
| `Alt+Space` | Abre/cierra el buscador de apps |
| Escribir en el buscador | Filtra la lista en vivo (substring, sin distinguir mayúsculas) |
| `↑` / `↓` en el buscador | Mueve la selección |
| `Enter` / click en una fila | Lanza la app seleccionada y cierra el popup |
| `Esc` / click afuera del popup | Cierra el buscador sin lanzar nada |
| Click sobre el widget de volumen | Mutea/desmutea |
| Scroll sobre el widget de volumen | Sube/baja el volumen 5% por paso |

## Theming

Los colores van hardcodeados en el código (`src/main.rs` y `src/search.rs`,
constantes `bg_color` / `text_color` / `mauve_color` dentro de `draw()`), no
hay archivo de config. La paleta base es Catppuccin Mocha; el color de
acento (`mauve_color`, usado para resaltar el workspace activo y la fila
seleccionada del buscador) se pensó para sincronizarse con el wallpaper
activo:

- `~/.config/theming/extract_palette.py` saca el color más vívido del
  wallpaper activo de Wallpaper Engine y lo guarda en
  `~/.config/theming/colors.json` (campo `accent_rgb`).
- Para aplicar un cambio de wallpaper a `winbar`, hay que convertir ese RGB
  a `COLORREF` (formato `0x00BBGGRR`, o sea con los bytes de color al revés)
  y pegarlo a mano en las constantes `mauve_color` de `main.rs` y
  `search.rs`, y recompilar. No es automático todavía — `~/.config/theming/apply_wallpaper_theme.ps1`
  hace este mismo paso para Yasb (edita su CSS y lo reinicia); extenderlo
  para que también parchee y recompile `winbar` es una mejora pendiente.

## Estado del proyecto / arquitectura

Las decisiones técnicas y el detalle de cada módulo están comentados
directamente en el código (`src/main.rs` y `src/search.rs`), que además es
corto (~500 y ~360 líneas respectivamente) y vale la pena leer de punta a
punta si querés tocar algo. Un resumen rápido:

- **`src/main.rs`**: ventana principal de la barra. Registro como AppBar
  (`SHAppBarMessage`), render con double buffering manual (GDI + bitmap
  intermedio), integración con komorebi por named pipe en un thread aparte,
  Core Audio vía COM para el volumen, hook global de foreground
  (`SetWinEventHook`) para el título de ventana y la detección de
  fullscreen.
- **`src/search.rs`**: el popup del buscador de apps (Alt+Space), en su
  propio módulo. Indexado del Start Menu, filtro, y un control `EDIT`
  nativo subclaseado a mano para poder navegar la lista con las flechas.

Un detalle no obvio si tocás el layout: el proceso se declara
Per-Monitor-V2 DPI aware (`SetProcessDpiAwarenessContext` en `main()`) a
propósito. Sin eso, en cualquier monitor con escalado distinto de 100%
Windows "virtualiza" la resolución que ve el proceso y estira el contenido
dibujado para cubrir el tamaño físico real — todo el layout (workspaces,
reloj, volumen) termina corrido/desalineado en la pantalla real aunque las
cuentas de píxeles en el código sean correctas. Si algún día algo se ve
desalineado de nuevo, esa declaración es lo primero para revisar.

## Fuera de alcance (a propósito)

CPU/memoria, notificaciones, media player, power menu, clima, calendario,
systray, popups con blur/animaciones, theming vía archivo de config (por
ahora). La idea es cubrir justo lo que se usa a diario y nada más, para que
el binario se mantenga chico y simple.

## Limitaciones conocidas

- Pensado y probado para **un solo monitor**. La detección de fullscreen y
  el registro del AppBar asumen el monitor primario.
- El color de acento del wallpaper no se aplica solo (ver
  [Theming](#theming)) — hay que editar el código y recompilar.
- El buscador no cachea el índice entre aperturas: cada `Alt+Space` vuelve a
  leer las carpetas del Start Menu. Es rápido (son pocas decenas de
  archivos), pero no es instantáneo si el disco está muy ocupado.

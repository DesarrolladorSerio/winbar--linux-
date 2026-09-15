# winbar — plan de desarrollo

Barra nativa mínima para Windows, hecha a mano en Rust + Win32/GDI, para
reemplazar a Yasb y bajar el uso de RAM de ~180MB a un rango de ~10-15MB
(similar a whkd). Sin Qt, sin egui, sin webview — dibujo directo con GDI/GDI+
sobre una ventana registrada como Windows AppBar.

## Alcance (y lo que NO entra)

Solo estas 5 cosas, nada más:

1. Indicador de workspace activo (I–VII, sincronizado con `Win+1..8` de komorebi)
2. Nombre de la ventana/app en foco
3. Reloj
4. Volumen (nivel + mute, click para abrir el mixer o togglear mute)
5. Buscador de apps (reemplazo simple del `Alt+Space` actual)

Explícitamente fuera de alcance: CPU/memoria, notificaciones, media player,
power menu, clima, calendario, systray, popups con blur/animaciones,
theming vía archivo de config (los colores van hardcodeados al principio).

## Decisiones técnicas

- **Lenguaje**: Rust.
- **Render**: GDI/GDI+ con double buffering manual (`BeginPaint`/bitmap
  intermedio) para evitar parpadeo. Nada de contexto GPU (eso es lo que hace
  pesado a egui/Qt).
- **Integración con komorebi**: usar el crate oficial `komorebi-client`
  (el mismo que usa `komorebi-bar.exe`) para suscribirse a los eventos de
  workspace por named pipe, en vez de reimplementar el protocolo IPC a mano.
- **Ventana activa**: `SetWinEventHook` con `EVENT_SYSTEM_FOREGROUND` +
  `GetWindowText`.
- **Volumen**: Core Audio API vía COM (`IMMDeviceEnumerator` /
  `IAudioEndpointVolume`), con el crate `windows` (bindings oficiales de
  Microsoft).
- **Buscador**: enumerar accesos directos de
  `%ProgramData%\Microsoft\Windows\Start Menu\Programs` y
  `%AppData%\Microsoft\Windows\Start Menu\Programs`, filtro simple por
  substring/fuzzy, ventana popup propia (edit control + lista dibujada a
  mano), `ShellExecute` para lanzar.
- **Bar como AppBar real**: `SHAppBarMessage` (igual que hace Yasb con
  `windows_app_bar: true`) para reservar el espacio arriba correctamente y
  evitar el bug de offset fantasma que ya nos pasó una vez.
- **Config**: nada de YAML/CSS por ahora. Colores y layout van constantes en
  el código (paleta Catppuccin Mocha + el acento dinámico que ya generamos
  con el script de wallpaper). Se puede agregar un config file más adelante
  si hace falta.

## Fases

### Fase 1 — Esqueleto + lo más simple (workspaces, ventana activa, reloj)
- Crear proyecto Rust, dependencias (`windows`, `komorebi-client`)
- Ventana top-level sin bordes, anclada arriba, registrada como AppBar
- Loop de render con double buffering
- Reloj (timer de 1s, redibuja solo esa zona)
- Suscripción a komorebi vía `komorebi-client`, dibujar indicador de
  workspace (I–VII) resaltando el activo
- Hook de ventana en foco, dibujar su título
- **Entregable**: barra visualmente equivalente a lo que tenés hoy menos
  buscador y volumen, corriendo con RAM medida y comparada contra Yasb

### Fase 2 — Volumen
- Integración COM con Core Audio
- Ícono + porcentaje, click para mute, scroll para subir/bajar
- **Entregable**: widget de volumen funcional

### Fase 3 — Buscador
- Indexar accesos directos del Start Menu (con cache simple en memoria,
  refrescado al abrir el buscador)
- Popup con campo de texto + lista filtrada
- Atajo global (retomar `Alt+Space` o el que prefieras) para abrir/cerrar
- **Entregable**: reemplazo completo del quick_launch de Yasb

### Fase 4 — Pulido y reemplazo definitivo
- Autostart (entrada en Run key, como con Yasb)
- Manejo de multi-monitor si aplica
- Apagar/desinstalar Yasb una vez validado que winbar cubre todo
- Comparativa final de RAM (antes/después) y captura de pantalla

## Riesgos conocidos

- El buscador (Fase 3) es la pieza más grande: UI de texto + lista dibujada
  a mano en GDI es más trabajo que las demás partes juntas.
- Sin motor de layout como Qt, cualquier ajuste visual (alinear texto,
  paddings) es manual y más lento de iterar.
- `komorebi-client` es la dependencia externa más importante — si cambia su
  API entre versiones de komorebi, puede requerir ajustes.

## Estimación

~2-3 días de trabajo enfocado para las Fases 1-3. Fase 4 es incremental y
liviana una vez que lo anterior funciona.

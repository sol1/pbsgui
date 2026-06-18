# Application icons

These files are not committed yet. Generate them from a single source image
(1024x1024 PNG recommended) with the Tauri CLI:

    cargo tauri icon path/to/logo.png

That produces `32x32.png`, `128x128.png`, `icon.ico`, and the other sizes
referenced by `bundle.icon` in `tauri.conf.json`. The Windows build (`cargo tauri
build`) needs `icon.ico` to exist.

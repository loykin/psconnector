# Assets

- `Phosphor.ttf`: the regular weight of the [Phosphor](https://phosphoricons.com) icon font (MIT), as shipped in
  egui-phosphor 0.14.0 (MIT OR Apache-2.0). It is embedded into the viewer for the icons in `theme::icon`
  (`src/ui/theme.rs`; codepoints in `design/tokens.json`). The `egui-phosphor` crate itself targets egui 0.36, so only the font is used.
- `appicon.swift`: draws the app icon (a dark rounded tile, a blue rounded play mark, two signal arcs).
  `scripts/make-icons.sh` renders it and writes the three files below; run it on macOS after changing the
  drawing and commit them (builds only use the files):
  - `icon.png`: 256×256, the window icon set at run time (window, Dock while running, taskbar)
  - `icon.icns`: the macOS app bundle icon, used by `scripts/macos-app.sh`
  - `icon.ico`: the Windows .exe file icon, embedded by `build.rs` when building on Windows

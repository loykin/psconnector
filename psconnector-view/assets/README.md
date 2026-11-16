# Assets

- `Phosphor.ttf`: the regular weight of the [Phosphor](https://phosphoricons.com) icon font (MIT), as shipped in
  egui-phosphor 0.14.0 (MIT OR Apache-2.0). It is embedded into the viewer for the icons in `theme::icon`
  (`src/theme.rs`). The `egui-phosphor` crate itself targets egui 0.36, so only the font is used.
- `icon.png`: the app icon (window, Dock, taskbar), 256×256, drawn by `appicon.swift`
  (`swift appicon.swift icon1024.png 1024`, then scale down; the same render makes the macOS `.icns`).

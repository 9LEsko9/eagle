Copies of the generated brand assets (`cargo run -p eagle-brand -- build`, output in
`assets/brand/out/`, which is not committed): the window icon and the tray glyphs.
Rebuild and copy again when the sources in `assets/brand/src` change.

- `icon-256.png`: window icon (`png/icon-256.png`).
- `tray-macos.png`: black template glyph, 36 px (`tray/macos/trayTemplate@2x.png`).
- `tray-white.png`: white glyph, 24 px, for Windows and Linux (`tray/linux/tray-24.png`).

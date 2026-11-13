# Брендовые иконки Eagle

- `src/` — исходные SVG: `icon`, `icon-small`, `tray`, `tray-small`, `mark`, `android-fg/bg/mono`.
- Сборка: `cargo run -p eagle-brand -- build` (`--src`, `--out`; каталог `out` удаляется и создаётся заново).
- `out/` не коммитится: `desktop/` (ico, icns, png), `tray/`, `ios/`, `android/res/`, `web/`, `mark/`.
- `out/preview.png` (1600×1000) показывает все размеры для проверки глазами.
- Варианты `*-small` берутся автоматически для размеров до 32 px.
- Правило: золото (`#E9A23B`) только на клюве.

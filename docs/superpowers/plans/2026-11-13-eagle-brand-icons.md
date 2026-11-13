# Eagle brand icons — план

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Чистовые SVG-исходники знака eagle и Rust-утилита, которая собирает из них иконки всех платформ, трей и превью.

**Architecture:** Исходники SVG в `assets/brand/src/` — источник правды. Крейт `eagle-brand` (`tools/brand/`) рендерит их через resvg в PNG и пакует платформенные форматы в `assets/brand/out/`. Для размеров ≤ 32 px берутся отдельные упрощённые исходники. Цвет фона подменяется заменой строки, чтобы позже из того же кода делать иконку в доке в цвет обложки.

**Tech Stack:** Rust (edition 2021), resvg (вместе с реэкспортами `resvg::usvg` и `resvg::tiny_skia`), ico, icns, png, anyhow, clap (derive). Версии — последние совместимые на момент работы.

**Spec:** `docs/superpowers/specs/2026-11-13-eagle-logo-design.md` (копия документа проекта `claude/eagle-logo.md`). Перед первой задачей положи его в репозиторий по этому пути и закоммить вместе с этим планом (`docs/superpowers/plans/2026-11-13-eagle-brand-icons.md`).

## Global Constraints

- Цвета (точно, верхний регистр в SVG): графит `#1A1A1D`, диск `#F2F2F4`, золото `#E9A23B`.
- Золото — только клюв. Ни один другой элемент ни в одном исходнике не использует `#E9A23B`.
- В иконке цветом фона (`#1A1A1D`) красятся ровно три вещи: фон, голова (вырез из диска) и рассечение рта на клюве. Глаз и восковица — цвет диска.
- Размеры ≤ 32 px рендерятся из `*-small.svg` (константа `SMALL_MAX: u32 = 32`).
- Трей и монохромный слой Android — один цвет + альфа, голова и глаз сделаны настоящими вырезами (`fill-rule="evenodd"`), а не краской.
- iOS 1024: без прозрачности (alpha = 255 у всех пикселей).
- Android: передний план целиком внутри безопасной зоны 66 dp из 108 dp (радиус ≤ 30,55 % стороны от центра).
- Windows .ico приложения: 16, 20, 24, 32, 40, 48, 64, 256. .icns: 16, 32, 64, 128, 256, 512, 1024.
- Трей macOS: `trayTemplate.png` 18 px и `trayTemplate@2x.png` 36 px, чёрный + альфа. Windows: 16/20/24/32.
- Всё на Rust; утилита — dev-инструмент, в бинарники приложения не линкуется.
- Сборка детерминирована: повторный запуск даёт побайтно те же файлы, а `out/` перед сборкой очищается.

## Review Focus

- Детали пропадают на 16 px. На 16 px глаз и вырез головы в трее должны оставаться видимыми, а не сливаться в сплошной круг. Тест в Task 2: `tray_small_keeps_cutout_at_16`.
- Android обрезает знак маской. Пиксели переднего плана за пределами безопасной зоны срезаются круглой маской лаунчера. Тест в Task 2: `android_fg_inside_safe_zone`.
- iOS отклоняет иконку с альфой. Тест в Task 4: `ios_icon_is_opaque`.
- Цветные пиксели в template-изображении. macOS игнорирует цвет, и полупрозрачные цветные края дают серую кашу. Тест в Task 3: `macos_tray_is_template`.
- Сдвиг цвета при рендере (гамма или профиль), из-за которого фон перестаёт быть ровно `#1A1A1D`. Тест в Task 1: `render_keeps_exact_bg_color`.

### Task 1: Крейт, палитра, рендер, перекраска фона

**Files:**
- Create: `tools/brand/Cargo.toml` (пакет `eagle-brand`, lib + bin), `tools/brand/src/lib.rs`, `src/palette.rs`, `src/render.rs`
- Modify: корневой `Cargo.toml` — добавить `tools/brand` в `members`. Если workspace ещё нет, крейт живёт самостоятельно.
- Test: `tools/brand/tests/render.rs`

**Interfaces:**
- Produces:
  - `palette::{GRAPHITE, DISC, GOLD}: &'static str` (`"#1A1A1D"`, `"#F2F2F4"`, `"#E9A23B"`), `palette::rgb(hex: &str) -> [u8; 3]`
  - `render::render_svg(svg: &str, size: u32) -> anyhow::Result<tiny_skia::Pixmap>` — рендерит на квадрат size×size, вписывая viewBox
  - `render::recolor_background(svg: &str, hex: &str) -> String` — заменяет все вхождения GRAPHITE на hex
  - `render::to_template(p: &Pixmap) -> Pixmap` — каждый пиксель становится (0,0,0,a); `render::tint(p: &Pixmap, rgb: [u8;3]) -> Pixmap` — каждый пиксель становится (rgb, a)

- [ ] **Step 1: Write the failing tests** (на встроенном тестовом SVG: `<svg viewBox="0 0 10 10"><rect id="bg" width="10" height="10" fill="#1A1A1D"/><circle cx="5" cy="5" r="2" fill="#E9A23B"/></svg>`)

```rust
#[test] fn render_keeps_exact_bg_color() {
    let p = render_svg(TEST_SVG, 64).unwrap();
    assert_eq!((p.width(), p.height()), (64, 64));
    let c = p.pixel(1, 1).unwrap();
    assert_eq!([c.red(), c.green(), c.blue(), c.alpha()], [0x1A, 0x1A, 0x1D, 255]);
}
#[test] fn recolor_changes_bg_only() {
    let s = recolor_background(TEST_SVG, "#3E2430");
    let p = render_svg(&s, 64).unwrap();
    let c = p.pixel(1, 1).unwrap();
    assert_eq!([c.red(), c.green(), c.blue()], [0x3E, 0x24, 0x30]);
    let m = p.pixel(32, 32).unwrap();
    assert_eq!([m.red(), m.green(), m.blue()], [0xE9, 0xA2, 0x3B]);
}
#[test] fn template_is_black_with_alpha() {
    let p = to_template(&render_svg(TEST_SVG, 32).unwrap());
    assert!(p.pixels().iter().all(|c| c.red() == 0 && c.green() == 0 && c.blue() == 0));
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p eagle-brand --test render` → ошибки компиляции (функций нет).
- [ ] **Step 3: Implement** `palette.rs` и `render.rs` с сигнатурами выше. `render_svg` работает через `usvg::Tree::from_str` и `resvg::render` с `Transform::from_scale(size / tree.size().width(), …)`. В `to_template` и `tint` учитывай, что у tiny_skia premultiplied-альфа: собирай `PremultipliedColorU8` из нужного цвета, умноженного на a.
- [ ] **Step 4: Run, expect PASS** — `cargo test -p eagle-brand --test render`.
- [ ] **Step 5: Commit** — `git commit -m "feat(brand): add eagle-brand crate with svg render and recolor"`.

### Task 2: SVG-исходники

**Files:**
- Create в `assets/brand/src/`: `icon.svg`, `icon-small.svg`, `mark.svg`, `tray.svg`, `tray-small.svg`, `android-fg.svg`, `android-bg.svg`, `android-mono.svg`
- Create: `tools/brand/src/sources.rs`
- Test: `tools/brand/tests/sources.rs`

**Interfaces:**
- Consumes: `palette::*`, `render::render_svg`
- Produces:
  - `sources::Source` — enum { Icon, IconSmall, Mark, Tray, TraySmall, AndroidFg, AndroidBg, AndroidMono }
  - `Source::file_name(self) -> &'static str`
  - `sources::for_size(large: Source, size: u32) -> Source` — Icon→IconSmall, Tray→TraySmall при `size <= SMALL_MAX`, остальные не меняются
  - `sources::load(src_dir: &Path, s: Source) -> anyhow::Result<String>`
  - `pub const SMALL_MAX: u32 = 32`

**Геометрия.** Все файлы в `viewBox="0 0 100 100"`. Пути взяты из раздела «Эскиз-ориентир» спецификации, посадку можно задать атрибутом `transform`.

- `icon.svg`: `<rect id="bg" fill="#1A1A1D">` на весь холст. `circle#disc` (50,50, r 44, `#F2F2F4`). `circle#groove` (r 38, без заливки, штрих `#1A1A1D` 1.6). Группа головы с `transform="translate(50 50) scale(0.66) translate(-54 -56)"`: `path#head` `#1A1A1D`, `path#beak` `#E9A23B`, `path#eye` `#F2F2F4`, `path#cere` (штрих `#F2F2F4` 2.4), `path#mouth` (штрих `#1A1A1D` 2.2).
- `icon-small.svg`: те же фон и диск. Голова упрощённая (путь «Упрощённая голова для трея» из спецификации), клюв `M76 32 C84 33 91 39 91 48 C91 55 88 60 83 64 C84 57 81 53 76 53 L73 53 C72 46 72.5 39 76 32 Z`, глаз упрощённый. Канавки, восковицы и рта нет. Посадка `translate(50 50) scale(0.7) translate(-56 -58)`.
- `mark.svg`: только голова из `icon.svg` с клювом, глазом, восковицей и ртом, без диска и фона. Голова `#1A1A1D`, клюв золотой, глаз и восковица `#F2F2F4`.
- `tray.svg` и `tray-small.svg`: один `path` с `fill="#000000" fill-rule="evenodd"`, в котором круг r 48, упрощённая голова и глаз (посадка как у `icon-small`). Разница между файлами: в `tray-small` глаз увеличен на 25 % вокруг своего центра, чтобы пережить 16 px.
- `android-fg.svg`: диск r 30.5 + голова как в `icon.svg` при масштабе 0.66 × 30.5 / 44, прозрачный фон. `android-bg.svg`: сплошной `#1A1A1D`. `android-mono.svg`: геометрия `tray.svg`, уменьшенная до r 30.5.

После того как пути перенесены, вычисти их: выровняй точки, сгладь стыки кривых, сохрани силуэт и признаки орла из спецификации (крюк, восковица, надбровье-клин, плоская макушка, пряди на затылке).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn icon_has_required_ids() {
    let t = usvg::Tree::from_str(&load(SRC, Source::Icon).unwrap(), &Default::default()).unwrap();
    for id in ["bg", "disc", "groove", "head", "beak", "eye", "cere", "mouth"] {
        assert!(t.node_by_id(id).is_some(), "missing #{id}");
    }
}
#[test] fn gold_only_on_beak() {
    for s in ALL_SOURCES {
        let svg = load(SRC, s).unwrap();
        let n = svg.matches("#E9A23B").count();
        let expected = if matches!(s, Source::Icon | Source::IconSmall | Source::Mark) { 1 } else { 0 };
        assert_eq!(n, expected, "{s:?}");
    }
}
#[test] fn tray_small_keeps_cutout_at_16() {
    let p = render_svg(&load(SRC, Source::TraySmall).unwrap(), 16).unwrap();
    let inside_disc_transparent = (4..12).flat_map(|y| (4..12).map(move |x| (x, y)))
        .filter(|&(x, y)| p.pixel(x, y).unwrap().alpha() < 128).count();
    assert!(inside_disc_transparent >= 6, "head cutout lost at 16px");
}
#[test] fn android_fg_inside_safe_zone() {
    let p = render_svg(&load(SRC, Source::AndroidFg).unwrap(), 432).unwrap();
    let r_max = 432.0 * 0.3055;
    for y in 0..432 { for x in 0..432 {
        let (dx, dy) = (x as f32 + 0.5 - 216.0, y as f32 + 0.5 - 216.0);
        if (dx * dx + dy * dy).sqrt() > r_max + 1.0 {
            assert_eq!(p.pixel(x, y).unwrap().alpha(), 0, "pixel {x},{y} outside safe zone");
        }
    }}
}
#[test] fn tray_and_mono_are_single_colour() {
    for s in [Source::Tray, Source::TraySmall, Source::AndroidMono] {
        let svg = load(SRC, s).unwrap();
        assert!(!svg.contains("#1A1A1D") && !svg.contains("#F2F2F4") && !svg.contains("#E9A23B"), "{s:?}");
    }
}
#[test] fn for_size_picks_small() {
    assert_eq!(for_size(Source::Icon, 32), Source::IconSmall);
    assert_eq!(for_size(Source::Icon, 33), Source::Icon);
    assert_eq!(for_size(Source::Tray, 18), Source::TraySmall);
    assert_eq!(for_size(Source::Mark, 16), Source::Mark);
}
```

`SRC` — путь к `assets/brand/src`, считается от `env!("CARGO_MANIFEST_DIR")`. `ALL_SOURCES` — массив всех вариантов `Source`.

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p eagle-brand --test sources`.
- [ ] **Step 3: Implement** `sources.rs` и все восемь SVG по описанию выше.
- [ ] **Step 4: Run, expect PASS** — `cargo test -p eagle-brand --test sources`.
- [ ] **Step 5: Commit** — `git commit -m "feat(brand): add eagle mark, icon and tray svg sources"`.

### Task 3: Десктоп — .ico, .icns, трей

**Files:**
- Create: `tools/brand/src/pack.rs`, `tools/brand/src/build.rs` (функции десктопа)
- Test: `tools/brand/tests/desktop.rs` (сборка во временную папку через tempfile)

**Interfaces:**
- Consumes: `render::*`, `sources::*`
- Produces:
  - `pack::write_png(p: &Pixmap, path: &Path) -> Result<()>`
  - `pack::write_ico(images: &[Pixmap], path: &Path) -> Result<()>`
  - `pack::write_icns(images: &[Pixmap], path: &Path) -> Result<()>`
  - `build::build_desktop(src: &Path, out: &Path) -> Result<()>`, `build::build_tray(src: &Path, out: &Path) -> Result<()>`

**Выход:**
- `out/desktop/eagle.ico`, `out/desktop/eagle.icns`, `out/png/icon-{16,32,64,128,256,512,1024}.png`
- `out/tray/macos/trayTemplate.png` (18) и `trayTemplate@2x.png` (36), обе через `to_template`
- `out/tray/windows/tray-light.ico` (чёрный глиф) и `tray-dark.ico` (белый глиф, `tint([255,255,255])`), размеры 16/20/24/32
- `out/tray/linux/tray-symbolic.svg` (копия `tray.svg`) и `tray-{22,24}.png` (белые)

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn ico_has_all_sizes() {
    let dir = build_tmp(build_desktop);
    let icon_dir = ico::IconDir::read(File::open(dir.join("desktop/eagle.ico")).unwrap()).unwrap();
    let mut sizes: Vec<u32> = icon_dir.entries().iter().map(|e| e.width()).collect();
    sizes.sort();
    assert_eq!(sizes, vec![16, 20, 24, 32, 40, 48, 64, 256]);
}
#[test] fn icns_has_all_sizes() {
    let dir = build_tmp(build_desktop);
    let fam = icns::IconFamily::read(File::open(dir.join("desktop/eagle.icns")).unwrap()).unwrap();
    for t in [icns::IconType::RGBA32_16x16, icns::IconType::RGBA32_32x32, icns::IconType::RGBA32_64x64,
              icns::IconType::RGBA32_128x128, icns::IconType::RGBA32_256x256,
              icns::IconType::RGBA32_512x512, icns::IconType::RGBA32_512x512_2x] {
        assert!(fam.has_icon_with_type(t), "{t:?}");
    }
}
#[test] fn macos_tray_is_template() {
    let dir = build_tmp(build_tray);
    for (f, s) in [("trayTemplate.png", 18), ("trayTemplate@2x.png", 36)] {
        let img = read_png_rgba(&dir.join("tray/macos").join(f));
        assert_eq!((img.width, img.height), (s, s));
        assert!(img.pixels().all(|[r, g, b, _]| r == 0 && g == 0 && b == 0));
    }
}
#[test] fn small_ico_entries_use_small_source() {
    // 16 px из eagle.ico попиксельно совпадает с render_svg(IconSmall, 16), а не с render_svg(Icon, 16)
}
```

`build_tmp` и `read_png_rgba` — хелперы в `tests/common/mod.rs`, принимают функцию сборки и путь `assets/brand/src`. Тело `small_ico_entries_use_small_source`: декодируй запись 16×16 из .ico и сравни её байты RGBA с `render_svg(IconSmall, 16)` через `assert_eq!`.

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p eagle-brand --test desktop`.
- [ ] **Step 3: Implement** `pack.rs` и `build_desktop`/`build_tray`. Для каждого размера бери `for_size(...)`. В .icns клади PNG-типы.
- [ ] **Step 4: Run, expect PASS** — `cargo test -p eagle-brand --test desktop`.
- [ ] **Step 5: Commit** — `git commit -m "feat(brand): build desktop icons and tray assets"`.

### Task 4: Мобильные платформы и веб

**Files:**
- Modify: `tools/brand/src/build.rs` (добавить `build_android`, `build_ios`, `build_web`)
- Test: `tools/brand/tests/mobile.rs`

**Interfaces:**
- Consumes: `pack::*`, `render::*`, `sources::*`
- Produces: `build::build_android(src, out) -> Result<()>`, `build::build_ios(src, out) -> Result<()>`, `build::build_web(src, out) -> Result<()>` (все с `src: &Path, out: &Path`)

**Выход:**
- Android, `out/android/res/`:
  - `mipmap-{mdpi,hdpi,xhdpi,xxhdpi,xxxhdpi}/ic_launcher_foreground.png`, `ic_launcher_background.png` и `ic_launcher_monochrome.png` размерами 108/162/216/324/432
  - `ic_launcher.png` (легаси, из Icon/IconSmall) размерами 48/72/96/144/192
  - `mipmap-anydpi-v26/ic_launcher.xml` с `<adaptive-icon>`, в котором есть `<background>`, `<foreground>` и `<monochrome>`, ссылающиеся на `@mipmap/…`
- iOS: `out/ios/AppIcon.appiconset/AppIcon-1024.png` (из Icon, непрозрачный) и `Contents.json` с одной записью `{"idiom":"universal","platform":"ios","size":"1024x1024","filename":"AppIcon-1024.png"}` плюс `"info":{"author":"eagle-brand","version":1}`.
- Веб: `out/web/favicon.svg` (копия `mark.svg`), `favicon.ico` (16/32/48 из IconSmall/Icon), `apple-touch-icon.png` (180), `out/mark/mark-512.png`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn ios_icon_is_opaque() {
    let dir = build_tmp(build_ios);
    let img = read_png_rgba(&dir.join("ios/AppIcon.appiconset/AppIcon-1024.png"));
    assert_eq!((img.width, img.height), (1024, 1024));
    assert!(img.pixels().all(|[_, _, _, a]| a == 255));
}
#[test] fn android_layers_per_density() {
    let dir = build_tmp(build_android);
    for (d, s) in [("mdpi",108),("hdpi",162),("xhdpi",216),("xxhdpi",324),("xxxhdpi",432)] {
        for layer in ["foreground", "background", "monochrome"] {
            let img = read_png_rgba(&dir.join(format!("android/res/mipmap-{d}/ic_launcher_{layer}.png")));
            assert_eq!((img.width, img.height), (s, s), "{d} {layer}");
        }
    }
    let xml = std::fs::read_to_string(dir.join("android/res/mipmap-anydpi-v26/ic_launcher.xml")).unwrap();
    for tag in ["<background", "<foreground", "<monochrome"] { assert!(xml.contains(tag), "{tag}"); }
}
#[test] fn web_assets_exist() {
    let dir = build_tmp(build_web);
    for f in ["web/favicon.svg", "web/favicon.ico", "web/apple-touch-icon.png", "mark/mark-512.png"] {
        assert!(dir.join(f).exists(), "{f}");
    }
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p eagle-brand --test mobile`.
- [ ] **Step 3: Implement** `build_android`, `build_ios`, `build_web`.
- [ ] **Step 4: Run, expect PASS** — `cargo test -p eagle-brand --test mobile`.
- [ ] **Step 5: Commit** — `git commit -m "feat(brand): build android, ios and web icon assets"`.

### Task 5: CLI, превью, проверка глазами

**Files:**
- Create: `tools/brand/src/main.rs`, `tools/brand/src/preview.rs`, `assets/brand/README.md`
- Modify: `tools/brand/src/build.rs` (`build_all`), `.gitignore` (добавить `assets/brand/out/`)
- Test: `tools/brand/tests/cli.rs`

**Interfaces:**
- Consumes: все `build::*`
- Produces:
  - `build::build_all(src: &Path, out: &Path) -> Result<()>` — очищает `out`, затем вызывает все `build_*` и `preview::write_sheet`
  - `preview::write_sheet(src: &Path, out: &Path) -> Result<()>` → `out/preview.png`
  - CLI: `cargo run -p eagle-brand -- build [--src assets/brand/src] [--out assets/brand/out]`

Превью-лист (`preview.png`, 1600×1000, фон `#FAFAF7`) показывает ряды:
1. Иконка в размерах 256/128/64/32/24/16 в реальном размере.
2. Иконка с `recolor_background` в `#3E2430`, `#1F3A39`, `#22304A` при 128 px.
3. Android: передний план поверх фона, обрезанный кругом и «каплей».
4. Трей 18/36 на полосах `#E8E8E8` (template в чёрном) и `#2B2B2E` (белым), Windows 16/24 на `#1C1C1C`.
5. mark при 256 и 32 на светлом и тёмном фоне.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn build_is_deterministic() {
    let a = tempdir(); let b = tempdir();
    build_all(SRC, a.path()).unwrap(); build_all(SRC, b.path()).unwrap();
    assert_eq!(hash_tree(a.path()), hash_tree(b.path()));
}
#[test] fn build_cleans_stale_files() {
    let d = tempdir(); std::fs::write(d.path().join("stale.txt"), "x").unwrap();
    build_all(SRC, d.path()).unwrap();
    assert!(!d.path().join("stale.txt").exists());
    assert!(d.path().join("preview.png").exists());
}
```

`hash_tree` — хелпер в `tests/common`: отсортированный список пар «относительный путь + хеш содержимого».

- [ ] **Step 2: Run, expect FAIL** — `cargo test -p eagle-brand --test cli`.
- [ ] **Step 3: Implement** `build_all`, `preview::write_sheet`, `main.rs` (clap, подкоманда build). В `assets/brand/README.md` уложись в 10 строк: что где лежит, как собрать, правило «золото только на клюве».
- [ ] **Step 4: Run all tests** — `cargo test -p eagle-brand` → все PASS. Затем `cargo clippy -p eagle-brand -- -D warnings` → без предупреждений.
- [ ] **Step 5: Visual check** — `cargo run -p eagle-brand -- build`, открой `assets/brand/out/preview.png` и проверь: орёл читается на 16 px (виден клюв), золото есть только на клюве, Android-маски ничего не режут, трей не превращается в кляксу. Приложи `preview.png` к отчёту человеку.
- [ ] **Step 6: Commit** — `git commit -m "feat(brand): add eagle-brand cli, preview sheet and docs"`.

## Вне плана (позже, отдельными задачами из спецификации)

- Живая иконка в доке в цвет обложки (platform, использует `render::recolor_background`).
- Состояния иконки в трее.
- Формат Icon Composer (`.icon`) для macOS, после проверки актуальных требований Apple.
- Связка «знак + eagle» для сайта и README.

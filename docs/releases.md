# Desktop releases / Релизы для компьютера

## English

The `eg-release.yml` workflow builds portable desktop and CLI archives for Windows x86-64, Linux x86-64, and macOS Apple Silicon and Intel. A version tag publishes the archives to the corresponding GitHub release; a manual workflow run only uploads workflow artifacts. Each archive includes the GPL license and license exceptions.

Extract the archive and run `eagle-desktop` (Windows: `eagle-desktop.exe`) or `eagle-cli`. On macOS, the archive contains only `Eagle.app`. Open it or move it to Applications; the CLI is inside at `Eagle.app/Contents/MacOS/eagle-cli`, and licenses are in `Contents/Resources`. Windows binaries are unsigned. macOS apps are ad-hoc signed, without Apple notarization, so the operating system may require an explicit security approval. Linux needs compatible system libraries for audio, D-Bus, fonts and the X11 or Wayland display environment.

For local packaging, install Python 3.11 or newer, build the binaries for the desired Rust target, and run:

```sh
cargo run --locked -p eagle-brand -- build # required for macOS icon
python3 scripts/release/package.py --target x86_64-unknown-linux-gnu --version 1.2.3 --bin-dir target/x86_64-unknown-linux-gnu/release --output dist
python3 -m unittest discover -s scripts/release
```

To create a standalone `.app` directory instead of a ZIP after building macOS binaries:

```sh
python3 scripts/release/package.py --target aarch64-apple-darwin --version 1.2.3 --bin-dir target/aarch64-apple-darwin/release --output dist --app-only
codesign --force --deep --sign - dist/Eagle.app
codesign --verify --deep --strict dist/Eagle.app
```

Use `x86_64-apple-darwin` for Intel builds. The output directory must not already contain `Eagle.app`; the script refuses to overwrite an existing bundle.

The script prints the archive path. Its macOS archive is unsigned; the release workflow signs the app before publishing. Tests inspect archive contents, executable permissions and macOS metadata; they do not replace launching each build on its target OS.

## Русский

Workflow `eg-release.yml` собирает переносимые архивы desktop и CLI для Windows x86-64, Linux x86-64 и macOS на Apple Silicon и Intel. Тег версии публикует архивы в соответствующем GitHub Release; ручной запуск сохраняет их только как артефакты workflow. В каждом архиве есть лицензия GPL и исключения для лицензий.

Распакуйте архив и запустите `eagle-desktop` (в Windows — `eagle-desktop.exe`) или `eagle-cli`. В macOS архив содержит только `Eagle.app`: откройте его или перенесите в «Программы». CLI находится внутри в `Eagle.app/Contents/MacOS/eagle-cli`, лицензии — в `Contents/Resources`. Файлы Windows не подписаны. Приложение macOS имеет ad-hoc подпись без нотариального заверения Apple, поэтому система может потребовать разрешить его запуск. Для Linux нужны совместимые системные библиотеки звука, D-Bus, шрифтов и графического окружения X11 или Wayland.

Для получения отдельного `.app` вместо ZIP используйте `--app-only` и команды подписи из английского раздела. Для Intel укажите `x86_64-apple-darwin`. Упаковщик не перезаписывает существующий `Eagle.app` в выходном каталоге.

Для локальной упаковки установите Python 3.11 или новее, соберите программы для нужной цели Rust и выполните команды из английского раздела. Скрипт выводит путь к архиву. Локальный архив macOS не подписан; release workflow подписывает приложение перед публикацией. Тесты проверяют содержимое архивов, права запуска и метаданные macOS, но не заменяют запуск на целевой системе.

## Publishing / Публикация

**English.** Update `[workspace.package].version` in `Cargo.toml`, run `cargo check --workspace` to update `Cargo.lock`, commit both files and merge the commit to `main`. Create and push a matching annotated tag on that commit, for example:

```sh
git tag -a v1.2.3 -m 'Release v1.2.3'
git push origin v1.2.3
```

The tag must match the workspace version and point to a commit reachable from `main`. The workflow builds all targets, publishes a release with `SHA256SUMS`, and marks prerelease versions accordingly. Verify downloaded files with `sha256sum -c SHA256SUMS` in the directory containing the archives. If publication fails after creating a draft, inspect the draft and its assets, then delete the incomplete draft before rerunning the failed workflow: publication refuses to overwrite any existing release for the tag.

Linux binaries are built on Ubuntu 22.04 (glibc 2.35); use a compatible distribution with glibc 2.35 or newer. On Ubuntu 22.04, runtime packages include `libasound2`, `libdbus-1-3`, `libfontconfig1`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libwayland-client0`, `libx11-xcb1`, and `libxcursor1`, plus an X11 or Wayland session. Local builds need the development packages listed in [development.md](development.md).

**Русский.** Измените `[workspace.package].version` в `Cargo.toml`, выполните `cargo check --workspace` для обновления `Cargo.lock`, закоммитьте оба файла и включите коммит в `main`. Создайте и отправьте аннотированный тег на этом коммите, как показано выше. Тег должен совпадать с версией workspace, а коммит — быть достижимым из `main`. Workflow собирает все платформы, публикует релиз с `SHA256SUMS` и отмечает предварительные версии. Для проверки скачанных файлов выполните `sha256sum -c SHA256SUMS` в каталоге с архивами. Если публикация прервалась после создания черновика, проверьте черновик и файлы, затем удалите незавершённый черновик перед повторным запуском: workflow отказывается перезаписывать существующий релиз этого тега.

Linux-сборка создаётся на Ubuntu 22.04 (glibc 2.35); нужна совместимая система с glibc 2.35 или новее. В Ubuntu 22.04 для запуска нужны пакеты `libasound2`, `libdbus-1-3`, `libfontconfig1`, `libxkbcommon0`, `libxkbcommon-x11-0`, `libwayland-client0`, `libx11-xcb1`, `libxcursor1` и сеанс X11 или Wayland. Для локальной сборки нужны пакеты разработки из [development.md](development.md).

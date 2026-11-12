# Eagle

Музыкальный плеер на Rust. Pre-alpha: CLI, локальные файлы, HTTP и Яндекс Музыка.
Десктопный интерфейс на Slint — в разработке. VK пока недоступен.

```sh
cargo run -p eagle-desktop
cargo run -p eagle-cli -- play song.flac
cargo run -p eagle-cli -- yandex login
cargo run -p eagle-cli -- play yandex:123
```

Rust 1.92+. [Зависимости Linux](docs/development.md).

- [Десктоп](docs/desktop.md) · [CLI и кэш](docs/cli.md)
- [Движок, проверки и замеры](docs/engine.md)
- [Разработка](docs/development.md) · [Логирование](docs/logging.md)

[GPL-3.0-or-later](LICENSE) с [разрешением для магазинов приложений](EXCEPTIONS.md).

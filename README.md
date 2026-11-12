# Eagle

Музыкальный плеер на Rust. Pre-alpha: CLI, локальные файлы, HTTP и Яндекс Музыка.
Графический интерфейс и провайдер VK пока не реализованы.

```sh
cargo run -p eagle-cli -- play song.flac
cargo run -p eagle-cli -- yandex login
cargo run -p eagle-cli -- play yandex:123
```

Rust 1.89+. На Linux: `libasound2-dev`, `libdbus-1-dev`, `pkg-config`.

- [CLI и кэш](docs/cli.md)
- [Движок, проверки и замеры](docs/engine.md)
- [Разработка](docs/development.md) · [Логирование](docs/logging.md)

[GPL-3.0-or-later](LICENSE) с [разрешением для магазинов приложений](EXCEPTIONS.md).

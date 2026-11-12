# Разработка

Rust edition 2024, MSRV 1.92 (для Slint 1.17).
На Linux:

```sh
sudo apt install libasound2-dev libdbus-1-dev libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcursor-dev pkg-config
```

Общие зависимости: thiserror и log. В коде Eagle tracing запрещён; deny.toml допускает его только внутри перечисленных зависимостей Slint/AccessKit.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check licenses bans sources
cargo audit
cargo build --profile profiling
```

## Аккаунты, секреты и логи

Использовать отдельные тестовые аккаунты Яндекса и VK. Основные аккаунты не
использовать для проверки неофициального клиента. Аккаунты создаёт владелец;
автоматического создания и подключения аккаунтов в каркасе нет.

Ключи и client_id передавать через окружение сборки или config.local.toml
(исключён из Git). Загрузчик конфигурации появится вместе с провайдерами.
Никогда не логировать токены, Authorization, query-параметры подписанных URL
и полные URL треков. До появления сетевого кода логирование этих данных запрещено.

## Лицензия

Eagle распространяется под GPLv3 или более поздней версией
([LICENSE](../LICENSE), SPDX: GPL-3.0-or-later), с дополнительным разрешением
для магазинов приложений в [EXCEPTIONS.md](../EXCEPTIONS.md).
Вклад принимается по [CONTRIBUTING.md](../CONTRIBUTING.md).
Slint 1.17.0 подключён к desktop. Разрешение Eagle не меняет лицензии стороннего кода.
Для iOS проверяйте `cargo deny --config deny-ios.toml check licenses bans sources`:
GPL разрешена только для перечисленных собственных пакетов с разрешением
из EXCEPTIONS.md. Конфиг проверяет выражения лицензий, но не предоставляет прав.

## Публикация

Перед каждым первым push: `gitleaks detect --source . --log-opts="--all" --redact`.
Также проверьте текущие файлы: `gitleaks dir . --redact`.
CI запускается в GitHub Actions; локальные мобильные проверки требуют rustup,
целевых std, NDK/cargo-ndk для Android и полного Xcode для iOS.

Будущие рабочие потоки называйте `eg-*`, максимум 15 символов. Для ручной
сверки Linux можно использовать bpftrace (root) по sched:sched_wakeup;
непосредственные wakeup-события и voluntary switches не всегда взаимно однозначны.

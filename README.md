# Eagle

Pre-alpha.

Каркас Rust workspace: core, providers/yandex, providers/vk, net, store,
engine, palette, platform, app, ui; бинарники eagle-cli и eagle-desktop.
UI и провайдеры пока не реализованы.

## Разработка

Rust edition 2024, MSRV 1.85. Общие зависимости: thiserror и log; tracing запрещён.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check licenses bans sources
cargo audit
cargo build --profile profiling
```

## Замеры

Нужен Python 3.9+. `cargo xtask bench` собирает release eagle-cli и измеряет
сценарий idle за 60 секунд. `cargo xtask bench --seconds 2` — smoke-проверка.
CSV сохраняется в bench/results/. CPU — доля одного ядра по разнице процессорного
времени; память — конечный снимок, не пик. Linux: PSS; macOS: phys_footprint;
Windows: private working set. Linux: разница voluntary_ctxt_switches по всем
живым потокам, суммарно и в отдельном threads.csv. Это прокси пробуждений,
а не точный счётчик sched_wakeup; завершившиеся между снимками потоки
не наблюдаются. CSV отмечает исчезнувшие потоки и неполное окно новых потоков. CI сохраняет CSV как артефакты по каждой ОС.

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
([LICENSE](LICENSE), SPDX: GPL-3.0-or-later), с дополнительным разрешением
для магазинов приложений в [EXCEPTIONS.md](EXCEPTIONS.md).
Вклад принимается по [CONTRIBUTING.md](CONTRIBUTING.md).
Slint пока не подключён. Разрешение Eagle не меняет лицензии стороннего кода.
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

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
сценарий idle с включённым логгером за 60 секунд.
`cargo xtask bench --scenario info` измеряет одну info-запись в секунду,
включая структурированные поля, маскировку, файл и stderr. `cargo xtask bench --seconds 2` — smoke-проверка.
CSV сохраняется в bench/results/ и содержит также размер release-бинарника. CPU — доля одного ядра по разнице процессорного
времени; память — конечный снимок, не пик. Linux: PSS; macOS: phys_footprint;
Windows: private working set. Linux: разница voluntary_ctxt_switches по всем
живым потокам, суммарно и в отдельном threads.csv. Это прокси пробуждений,
а не точный счётчик sched_wakeup; завершившиеся между снимками потоки
не наблюдаются. CSV отмечает исчезнувшие потоки и неполное окно новых потоков. CI сохраняет оба сценария как CSV-артефакты по каждой ОС. На Linux idle
дополнительно проверяет нулевую разницу voluntary switches у потока eg-log.

## Логирование с этапа 0

Фасад `log` с `kv`, собственный бэкенд в `eagle-platform::logging`, без tracing.
В релизе статический максимум debug (`release_max_level_debug`); trace доступен
только в debug-сборке. По умолчанию info. Например,
`EAGLE_LOG=info,eagle_net=debug` включает debug для крейта и его модулей.
`Logger::set_filter` меняет те же правила из будущих настроек; `off` отключает вывод.

CLI и desktop инициализируют логгер при запуске. Файлы `eagle.log` и
`eagle.previous.log` ограничены 1 000 000 байт каждый. Каталог: Linux —
`$XDG_DATA_HOME/eagle/logs` или `~/.local/share/eagle/logs`, Windows —
`%LOCALAPPDATA%/eagle/logs`, macOS — `~/Library/Application Support/eagle/logs`.
`EAGLE_DATA_DIR` переопределяет каталог данных (логи в его подпапке logs).
Мобильная оболочка передаёт свой приватный каталог в `Logger::init`.
Десктоп дублирует записи в stderr, Android — через android_logger в logcat,
iOS — в os_log. `Logger::report` синхронно сбрасывает буфер и возвращает оба
файла для будущей кнопки «Скопировать отчёт». Одновременный запуск нескольких
экземпляров с одним каталогом логов пока не поддерживается.

Общий буфер под mutex — 64 КиБ, запись — максимум 4096 байт плюс перевод строки.
Переполнение отбрасывает запись и добавляет «пропущено N записей» при сбросе.
Поток eg-log ждёт Condvar без таймера при пустом буфере. Первая запись
взводит секундный срок; заполнение буфера или warn/error требуют раннего сброса.
Warn/error ждут записи и sync_data; ошибки файлов доступны через `flush`.
При обычном завершении guard сбрасывает буфер и останавливает поток.

Хук Rust-паники сам синхронно пишет буфер, маскированное сообщение и принудительно
снятый бэктрейс (до 4096 байт), затем sync_data. Он не вызывает log и не ждёт
поток-писатель. Стандартный хук заменён, чтобы stderr не раскрыл секреты.
Крах в C/Skia, abort и убийство процесса системой не вызывают хук: ещё не
сброшенные info могут потеряться. Сбой диска также не позволяет гарантировать
сохранность warn/error. В stripped-релизе бэктрейс может содержать только адреса;
для символизации используйте profiling-сборку.

`eagle_core::Token` и `SignedUrl` маскируют Display/Debug; URL скрывает query,
fragment и userinfo. Для сетевого запроса нужен явный `expose()`.
Бэкенд без regex дополнительно маскирует token/sign/signature и другие
известные параметры, Authorization и Cookie. Это страховка: произвольный
секрет без метки распознать нельзя. На info разрешены provider и track_id,
запрещены названия треков и поисковые запросы. Логи должны подходить для
публичных issues. Оба уровня маскировки проверяются тестами.

Сравнение бэкендов воспроизводится командой
`python3 bench/compare_loggers.py --seconds 60`: изолированные release-пробы
пустого процесса, своего бэкенда и flexi_logger; зависимость flexi_logger
не добавляется в workspace. Решение и замеры — в [ADR](docs/adr/0001-logging.md).

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

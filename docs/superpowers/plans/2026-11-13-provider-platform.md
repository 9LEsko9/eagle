# Платформа провайдеров — план реализации

> **Для исполнителя:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development (рекомендуется) или superpowers:executing-plans. Шаги — чекбоксы. OpenCode для этого проекта не использовать (AGENTS.md проекта).
> После выхода из plan mode скопировать файл в `docs/superpowers/plans/2026-11-13-provider-platform.md`.

**Goal:** вынести границу провайдера в `eagle-provider-sdk`, заменить единственный `Arc<dyn Provider>` реестром с приоритетом, добавить склейку треков, подмену источника, локальные лайки с зеркалированием и журнал профиля.

**Architecture:** `eagle-core` реэкспортирует SDK (`pub use eagle_provider_sdk::*`), так что workspace переписывается постепенно. Группы треков и профиль — слой в `core`/`store` поверх неизменного `TrackId`. Все изменения `store` — миграции `user_version` 9+ (сейчас 8, `crates/store/src/lib.rs:59-125`). Каждая задача оставляет `cargo fmt --check && cargo test` зелёными и коммитится отдельно (`[DEV]`/`[FEATURE]`/`[FIX]`).

**Tech Stack:** Rust 2024, MSRV 1.92, rusqlite, serde, `thiserror`, `log` (kv). Новая зависимость допустима одна: `unicode-normalization` (NFKC, в `core`).

**Spec:** `docs/superpowers/specs/2026-11-13-provider-platform-design.md` (вставленный дизайн; в репозитории сейчас `docs/superpowers/2026-11-13-provider-platform-design.md`, untracked).

## Состояние кода (по CodeGraph)

- `Provider` + модели + `Token`/`SignedUrl` лежат в `crates/core/src/{models,secrets}.rs`; `core` зависит только от `thiserror/log/serde`.
- `eagle-net::Client` (`crates/net/src/lib.rs`) зависит от `core` (`Token`, `SignedUrl`) → **SDK не может зависеть от `net`** (цикл). Решение: `Http`-трейт в SDK, `impl Http for Client` — в `net`.
- `eagle-yandex` сам держит токен в keyring (`lib.rs:22,172,432`) и `rpassword` — противоречит §2 («провайдер не хранит токен»): keyring уходит в `platform`/`app`, `rpassword` — в `cli`.
- Жёсткая привязка: `session.rs` — `Worker.provider` (строки 554, 643, 662…), `ACCOUNT_UID`/`LIKED_ALBUMS`/`LIKED_ARTISTS` (24-26), `switch_account` (3104, `DELETE … 'yandex:%'`); `downloads.rs:375` (`provider.name()`); `store::replace_likes` (`lib.rs:265`); `playback.rs`, `music.rs`; `bridge.rs`/`lib.rs` в `ui` (в основном тесты); `fixture.rs`, `benchmark.rs`.
- `session.rs` — 5643 строки; выносить только провайдерскую/аккаунтную логику.
- Существующие тесты, которые нельзя сломать: `crates/app/tests/desktop_offline.rs`, тесты `session.rs`, миграционные тесты `store`.

## Решения плана (где спецификация оставляет выбор или расходится с кодом)

1. **`TrackId` допускает провайдер только `[a-z]+`** (`models.rs:33`). `fake-a`/`fake-b` из §9 не распарсятся → фейки получают id `fakea`/`fakeb`; формат `TrackId` не менять.
2. **`Http` в SDK:** свой трейт `Http { fn send(&self, priority: Priority, req: HttpRequest, refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>) -> Result<HttpResponse, HttpError> }` с собственными `Priority`, `HttpRequest`, `HttpResponse`, `HttpError { Status(u16), TooLarge, … }`; точный набор полей — по тому, что реально использует Yandex (`Request`, `Priority::{Metadata,Playback}`, `Status(410)` при повторе). `eagle-net` реализует трейт для `Client`, не меняя пул воркеров (2 потока).
3. **Capabilities**: `#[non_exhaustive]`, `Default`; эффективные = `info().capabilities ∧ account.capabilities`. `Account.capabilities` — `#[serde(default)]`-совместимо (Account уже `Deserialize`).
4. **Токены**: хранение в `platform` (ключ `eagle`/`<provider_id>`); провайдер получает токен через `login(Token)` и `restore(Token)` (новый метод `fn restore(&self, token: Token) -> Result<Account, ProviderError>`, по умолчанию = `login` без записи) — чтобы `Restore` не требовал ввода.
5. **Лайки**: `liked(grp, at)`; снятие лайка при синхронизации — по правилу §6 с таблицей `provider_liked_seen(provider, track, synced_at)` (хранит «в прошлую синхронизацию лайкнут у провайдера»).
6. **Сжатие журнала** (§8): снимок состояния в `profile_snapshot` + надгробия старше 90 дней + операции, не подтверждённые известными `profile_devices.seen_seq`; запуск при открытии `store`, не чаще раза в сутки; старт читает только таблицы состояния.

## Global Constraints

- Rust 2024, MSRV 1.92, Google Rust Style, `rustfmt`; ошибки через `thiserror`; логи `log` с kv; **без `tracing`**.
- Без `unwrap`/`expect` в рабочем коде; без новых зависимостей, кроме `unicode-normalization`.
- Сеть блокирующая (`ureq`), без tokio; **не плодить потоки на провайдера/запрос** (поиск — последовательно в каталожном потоке).
- На info — только `provider`/`track_id`; секреты только `Token`/`SignedUrl`, `expose()` не логировать.
- Офлайн-права (`access`, `offline_owners`, `downloads.account`) привязаны к аккаунту провайдера, не к профилю; логику не ослаблять.
- Миграции через `PRAGMA user_version`, без потери данных (проверять на копии схемы v8).
- Анти-цели: провайдер не зависит от `app/store/ui/engine`, не знает SQLite; в `app/store/ui` нет `"yandex"`/`'yandex:` вне тестов и `builtin()`; нет регресса памяти/пробуждений (`cargo xtask bench`, baseline до начала — Task 0).
- Коммиты: `[FEATURE|FIX|DEV|INIT] описание`; завершать trailer-ом `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`; перед каждым коммитом `cargo fmt --check` и `cargo test`.
- Склейка консервативна: лучше не склеить.

## Review Focus (не покрыто задачами напрямую → тест добавлен в указанную задачу)

1. Пользователь с одним Яндексом после обновления: лайки, плейлисты, сохранённое, история, позиция воспроизведения целы (Task 6, 12).
2. Тезки-исполнители и треки `Live`/`Remix`/`Acoustic` не склеиваются; два трека одного провайдера не склеиваются никогда (Task 7).
3. Выход из провайдера: лайки на месте, сохранённое другого провайдера не тронуто, нет `DELETE … LIKE 'yandex:%'` (Task 10).
4. Гонка «лайкнул локально / снят на сервере» при синхронизации; `Uncertain` в `like_outbox` не повторяется (Task 11).
5. Пустой реестр / ни один провайдер не вошёл: eagle запускается, история пишется в профиль, UI не падает (Task 6, 9).

---

### Task 0: Baseline

**Files:** `docs/superpowers/plans/…-baseline.md` (результаты)

- [ ] **Step 1:** `cargo fmt --check && cargo test` на чистой ветке — записать, что зелёное.
- [ ] **Step 2:** `cargo xtask bench` → сохранить память и пробуждения в фоне как baseline (та же машина для всех сравнений).
- [ ] **Step 3:** Закоммитить спецификацию (`docs/superpowers/specs/…`) — `[DEV] Add provider platform design`.

### Task 1: Крейт `eagle-provider-sdk` (перенос без изменения поведения)

**Files:** Create `crates/provider-sdk/{Cargo.toml,src/lib.rs,src/models.rs,src/secrets.rs}`; Modify `Cargo.toml` (members), `crates/core/{Cargo.toml,src/lib.rs}`; удалить `core/src/{models,secrets}.rs`.

**Interfaces — Produces:** `eagle_provider_sdk::{TrackId, Track, Artist, Album, ArtistPage, Playlist, PlaylistSnapshot, PlaylistEdit, PlaylistTrack, Page, Stream, Quality, Encryption, Availability, Account, AccessCheck, Radio*, ProviderError, PlaylistEditError, RadioError, Token, SignedUrl, Provider}`; `eagle_core` делает `pub use eagle_provider_sdk::*`.

- [ ] **Step 1:** Тест-«гарант»: в `core` `tests/reexport.rs` — `let _: eagle_core::TrackId = "yandex:1".parse().unwrap();` и проверка маскировки `Token`/`SignedUrl` (`format!("{token}") == "[REDACTED]"`). Запустить — проходит до переноса (характеризационный).
- [ ] **Step 2:** Создать крейт, переместить код и существующие тесты `models.rs`; `core` зависит от SDK и реэкспортирует.
- [ ] **Step 3:** `cargo test` по всему workspace зелёный, без правок в других крейтах.
- [ ] **Step 4:** Коммит `[DEV] Extract eagle-provider-sdk from core`.

### Task 2: `ProviderInfo`, `Capabilities`, `AuthMethod`, новые методы трейта

**Files:** Modify `crates/provider-sdk/src/{provider.rs,models.rs}`; все реализации `Provider` (yandex, vk-заготовка, `fixture.rs`, `benchmark.rs`, тестовые в `session.rs`/`downloads.rs`/`music.rs`).

**Interfaces — Produces:**
```rust
pub struct ProviderInfo { pub id: &'static str, pub name: &'static str, pub auth: AuthMethod, pub capabilities: Capabilities }
pub enum AuthMethod { None, Token { hint: &'static str, help_url: Option<&'static str> }, Browser { start_url: String, done: RedirectMatch } }
#[non_exhaustive] #[derive(Clone, Copy, Default, PartialEq, Eq, Debug)] pub struct Capabilities { /* 17 bool из §2 */ }
impl Capabilities { pub fn intersect(&self, other: &Capabilities) -> Capabilities }
// Provider: fn info(&self) -> &ProviderInfo   (вместо name)
//           fn logout(&self);  fn restore(&self, Token) -> Result<Account, ProviderError>;
//           fn artist_tracks(&self, &TrackId, Option<&str>) -> Result<Page<Track>, ProviderError> { Err(Unavailable) }
// Track { #[serde(default)] pub isrc: Option<String> }   Account { #[serde(default)] pub capabilities: Capabilities }
```
- [ ] **Step 1:** Тесты: `Capabilities::default()` — всё `false`; `intersect` покомпонентно; `Track` без поля `isrc` в JSON десериализуется (старые кэши); `Account` без `capabilities` — тоже.
- [ ] **Step 2:** Реализовать типы; заменить `name()` на `info()` во всех реализациях и в `downloads.rs:375` (`provider.info().id`).
- [ ] **Step 3:** `cargo test` зелёный; коммит `[DEV] Add provider info and capabilities`.

### Task 3: `Http` в SDK и перенос Яндекса на SDK

**Files:** Create `crates/provider-sdk/src/http.rs`; Modify `crates/net/src/lib.rs` (`impl Http for Client`), `crates/providers/yandex/{Cargo.toml,src/*.rs}`; Create `crates/platform/src/tokens.rs` (keyring) ; Modify `crates/cli` (`prompt_token` через `AuthMethod::Token`).

- [ ] **Step 1:** Тест в `net`: `Client` через `dyn Http` ходит на локальный сервер и возвращает `Status(401)` как `HttpError::Status(401)` (по образцу существующих `net`-тестов).
- [ ] **Step 2:** Тест в `yandex`: существующие контрактные тесты (`contracts.rs`) проходят без обращения к keyring (токен приходит через `login`/`restore`).
- [ ] **Step 3:** Реализовать `Http`; `Yandex { net: Arc<dyn Http>, … }`; убрать `eagle-core`/`eagle-net`/`keyring`/`rpassword` из зависимостей yandex, оставить только `eagle-provider-sdk`, `serde_json`, `md5`, `roxmltree`. `Yandex::info()`: `id "yandex"`, `name "Яндекс Музыка"`, `AuthMethod::Token`, честные возможности. Вызовы `eagle_yandex::` в `app`/`cli` пока остаются.
- [ ] **Step 4:** Реализовать `artist_tracks`; заполнить `isrc`, если API его отдаёт (проверить `contracts.rs` и записанные ответы; иначе `capabilities.isrc = false`).
- [ ] **Step 5:** Проверка зависимостей: `cargo tree -p eagle-yandex` не содержит `eagle-store/-app/-ui/-engine/-net`. Коммит `[DEV] Move Yandex onto the provider SDK`.

### Task 4: testkit и `FakeProvider`

**Files:** Create `crates/provider-sdk/src/testkit.rs` (фича `testkit`), `crates/provider-sdk/README.md`, `crates/providers/_template/` (компилирующийся провайдер с тестом; добавить в workspace members).

**Interfaces — Produces:** `FakeProvider::new(id: &'static str) -> FakeProvider` + builder (`with_tracks`, `with_capabilities`, `fail_with(ProviderError)`, `signed_out()`), `conformance::run(&dyn Provider, &Fixtures) -> Vec<Violation>`.

- [ ] **Step 1:** Тесты testkit: корректный `FakeProvider` проходит `conformance::run`; провайдер с битым ID, заявленной, но `Unavailable` возможностью, бесконечной пагинацией или `expires_at` в прошлом — даёт конкретные `Violation`.
- [ ] **Step 2:** Реализовать; прогнать `conformance::run` на Yandex по записанным ответам (в `yandex` dev-dependency на testkit).
- [ ] **Step 3:** README (правила: без своих потоков, без логов секретов, блокирующий API, непрозрачные курсоры, регистрация в `builtin()`); шаблон компилируется (`cargo test -p eagle-provider-template`).
- [ ] **Step 4:** Коммит `[FEATURE] Add provider testkit and template`.

### Task 5: Реестр `Providers` и `builtin()`

**Files:** Create `crates/app/src/providers.rs`; Modify `crates/app/src/{lib.rs,session.rs,downloads.rs,playback.rs,music.rs,fixture.rs,benchmark.rs}`.

**Interfaces — Produces:**
```rust
pub struct Providers { /* порядок = приоритет */ }
impl Providers {
    pub fn new(all: Vec<Arc<dyn Provider>>, order: &[String]) -> Self; // неизвестные id в order игнорируются, новые — в конец
    pub fn get(&self, id: &str) -> Option<&Arc<dyn Provider>>;
    pub fn signed_in(&self) -> impl Iterator<Item = &Arc<dyn Provider>>;
    pub fn with_capability(&self, f: impl Fn(&Capabilities) -> bool) -> impl Iterator<Item = &Arc<dyn Provider>>; // эффективные, по приоритету, только вошедшие
    pub fn for_track(&self, id: &TrackId) -> Option<&Arc<dyn Provider>>;
}
pub fn builtin(net: Arc<Client>) -> Vec<Arc<dyn Provider>>; // единственное место с eagle_yandex::Yandex::new
// Session::start_with_providers(root, Vec<Arc<dyn Provider>>, emit)  (заменяет start_with_provider)
```
- [ ] **Step 1:** Юнит-тесты: порядок по `provider_order`; новый провайдер дописывается в конец; неизвестный id игнорируется; `with_capability` пропускает невошедшие и провайдеров, чьи эффективные возможности (`info ∧ account`) не включают флаг.
- [ ] **Step 2:** Реализовать `Providers`; `Worker.provider` → `Worker.providers: Arc<Providers>`; `Catalog`, `Downloads`, `playback`, `music` получают реестр, выбирают провайдера по `TrackId::provider()`; существующие тесты `session.rs`/`desktop_offline.rs` — правка конструкторов.
- [ ] **Step 3:** Статус возможностей аккаунта кэшируется при `login/restore` (не опрашивать провайдера на каждый вызов).
- [ ] **Step 4:** `cargo test` + `cargo xtask bench` не хуже baseline. Коммит `[DEV] Replace single provider with a registry`.

### Task 6: Профиль и аккаунты (миграция v9)

**Files:** Modify `crates/store/src/{lib.rs,history.rs,playback.rs}`, `crates/app/src/session.rs`.

- [ ] **Step 1:** Миграционный тест на копии схемы v8: `settings{account_uid:yandex=42}`, `history`/`playback` с `account='42'`, `downloads/access/offline_owners/play_outbox/radio_play_intents` с `42` → после v9: `settings.profile_id`, `settings.device_id`, `account:yandex=42`; `history`/`playback` на `profile_id`; остальные таблицы нетронуты; `account_uid:yandex` удалён.
- [ ] **Step 2:** Тест: без аккаунта `record_play` пишет историю в профиль (сейчас молча ничего); сохранённый трек не играет без прав (`desktop_offline.rs` проходит).
- [ ] **Step 3:** Реализовать миграцию (одна транзакция, `profile_id`/`device_id` — случайные, генерация в `store`); ключи настроек строятся от `provider_id`: `account:<id>`, `liked_albums:<id>`, `liked_artists:<id>` (константы `ACCOUNT_UID`/`LIKED_*` удалить); `Worker.account` → `Worker.profile` + `accounts: HashMap<provider_id, uid>`.
- [ ] **Step 4:** Коммит `[FEATURE] Separate profile from provider accounts`.

### Task 7: Нормализация и склейка (чистая логика в `core`)

**Files:** Create `crates/core/src/matching.rs`; Modify `crates/core/Cargo.toml` (`unicode-normalization`).

**Interfaces — Produces:**
```rust
pub fn normalize(title_or_name: &str) -> String;                 // NFKC, lower, ё→е, без пунктуации, хвосты feat./Remastered/Radio Edit — только для ключа
pub fn match_key(track: &Track) -> Option<String>;               // normalize(title) + '\u{1f}' + normalize(первый артист); None без артистов
pub fn can_merge(a: &Track, b: &Track) -> Option<MergeMethod>;  // Isrc | Meta; None если один провайдер или |Δduration| > 2000 мс
```
- [ ] **Step 1:** Тесты (все как `#[test]` в `matching.rs`): `Song (feat. X)` ≡ `Song`; `Song - Radio Edit` ≡ `Song`; `[Remastered 2011]` отбрасывается; `Song (Live)`, `Song (Remix)`, `Song (Acoustic)`, `Song (Instrumental)` ≠ `Song`; `Ёлка` ≡ `Елка`; ISRC совпал → `Isrc` даже при разных названиях; разница 2001 мс → `None`; два трека одного провайдера → `None`; нет артистов → `None`.
- [ ] **Step 2:** Реализовать; `unicode-normalization` подключить только для NFKC.
- [ ] **Step 3:** Коммит `[FEATURE] Add conservative track matching rules`.

### Task 8: Группы в `store` (миграция v10)

**Files:** Create `crates/store/src/groups.rs`; Modify `crates/store/src/lib.rs` (`put_tracks` вызывает сопоставление в той же транзакции).

**Interfaces — Produces:** таблицы `track_groups`, `track_keys`, `artist_groups` по §5; `Store::group_of(&TrackId) -> Result<Option<u64>>`; `Store::group_members(grp) -> Vec<TrackId>`; `Store::artist_group(&TrackId) -> Option<u64>`.

- [ ] **Step 1:** Тесты: новый трек получает группу `self`; тот же ISRC у другого провайдера → одна группа (`isrc`); совпадение ключа → `meta`; второй трек того же провайдера не входит в группу; тезки-исполнители без общей группы треков остаются раздельно, с общей — склеиваются; миграция v9→v10 бэкфиллит группы из `tracks` без потери данных; повторная запись трека идемпотентна.
- [ ] **Step 2:** Реализовать в транзакции `put_tracks` (никаких фоновых проходов); бэкфилл при миграции — пакетами, чтобы не блокировать старт надолго.
- [ ] **Step 3:** Коммит `[FEATURE] Group identical tracks across providers`.

### Task 9: Подмена источника и возможностей, поиск по нескольким провайдерам

**Files:** Modify `crates/app/src/{playback.rs,downloads.rs,session.rs,music.rs}`; Create `crates/app/src/source.rs`.

**Interfaces — Produces:** `fn candidates(store, providers, id: &TrackId) -> Vec<TrackId>` (группа, по приоритету; сначала уже сохранённый/в кэше); `fn resolve_with_fallback(...) -> Result<(TrackId, Stream), ProviderError>` — правила перехода из §3 (`Unavailable|UnsupportedQuality|NotFound|Auth(невошедший пропускается)|Uncertain|ApiChanged` → следующий, с `log::warn!`; `Network|RateLimited` → только если следующий кандидат у другого провайдера); `Providers::first_with(f) -> Option<…>` для действий по возможности.

- [ ] **Step 1:** Интеграционные тесты `app` на `FakeProvider` `fakea`/`fakeb`: трек `Unavailable` у `fakea` играет версию `fakeb`; `Network` у единственного кандидата не вызывает переход; `Auth` у невошедшего `fakea` пропускается; панель «Сейчас играет» получает реально сыгравший источник; радио/тексты/страница исполнителя берутся у первого по приоритету провайдера с возможностью; при отсутствии — элемент неактивен с подсказкой «Подключите сервис с этой функцией».
- [ ] **Step 2:** Поиск: запрос идёт всем вошедшим провайдерам с `search_*` последовательно в существующем каталожном потоке; результаты отправляются по мере прихода; устаревшие отбрасываются по `view_generation`; результат склеивается по группам (версия провайдера с высшим приоритетом + список источников в поле результата). Тест: два провайдера, один медленный — первые результаты приходят до ответа второго; потоков не добавляется.
- [ ] **Step 3:** Коммит `[FEATURE] Source fallback, capability fallback and merged search`.

### Task 10: Вход/выход по провайдеру и состояние UI

**Files:** Modify `crates/app/src/session.rs` (`Command::Login/Logout/Restore`, `Event`), `crates/ui/src/{bridge.rs,lib.rs}`, `crates/ui/ui/*` (экран входа), `crates/cli`.

**Interfaces — Produces:** `Command::Login { provider: String, token: Token }`, `Command::Logout { provider: String, keep_saved: bool }`, `Event::Providers(Vec<ProviderStatus { id, name, auth, signed_in, capabilities }>)` (вместо `signed_in: bool`); команда «скопировать плейлист провайдера в локальный» — только если локальные плейлисты существуют, иначе откладывается в проект «вход/выход» (зафиксировать в коде комментарием-ссылкой на спецификацию не нужно, достаточно в описании коммита).

- [ ] **Step 1:** Тесты `app` на двух фейках: `Logout{fakea, keep_saved:false}` забывает токен (keyring), сбрасывает `account:fakea`, удаляет сохранённое `fakea`-аккаунта, **не трогает** лайки и данные `fakeb`; `keep_saved:true` оставляет сохранённое; `Restore` проверяет токены всех провайдеров с сохранённым токеном, невалидный не мешает остальным.
- [ ] **Step 2:** Реализовать; удалить `DELETE … 'yandex:%'` из `switch_account` — смена аккаунта чистит данные именно этого провайдера по `provider_id`-префиксу, полученному из `info().id`.
- [ ] **Step 3:** UI: экран входа строится по списку `ProviderStatus` (`AuthMethod::Token` → поле токена с `hint`/`help_url`); элементы без провайдера с нужной возможностью неактивны с подсказкой. Тексты «создан в Яндекс Музыке» берут `info().name`.
- [ ] **Step 4:** Коммит `[FEATURE] Per-provider login, logout and status`.

### Task 11: Журнал профиля и локальные лайки (миграция v11–v12)

**Files:** Create `crates/store/src/{profile_ops.rs,likes.rs,like_outbox.rs}`; Modify `crates/store/src/lib.rs` (удалить `replace_likes`), `crates/app/src/{session.rs,music.rs}`.

**Interfaces — Produces:**
```rust
pub struct Op { pub device: String, pub seq: u64, pub hlc: u64, pub kind: String, pub key: String, pub body: String }
Store::ops_since(&self, seen: &HashMap<String, u64>) -> Result<Vec<Op>, Error>;
Store::apply_ops(&self, ops: Vec<Op>) -> Result<(), Error>;   // идемпотентно, коммутативно
Store::like(&self, id: &TrackId, liked: bool) -> Result<(), Error>; // op + liked + like_outbox в одной транзакции
```
Таблицы: `profile_ops`, `profile_devices`, `liked(grp, at)`, `like_outbox(provider, track, liked, state, …)`, `provider_liked_seen(provider, track, synced_at)`. `grp` в операции не пишется; лайк = `TrackId` + снимок метаданных.

- [ ] **Step 1:** Property-тесты журнала: один набор операций в произвольном порядке и повторно (≥ 3 перестановок, дубликаты) → одно и то же состояние `liked`; надгробие побеждает более старую запись и не воскрешается позднейшим слиянием; ничья `hlc` решается по `device_id`; лайк `yandex:1`, созданный на A, после `apply_ops` на B попадает в локальную группу B (трек в B сопоставляется по §5, снимок метаданных сохраняет его без сети); HLC монотонен при расхождении часов.
- [ ] **Step 2:** Тесты лайков: `like()` пишет op + `liked` + задания `like_outbox` для каждого вошедшего провайдера с `likes`, у которого в группе есть трек; `Uncertain` не повторяется автоматически; `RateLimited` откладывает; синхронизация объединяет лайки провайдера; снятие у провайдера удаляет локально **только** если трек был в `provider_liked_seen` и локальный `at` ≤ прошлой синхронизации; лайк, никогда не стоявший у провайдера, не трогается; «лайкнул локально после снятия на сервере» → остаётся.
- [ ] **Step 3:** Миграция: `likes` → `liked` через группы, каждый лайк записывается как операция текущего устройства; `likes` удаляется. Сжатие журнала (решение 6) с тестом: журнал не растёт безгранично при 10 000 операций и старт не читает `profile_ops` целиком (проверка по числу прочитанных строк/EXPLAIN).
- [ ] **Step 4:** Фоновая отправка `like_outbox` — на существующем потоке отчётов (`play_outbox`-образец), без нового потока.
- [ ] **Step 5:** Коммит `[FEATURE] Profile op journal and local likes`.

### Task 12: Согласие на отправку старых лайков и `provider_order`

**Files:** Modify `crates/app/src/{session.rs,providers.rs}`, `crates/store/src/likes.rs`.

**Interfaces — Produces:** `Event::LikesBackfillOffer { provider, count }`, `Command::LikesBackfill { provider, accept: bool }`; настройки `likes_backfill:<provider>` (`yes|no`) и `provider_order` (дробные позиции для порядка, переносимо).

- [ ] **Step 1:** Тесты на фейках: подключение `fakeb` с `likes` при 3 локальных лайках, у которых в группе есть трек `fakeb` и которых нет у `fakeb` → `LikesBackfillOffer{count:3}`, ничего не отправлено; `accept:true` → задания в `like_outbox` пачкой и новые связи отправляются автоматически; `accept:false` → не отправляются, пока не включено в настройках; новые лайки после подключения уходят сразу; `count == 0` → предложения нет.
- [ ] **Step 2:** Реализовать; порядок провайдеров меняется только через настройку (экрана нет).
- [ ] **Step 3:** Коммит `[FEATURE] Likes backfill consent and provider order`.

### Task 13: Финальные проверки и чистка

**Files:** Create `crates/app/tests/no_provider_names.rs`; Modify `AGENTS.md` (ссылка на SDK/правила добавления провайдера).

- [ ] **Step 1:** Тест-grep: в `crates/app`, `crates/store`, `crates/ui` нет `"yandex"` и `'yandex:` вне `#[cfg(test)]`, `tests/` и `builtin()` (список исключений — явный); сначала красный, добить хвосты (`fixture.rs`, `benchmark.rs`, `ui`-обработчики).
- [ ] **Step 2:** Тесты: приложение стартует без вошедших провайдеров; пользователь с одним Яндексом после миграции v8→последняя: лайки/плейлисты/сохранённое/история/позиция на месте (копия реальной схемы v8 как фикстура).
- [ ] **Step 3:** `cargo fmt --check`, `cargo test`, `cargo clippy` (если применимо), `cargo xtask bench` — память и пробуждения не хуже baseline Task 0; результаты в описание финального PR.
- [ ] **Step 4:** Коммит `[DEV] Enforce provider-agnostic app, store and ui`.

---

## Распределение по воркерам (экономия токенов)

Воркеры — субагенты `Agent` с `model: "haiku"` (общий `general-purpose`; **не** `opencode-worker`: AGENTS.md проекта запрещает OpenCode). Главный (я) — архитектор и интегратор: решения по интерфейсам, слияние, ревью диффов, финальные прогоны. Каждому воркеру в промпте: ссылка на спецификацию, раздел «Global Constraints», блок Interfaces своей задачи, список файлов (читать только их через `codegraph_explore`), запрет трогать чужие файлы, требование вернуть краткую сводку (изменённые файлы, результат `cargo test -p <crate>`). Воркеры работают в `isolation: "worktree"` там, где задачи идут параллельно.

| Задача | Кто | Почему |
|---|---|---|
| T0 baseline | я | запуск bench, ничтожно мало токенов |
| T1 вынос SDK | haiku | механический перенос + реэкспорт |
| T2 info/Capabilities | я пишу типы и трейт; haiku правит все реализации `Provider` (`name`→`info`) | дизайн API — моё, массовая правка — воркеру |
| T3 `Http` + Yandex на SDK | я определяю `Http`; haiku переносит Yandex, keyring→platform, `impl Http for Client` | граница зависимостей критична, перенос однотипный |
| T4 testkit/FakeProvider/шаблон/README | haiku | изолированный новый код по спеку |
| T5 реестр | я (ядро `Providers`, `builtin`); haiku протягивает реестр через `downloads/playback/music/fixture/benchmark` и правит конструкторы тестов | |
| T6 профиль + миграция v9 | я | риск потери данных пользователей |
| T7 matching | haiku (параллельно с T4/T6; чистая функция, тесты заданы) | |
| T8 группы в store | haiku по моему интерфейсу; я ревью схемы/транзакций | |
| T9 подмена источника, поиск | я | логика ошибок и потоков |
| T10 вход/выход | я (команды/события); haiku — UI-экран и тесты на фейках | |
| T11 журнал и лайки | я (модель операций, правило синхронизации); haiku — property-тесты и `like_outbox` | |
| T12 согласие на лайки | haiku по готовым интерфейсам T11 | |
| T13 grep-тест, чистка, bench | haiku чистит хвосты; я — финальные проверки | |

Порядок/параллелизм: T0 → T1 → T2 → (T3 ‖ T4 ‖ T7) → T5 → T6 → T8 → T9 → T10 → T11 → T12 → T13. Параллельные воркеры не пересекаются по файлам (T3: yandex/net/platform; T4: provider-sdk testkit/_template; T7: core/matching.rs). После каждого воркера: `git diff --stat`, `cargo fmt --check`, `cargo test` — только потом коммит; провалы возвращаю воркеру с точным выводом, а не чиню сам, если правка механическая.

## Самопроверка покрытия спецификации

§1 → T1,T3,T4 · §2 → T2 · §3 → T5,T9 · §4 → T6,T10 · §5 → T7,T8 · §6 → T11,T12 · §7 → T3,T10 · §8 → T11 · §9 → тесты в T4–T13. Вне плана (по спецификации): экраны входа/выхода и диалоги, VK, перенос профиля, «Все треки», «волна».

## Верификация end-to-end

1. `cargo fmt --check && cargo test` — зелёные после каждой задачи.
2. `cargo xtask bench` до (Task 0) и после (Task 13): память и пробуждения не хуже.
3. Миграция на копии реальной базы v8: открыть новой сборкой, сверить число лайков/плейлистов/сохранённого/истории и позицию воспроизведения.
4. Ручная проверка десктопа (`run`): вход токеном Яндекса, поиск, воспроизведение, лайк, выход с сохранением лайков.
5. `cargo tree -p eagle-yandex` не содержит `app/store/ui/engine`.

# Платформа провайдеров — план исправлений

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Закрыть расхождения ветки `feature/provider-platform` (до `91ce1f0`) с дизайном перед слиянием: потеря лайков в миграции v12, доставка лайков без повторов, поиск карточек по одному провайдеру, непрогнанная проверка Яндекса, ненастоящая схема v8, гигиена и замеры.

**Architecture:** Исправления ложатся в существующие модули: `store` (миграции, `likes.rs`, `like_outbox.rs`), `app` (`session.rs` — цикл `Worker::run`, `Catalog`), `provider-sdk/testkit` (запись и воспроизведение HTTP), `providers/yandex/tests`. Новых крейтов и потоков нет.

**Tech Stack:** Rust 2024 (MSRV 1.92), rusqlite, Slint, блокирующий `Http` из SDK, `cargo xtask bench`.

**Spec:** `docs/superpowers/specs/2026-11-13-provider-platform-design.md` (план исполнения ветки: `docs/superpowers/plans/2026-11-13-provider-platform.md`).

## Global Constraints

- Rust edition 2024, MSRV 1.92, Google Rust Style + `rustfmt`; ошибки через `thiserror`, логи через `log` с kv; без `tracing`; без `unwrap`/`expect` в рабочем коде.
- Без новых зависимостей, кроме dev-зависимостей тестов, если без них не обойтись.
- На info в логах — только provider/track_id; секреты только в `Token`/`SignedUrl`, `expose()` не логировать.
- Ни одна миграция не удаляет данные пользователя, которые нельзя восстановить: лучше оставить строку неразобранной, чем потерять.
- Права на офлайн привязаны к аккаунту провайдера — не ослаблять.
- Без пробуждений в фоне, когда делать нечего: таймер повтора лайков существует только пока есть отложенные задания.
- В `crates/app`, `crates/store`, `crates/ui` вне `providers::builtin` и миграций нет имён провайдеров (тест `no_provider_names` остаётся зелёным).
- Коммиты: `[FIX] …`, `[FEATURE] …`, `[DEV] …`; перед коммитом `cargo fmt --check` и `cargo test -p <затронутый крейт>`; в конце задачи 7 — весь workspace.

## Review Focus

1. **Лайк трека провайдера, которого нет в сборке** (например, старый `vk:…`) при миграции и при разборе — остаётся в `likes_unresolved` навсегда, без паники и без бесконечных запросов. Тест — задача 1, шаг 1 (`migration_keeps_likes_of_unknown_providers`) и задача 2, шаг 1.
2. **Ноутбук спал с отложенными лайками, сеть всё ещё недоступна** — после пробуждения одна попытка, затем интервал растёт; горячего цикла нет. Тест — задача 3 (`network_failures_back_off_and_never_spin`).
3. **Согласие на 500 старых лайков** — поиск, поставленный в очередь во время отправки, отвечает раньше, чем уйдёт вся пачка. Тест — задача 3 (`search_is_answered_between_like_batches`).
4. **Записанные ответы Яндекса** не содержат токена, uid, логина, почты и медиатеки пользователя. Тест — задача 5 (`recordings_contain_no_secrets`).
5. **Поиск исполнителей, когда один провайдер упал или вышел из аккаунта посреди запроса** — ответ остальных показывается, ошибка только если упали все. Тест — задача 4 (`card_search_survives_one_failing_provider`).

---

### Task 1: Миграция лайков без потерь

**Files:**
- Modify: `crates/store/src/likes.rs` (`migrate_likes`, новые функции разбора)
- Modify: `crates/store/src/lib.rs` (шаг `version < 13`)
- Modify: `crates/store/src/lib.rs` или `crates/store/src/tracks.rs` — там, где живёт `put_tracks` (вызов разбора в той же транзакции)
- Test: `crates/store/src/likes.rs` (`mod tests`)

**Interfaces:**
- Produces:
  - таблица `likes_unresolved(track TEXT PRIMARY KEY, at INTEGER NOT NULL)`;
  - `pub fn unresolved_likes(&self, provider: &str, limit: usize) -> Result<Vec<TrackId>, Error>` — по возрастанию `at`;
  - `pub fn unresolved_likes_count(&self) -> Result<usize, Error>`;
  - `pub(crate) fn resolve_likes_in(tx: &Transaction, tracks: &[String]) -> Result<usize, Error>` — для каждого `track` из `likes_unresolved`, у которого теперь есть строка в `tracks`, вызывает `set_liked_in(tx, track, true)` и удаляет его из `likes_unresolved`; возвращает число разобранных.

- [ ] **Step 1: Тесты**

```rust
#[test]
fn migration_moves_likes_without_metadata_to_unresolved() {
    // v11-база: likes = {yandex:1 (есть в tracks), yandex:2 (нет в tracks)}
    // после Store::open:
    assert_eq!(liked_ids(&store), ["yandex:1"]);
    assert_eq!(store.unresolved_likes("yandex", 10).unwrap(), [id("yandex:2")]);
    assert_eq!(table_exists(&store, "likes"), false);
}
#[test]
fn migration_keeps_likes_of_unknown_providers() {
    // likes = {vk:7} без метаданных
    assert_eq!(store.unresolved_likes("vk", 10).unwrap(), [id("vk:7")]);
}
#[test]
fn storing_the_track_resolves_its_like_with_a_journal_op() {
    // после миграции yandex:2 неразобран; store.put_tracks(&[track("yandex:2")])
    assert!(store.is_liked(&id("yandex:2")).unwrap());
    assert!(store.unresolved_likes("yandex", 10).unwrap().is_empty());
    assert_eq!(like_ops_for(&store, "yandex:2"), 1);
}
#[test]
fn v12_database_gets_the_unresolved_table() {
    // база, уже мигрированная старым кодом до user_version=12 (без likes_unresolved)
    // Store::open → user_version == 13, таблица likes_unresolved существует, liked не изменился
}
```

- [ ] **Step 2: Запустить — падают**

Run: `cargo test -p eagle-store likes::tests`
Expected: FAIL — `unresolved_likes` не определена.

- [ ] **Step 3: Реализация**

В `migrate_likes`: создать `likes_unresolved` (`CREATE TABLE IF NOT EXISTS`); на `Err(Error::UnknownTrack)` — `INSERT OR IGNORE INTO likes_unresolved VALUES(id, rowid)` вместо счётчика `lost`; `log::warn!` убрать или заменить на `info!(count)` без потери. Новый шаг в `lib.rs`: `if version < 13 { CREATE TABLE IF NOT EXISTS likes_unresolved(...); PRAGMA user_version=13 }` — для баз, уже прошедших старую v12. В `put_tracks` после записи треков в той же транзакции — `resolve_likes_in(tx, &ids)`.

- [ ] **Step 4: Запустить — проходят**

Run: `cargo test -p eagle-store`
Expected: PASS (включая существующий `migrate_v8`).

- [ ] **Step 5: Commit**

```bash
git commit -am "[FIX] Keep likes of tracks without metadata through the v12 migration"
```

---

### Task 2: Разбор неразобранных лайков через провайдера

**Files:**
- Modify: `crates/app/src/session.rs` (`Catalog`, обработка завершения синхронизации и `Login`/`Restore`)
- Test: `crates/app/src/session.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Store::unresolved_likes`, `Store::put_tracks` (задача 1).
- Produces: `fn resolve_unresolved_likes(&self, provider: &dyn Provider)` в `Catalog`; константа `UNRESOLVED_BATCH: usize = 50`.

- [ ] **Step 1: Тесты**

```rust
#[test]
fn sync_fetches_metadata_for_unresolved_likes() {
    // store: likes_unresolved = {fake-a:3}; FakeProvider("fake-a").with_tracks([fake-a:3]); вход + SyncLibrary
    assert!(store.is_liked(&id("fake-a:3")).unwrap());
    assert_eq!(store.unresolved_likes_count().unwrap(), 0);
}
#[test]
fn unresolved_likes_of_missing_tracks_stay_and_are_not_refetched_in_a_loop() {
    // likes_unresolved = {fake-a:404, vk:7}; провайдер отвечает NotFound на fake-a:404
    // после SyncLibrary: оба остаются; track() вызван для fake-a:404 ровно 1 раз за синхронизацию; для vk:7 — 0 раз
}
```

- [ ] **Step 2: Запустить — падают**

Run: `cargo test -p eagle-app unresolved`
Expected: FAIL.

- [ ] **Step 3: Реализация**

После успешной синхронизации лайков провайдера (и после `Login`/`Restore`) `Catalog` берёт `unresolved_likes(provider_id, UNRESOLVED_BATCH)`, для каждого вызывает `provider.track(id)`; `Ok(track)` → `store.put_tracks(&[track])` (разбор произойдёт в store); `NotFound`/`Unavailable` — пропустить; `Network`/`RateLimited`/`Auth` — прервать до следующей синхронизации. Провайдеры, которых нет в реестре, не опрашиваются.

- [ ] **Step 4: Запустить — проходят**

Run: `cargo test -p eagle-app`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "[FIX] Resolve migrated likes once their tracks are known"
```

---

### Task 3: Доставка лайков с повтором и пачками

**Files:**
- Modify: `crates/store/src/like_outbox.rs`
- Modify: `crates/store/src/lib.rs` (новый шаг `version < 14`: колонка `attempts`; отдельный шаг, потому что базы на машинах разработчиков могут уже стоять на 13 после задачи 1)
- Modify: `crates/app/src/session.rs` (`Worker::run`, `Catalog::flush_likes`, `CatalogRequest`, `CatalogReply`)
- Modify: `crates/app/src/downloads.rs` (проброс события «сеть появилась»)
- Test: `crates/store/src/like_outbox.rs`, `crates/app/src/session.rs`

**Interfaces:**
- Produces (store):
  - миграция v14: `ALTER TABLE like_outbox ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0; PRAGMA user_version=14`;
  - `pub fn defer_like_job_backoff(&self, job: &LikeJob, now: i64) -> Result<i64, Error>` — `attempts += 1`, `retry_at = now + min(30 * 2^(attempts-1), 1800)`; возвращает новый `retry_at`;
  - `pub fn next_like_retry_at(&self) -> Result<Option<i64>, Error>` — `MIN(retry_at)` по `state='pending'` всех провайдеров;
  - успешная отправка (`finish_like_job`) не меняется.
- Produces (app):
  - `CatalogRequest::FlushLikes`; `CatalogReply::LikesPending` — в очереди ещё есть готовые задания;
  - `const LIKE_BATCH: usize = 20;` — максимум заданий за один `flush_likes`;
  - `Message::NetworkOnline`;
  - `Downloads::start(…, on_online: Box<dyn Fn() + Send + Sync>)` — вызывается из существующего колбэка `NetworkMonitor` при переходе в «сеть есть». Второй монитор не создавать.

- [ ] **Step 1: Тесты store**

```rust
#[test]
fn network_failures_back_off_and_never_spin() {
    // задание fake:1; now = 1000
    assert_eq!(store.defer_like_job_backoff(&job, 1000).unwrap(), 1030);
    assert_eq!(store.defer_like_job_backoff(&job, 1030).unwrap(), 1090);
    // … после 7-й неудачи интервал упирается в 1800
    assert_eq!(store.next_like_retry_at().unwrap(), Some(/* последний retry_at */));
    assert!(store.like_jobs("fake", /* retry_at - 1 */).unwrap().is_empty());
}
#[test]
fn empty_outbox_has_no_retry_deadline() {
    assert_eq!(store.next_like_retry_at().unwrap(), None);
}
```

- [ ] **Step 2: Тесты app**

```rust
#[test]
fn like_made_offline_is_delivered_after_the_retry_deadline() {
    // FakeProvider fail_with(Network); Command::Like(fake:1) → set_liked_calls() == []
    // clear_failure(); продвинуть время за retry_at (часы Worker через тестовый хук) → FlushLikes
    assert_eq!(fake.set_liked_calls(), [(id("fake:1"), true)]);
}
#[test]
fn network_coming_back_flushes_pending_likes() {
    // как выше, но вместо времени — Message::NetworkOnline
}
#[test]
fn search_is_answered_between_like_batches() {
    // 45 заданий в like_outbox; FlushLikes и сразу Command::Search("x")
    // после первого ответа Tracks: fake.set_liked_calls().len() == 20
    // в итоге все 45 отправлены
}
#[test]
fn idle_worker_has_no_like_timer() {
    // like_outbox пуст → Worker ждёт через recv() без таймаута (проверить вычисление дедлайна: None)
}
```

- [ ] **Step 3: Запустить — падают**

Run: `cargo test -p eagle-store like_outbox && cargo test -p eagle-app like`
Expected: FAIL.

- [ ] **Step 4: Реализация**

- `flush_likes`: `Network` → `defer_like_job_backoff` и прервать этого провайдера (вместо `break` без отметки); `RateLimited` — тоже через backoff; `Auth` — прервать без отметки (вход вызовет flush). Отправить не больше `LIKE_BATCH`; если готовые задания остались — ответить `CatalogReply::LikesPending`, и `Worker` снова кладёт `FlushLikes` в конец очереди каталога (так запросы пользователя проходят между пачками).
- `Worker::run`: дедлайн ожидания = минимум из «чекпоинт воспроизведения» (если играет) и `next_like_retry_at()` (если есть). Перечитывать `next_like_retry_at` после каждого ответа каталога, связанного с лайками. При таймауте лайков — `FlushLikes`. Нет дедлайнов — `recv()`.
- `Message::NetworkOnline` → `FlushLikes`.
- Часы в тестах — через уже существующий в `Worker` способ подмены времени, если он есть; иначе поле `clock: fn() -> i64` только под `#[cfg(test)]`.

- [ ] **Step 5: Запустить — проходят**

Run: `cargo test -p eagle-store && cargo test -p eagle-app`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git commit -am "[FIX] Retry like delivery with backoff, on reconnect and in batches"
```

---

### Task 4: Поиск исполнителей, альбомов и плейлистов по всем провайдерам

**Files:**
- Modify: `crates/app/src/session.rs` (`Command::SearchArtists|SearchAlbums|SearchPlaylists` в `Catalog`, новая `search_cards_merged`; у `Card` поле `sources`, если его нет)
- Modify: `crates/ui/src/bridge.rs` — только если без этого не компилируется (`sources` в UI не отрисовывается — это проект «интерфейс»)
- Test: `crates/app/src/session.rs`

**Interfaces:**
- Consumes: `Providers::with_capability`, `Store::artist_group`, `Store::put_tracks`, `artist_card`/`album_card`/`playlist_card`.
- Produces: `fn search_cards_merged(&self, kind: CardKind, query: &str, emit: &dyn Fn(CatalogReply)) -> Result<CatalogReply>`; `enum CardKind { Artists, Albums, Playlists }`; `Card { …, sources: Vec<TrackId> }`.

- [ ] **Step 1: Тесты**

```rust
#[test]
fn artist_search_merges_the_same_artist_across_providers() {
    // fake-a и fake-b: исполнитель «Same» с общим треком (сначала put_tracks обоих треков → общая artist group)
    // SearchArtists("same") → одна карточка, id от fake-a (выше приоритет), sources == [fake-b:<artist>]
}
#[test]
fn namesakes_without_a_shared_track_stay_separate() {
    // «Same» у обоих, общих треков нет → две карточки
}
#[test]
fn albums_and_playlists_are_listed_by_provider_priority_without_merging() {
    // порядок [fake-b, fake-a] → сначала карточки fake-b, затем fake-a
}
#[test]
fn card_search_survives_one_failing_provider() {
    // fake-a fail_with(Network) → карточки fake-b; оба падают → Err
}
#[test]
fn card_search_reports_first_results_before_the_slow_provider() {
    // fake-b with_delay(200 ms) → первый emit содержит только карточки fake-a
}
```

- [ ] **Step 2: Запустить — падают**

Run: `cargo test -p eagle-app card_search artist_search namesakes albums_and_playlists`
Expected: FAIL.

- [ ] **Step 3: Реализация**

По образцу `search_merged`: опрос провайдеров с нужной возможностью по приоритету, частичный `emit` после каждого, кроме последнего. Исполнители: карточка сливается с уже показанной, если `store.artist_group` у обоих `Some` и равны. Альбомы и плейлисты не сливаются (правил склейки для них в дизайне нет). Ошибка — только если не ответил ни один.

- [ ] **Step 4: Запустить — проходят**

Run: `cargo test -p eagle-app`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "[FIX] Search artists, albums and playlists across all providers"
```

---

### Task 5: Проверка Яндекса на записанных ответах

**Files:**
- Create: `crates/provider-sdk/src/testkit/replay.rs`
- Modify: `crates/provider-sdk/src/testkit/mod.rs`
- Create: `crates/providers/yandex/tests/conformance.rs`
- Create: `crates/providers/yandex/tests/recorded/` (записи)
- Modify: `crates/providers/yandex/src/lib.rs` (функция очистки ответов; удалить комментарий у `info_passes_conformance_checks`)
- Modify: `crates/provider-sdk/README.md` (раздел «Записать ответы для своего провайдера»)

**Interfaces:**
- Produces:
  - `pub struct RecordingHttp { … }` — `RecordingHttp::new(inner: Arc<dyn Http>, dir: PathBuf, scrub: fn(&str /*path*/, &mut serde_json::Value))`, реализует `Http`; пишет `NNN-<method>-<path-slug>.json` с `{method, path, query (отсортирован, без параметров-подписей), status, body}`; бинарное тело — base64; заголовок авторизации и `Request.token` не пишутся никогда;
  - `pub struct ReplayHttp { … }` — `ReplayHttp::open(dir: &Path) -> io::Result<Self>`, реализует `Http`; сопоставляет по `method + path + query`; нет записи → `HttpError` с понятным текстом «нет записи для …»;
  - в `eagle-yandex`: `pub fn scrub_recording(path: &str, body: &mut serde_json::Value)` — заменяет uid, login, email, display name, телефоны на заглушки; ответы лайков и плейлистов урезает до 3 элементов.

- [ ] **Step 1: Тесты SDK**

```rust
#[test]
fn replay_returns_what_recording_saw() { /* FakeHttp → RecordingHttp во временную папку → ReplayHttp: тот же status и body */ }
#[test]
fn recording_never_writes_the_token() { /* Request с token "SECRET" и заголовком Authorization → ни в одном файле нет "SECRET" */ }
#[test]
fn replay_reports_a_missing_recording() { /* неизвестный путь → Err, текст содержит путь */ }
```

- [ ] **Step 2: Тесты Яндекса**

```rust
// crates/providers/yandex/tests/conformance.rs
#[test]
fn yandex_is_conformant_on_recorded_responses() {
    // конструктор Яндекса, принимающий Arc<dyn Http> (если нынешний `new` берёт не `dyn Http` — добавить `with_http`)
    let provider = Yandex::with_http(Arc::new(ReplayHttp::open(&recorded_dir()).unwrap()));
    provider.login(Token::new("recorded")).unwrap();
    assert!(conformance::run(&provider, &fixtures()).is_empty());
}
#[test]
fn recordings_contain_no_secrets() {
    // ни в одном файле recorded/ нет: "OAuth ", поля uid/login/email с не-заглушечными значениями,
    // и (если задана) переменная EAGLE_YANDEX_TOKEN
}
#[test]
#[ignore = "живой API: EAGLE_YANDEX_TOKEN=… cargo test -p eagle-yandex --test conformance -- --ignored record"]
fn record() { /* RecordingHttp поверх настоящего Http + scrub_recording → conformance::run → пусто */ }
```

`fixtures()`: `known_track`, `query`, `artist` — публичные каталожные объекты (исполнитель с большой дискографией, запрос из одного слова), `signed_in: true`.

- [ ] **Step 3: Запустить тесты SDK — падают, реализовать `replay.rs`, запустить — проходят**

Run: `cargo test -p eagle-provider-sdk --features testkit replay`
Expected: сначала FAIL, после реализации PASS.

- [ ] **Step 4: Человек: записать ответы**

Run (на машине пользователя): `EAGLE_YANDEX_TOKEN=<токен> cargo test -p eagle-yandex --test conformance -- --ignored record`
Expected: PASS, в `tests/recorded/` появились файлы. Просмотреть `git diff --stat` и выборочно файлы аккаунта глазами.

- [ ] **Step 5: Запустить проверку на записях**

Run: `cargo test -p eagle-yandex --test conformance`
Expected: PASS (`record` пропущен как ignored).

- [ ] **Step 6: Commit**

```bash
git add crates/provider-sdk crates/providers/yandex
git commit -m "[DEV] Run Yandex conformance on scrubbed recorded responses"
```

---

### Task 6: Настоящая база v8 для теста миграции

**Files:**
- Modify: `crates/store/tests/fixtures/v8.sql` (пересоздать)
- Modify: `crates/store/tests/migrate_v8.rs`

**Interfaces:**
- Consumes: `Store::unresolved_likes` (задача 1).

- [ ] **Step 1: Собрать базу кодом версии 8**

```bash
git worktree add ../eagle-v8 adbfa9e   # родитель c84de0c; проверить: в его lib.rs последняя миграция ставит user_version=8
```
В `../eagle-v8` временным (некоммитимым) примером `crates/store/examples/seed_v8.rs` через публичный API той версии создать в пустом каталоге: 2 трека с метаданными и лайками; 1 лайк трека без метаданных; плейлист; сохранённую загрузку с подтверждёнными правами аккаунта `u1`; 2 записи истории; позицию воспроизведения; настройки `account_uid:yandex=u1`, тему.
Run: `cargo run -p eagle-store --example seed_v8 -- /tmp/v8 && sqlite3 /tmp/v8/metadata.sqlite 'PRAGMA user_version'`
Expected: `8`.

- [ ] **Step 2: Выгрузить и убрать worktree**

Run: `sqlite3 /tmp/v8/metadata.sqlite .dump > crates/store/tests/fixtures/v8.sql && git worktree remove ../eagle-v8`
Expected: дамп содержит `PRAGMA user_version=8` (если `.dump` его не пишет — дописать последней строкой).

- [ ] **Step 3: Обновить тест и запустить**

К существующим проверкам добавить: `liked.len() == 2`, `store.unresolved_likes("yandex", 10) == [третий]`, после `put_tracks(&[третий])` — `liked.len() == 3`; история и позиция принадлежат `profile_id`; права офлайн — у `u1`; `user_version == 14`.
Run: `cargo test -p eagle-store --test migrate_v8`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/store/tests
git commit -m "[DEV] Migration test on a database written by the v8 code"
```

---

### Task 7: Гигиена перед слиянием

**Files:**
- Modify: по результатам `cargo deny` (`deny.toml`, `deny-ios.toml` — только если находка законная и обоснована комментарием)
- Modify: `crates/app/src/session.rs` (тип `logout_worker` → псевдоним `type LogoutHarness = (…)`)

- [ ] **Step 1: cargo deny**

Run: `cargo install cargo-deny --locked && cargo deny check && cargo deny --config deny-ios.toml check licenses bans sources`
Expected: `advisories ok, bans ok, licenses ok, sources ok`. Каждую находку чинить (обновить или убрать зависимость); исключение в `deny.toml` — только с комментарием-обоснованием.

- [ ] **Step 2: Убедиться, что `no_provider_names` ловит нарушение**

Временно вписать `let _ = "yandex";` в `crates/app/src/storage.rs`.
Run: `cargo test -p eagle-app --test no_provider_names`
Expected: FAIL с указанием файла. Откатить вставку, повторить — PASS.

- [ ] **Step 3: clippy и полный прогон**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: без предупреждений, все тесты PASS.

- [ ] **Step 4: Commit**

```bash
git commit -am "[DEV] Pass cargo deny and clippy before merging the provider platform"
```

---

### Task 8: Замеры воспроизведения, синхронизации и прокрутки

**Files:**
- Modify: `docs/benchmarking.md` (строки таблицы для ветки и `main`)

Порог (решение плана): пробуждения и CPU в простое и при воспроизведении — не больше, чем на `main`; память — рост не больше 64 КБ в `idle` и не больше 5 % в `playback`/`sync`/`scroll`. Превышение — разобраться до слияния, а не записывать.

- [ ] **Step 1: Человек: подготовить каталог с закэшированным треком** (как в `docs/benchmarking.md`, раздел «Для реального трека»).

- [ ] **Step 2: Замерить `main` и ветку на одной машине**

Run (для каждого: `main`, затем `feature/provider-platform`):
```bash
cargo xtask bench --scenario idle
cargo xtask bench --scenario playback --track yandex:<ID> --data-dir <каталог>
cargo xtask bench --scenario sync
cargo xtask bench --scenario scroll
```
Expected: CSV в `bench/results/`; разница в пределах порога.

- [ ] **Step 3: Записать таблицу в `docs/benchmarking.md` и закоммитить**

```bash
git commit -am "[DEV] Record provider platform measurements against main"
```

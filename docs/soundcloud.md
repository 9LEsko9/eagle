# SoundCloud

Eagle can search and play public SoundCloud tracks without an account or a paid
subscription. It discovers the public web client's identifier from the current
SoundCloud website and keeps it in memory; no developer credentials are required.
This uses an undocumented website interface, which can change without notice.
Guest playback requires tracks marked public, streamable and `ALLOW`; previews,
monetized/restricted tracks, encrypted streams and private tracks are excluded.
An optional web-session token may be entered in Eagle’s service screen; it is
validated against the web API and stored in the system secret store. This is an
experimental, undocumented interface, not an OAuth sign-in flow, and may stop
working or require a new token. Enter only the token for your own account, never
share it in chat. See the [Music Assistant instructions](https://www.music-assistant.io/music-providers/soundcloud/)
for locating it; enter the token value without the `OAuth ` prefix. Eagle does
not read browser cookies itself. Signing out removes the local token; guest
search remains available. A valid account may access a private `ALLOW` track by
its known ID when the server permits it. The account’s liked tracks are read page by page into the existing collection.
Likes are read-only on SoundCloud: changes in Eagle remain local and do not change
the SoundCloud account. Eagle has no private-link import.
Track metadata obtained through search is cached locally and is not erased on
sign-out. Remote like changes, playlists, downloads, previews, encrypted and
restricted streams are not supported. Account playback has not been verified
with a live account.

Desktop playback accepts finite unencrypted AAC HLS playlists with whole segments
and an optional initialization map. Master playlists, byte ranges and
discontinuities are unsupported. Segments load on demand with at most one in
memory; seeking reopens and decodes from the start when fast seeking is
unavailable. The CLI playback path does not support HLS.

Search and collection rows and the player panel credit the uploader and link the
provider’s logo to the original work, or to the uploader profile for a private track. This integration remains experimental.

# SoundCloud: русский

Eagle умеет искать и воспроизводить публичные треки SoundCloud без аккаунта и
платной подписки. Идентификатор общедоступного веб-клиента извлекается из текущего
сайта SoundCloud и хранится в памяти; учётные данные разработчика не нужны.
Используется недокументированный интерфейс сайта, который может измениться без
предупреждения. В гостевом режиме воспроизводятся только публичные доступные треки с политикой
`ALLOW`; превью, монетизированные и ограниченные треки, шифрованные потоки и
приватные треки исключены. На экране сервисов можно необязательно ввести токен собственного веб-сеанса;
Eagle проверит его через веб-API и сохранит в системном хранилище секретов.
Это экспериментальный недокументированный интерфейс, не OAuth-вход: он может
перестать работать или потребовать новый токен. Не отправляйте токен в чат.
Его расположение описано в [инструкции Music Assistant](https://www.music-assistant.io/music-providers/soundcloud/);
вводите только значение без префикса `OAuth `. Eagle не читает cookies браузера.
Выход удаляет локальный токен, гостевой поиск сохраняется. При наличии доступа
сервер может разрешить приватный трек с политикой `ALLOW` по известному ID.
Понравившиеся треки аккаунта постранично загружаются в существующую медиатеку.
Лайки SoundCloud доступны только для чтения: изменения в Eagle остаются локальными
и не меняют аккаунт SoundCloud. Импорт приватных ссылок не поддерживается. Метаданные
найденных треков сохраняются локально и не удаляются при выходе.
Изменение лайков на сервере, плейлисты, загрузки, превью, шифрованные и ограниченные потоки не
поддерживаются. Воспроизведение с аккаунтом пока не проверено на живом аккаунте.

В настольном приложении поддерживаются конечные незашифрованные AAC HLS-плейлисты
с целыми сегментами и необязательной картой инициализации. Мастер-плейлисты,
диапазоны байтов и разрывы не поддерживаются. Сегменты загружаются по мере
чтения, в памяти хранится не более одного сегмента; если быстрый переход не
поддерживается, выполняется повторное открытие и декодирование с начала. CLI
не воспроизводит HLS.

В строках поиска и медиатеки и в панели плеера указан автор загрузки; логотип
провайдера ведёт на оригинал или профиль автора для приватного трека. Интеграция остаётся экспериментальной.

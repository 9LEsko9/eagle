Записи ответов Яндекс Музыки для теста conformance (tests/conformance.rs).

Пока здесь нет файлов NNN-*.json, тест yandex_is_conformant_on_recorded_responses
пропускается с сообщением. Чтобы увидеть сообщение, запускайте с --nocapture:
  cargo test -p eagle-yandex --test conformance -- --nocapture

Записывает владелец аккаунта на своей машине, не агент и не CI:
  EAGLE_YANDEX_TOKEN=<токен> cargo test -p eagle-yandex --test conformance -- --ignored record

Команда удаляет старые NNN-*.json и записывает заново. Токен в файлы не попадает
(RecordingHttp не сохраняет заголовки и Request.token), но после записи проверьте глазами:
  git diff --stat
Перед записью каждый ответ очищает scrub_recording (src/lib.rs):
- uid, логин, почта, имя, фамилия, телефон, дата рождения, хеш и заказ подписки заменяются
  заглушками; uid в пути и в теле равен 1;
- даты, uuid и названия лайков и плейлистов, описания, обложки и аватары заменяются заглушками;
- ссылки скачивания (downloadInfoUrl) и ответы /get-mp3/ заменяются заглушкой
  (get-mp3/stub?sign=stub&ts=0 и короткий XML); настоящие байты не сохраняются;
- лайки: до 3 элементов, плейлисты: до 2, пакетный запрос треков: до 3, поиск: до 5 на каждый вид.

Проверка тестом recordings_contain_no_secrets падает на любой записи с настоящими данными.
После записи проверьте глазами:
  git diff --stat
Затем:
  cargo test -p eagle-yandex --test conformance

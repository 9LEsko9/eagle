Записи ответов Яндекс Музыки для теста conformance (tests/conformance.rs).

Пока здесь нет файлов NNN-*.json, тест yandex_is_conformant_on_recorded_responses
пропускается с сообщением. Чтобы увидеть сообщение, запускайте с --nocapture:
  cargo test -p eagle-yandex --test conformance -- --nocapture

Записывает владелец аккаунта на своей машине, не агент и не CI:
  EAGLE_YANDEX_TOKEN=<токен> cargo test -p eagle-yandex --test conformance -- --ignored record

Команда удаляет старые NNN-*.json и записывает заново. Токен в файлы не попадает
(RecordingHttp не сохраняет заголовки и Request.token), но после записи проверьте глазами:
  git diff --stat
В файлах не должно быть uid, логина, почты, имени или телефона аккаунта, а списки лайков
и плейлистов должны содержать не больше трёх элементов. Затем:
  cargo test -p eagle-yandex --test conformance

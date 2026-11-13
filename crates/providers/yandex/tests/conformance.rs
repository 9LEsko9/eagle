// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Yandex conformance on recorded answers. The recordings live in `tests/recorded/`; see the
//! README there for how the owner of the account writes them.

use eagle_net::Client;
use eagle_provider_sdk::{
    Provider, Token,
    testkit::{
        conformance::{self, Fixtures},
        replay::{RecordingHttp, ReplayHttp},
    },
};
use eagle_yandex::{Yandex, scrub_recording};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

const TOKEN_VAR: &str = "EAGLE_YANDEX_TOKEN";

fn recorded_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recorded")
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .expect("tests/recorded exists")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

fn json_recordings(dir: &Path) -> Vec<PathBuf> {
    files(dir)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

fn fixtures() -> Fixtures {
    Fixtures {
        // Streams in both qualities for this track were checked on the test account.
        known_track: "yandex:128582477".parse().expect("valid track id"),
        query: "Queen".into(),
        // No artist id has been verified yet, so the artist checks are skipped.
        artist: None,
        signed_in: true,
    }
}

#[test]
fn yandex_is_conformant_on_recorded_responses() {
    let dir = recorded_dir();
    if json_recordings(&dir).is_empty() {
        eprintln!(
            "пропуск: в {} нет записей ответов; см. README.txt в этом каталоге",
            dir.display()
        );
        return;
    }
    let replay = ReplayHttp::open(&dir).expect("recordings are readable");
    let provider = Yandex::new(Arc::new(replay));
    provider
        .login(Token::new("recorded".into()))
        .expect("the recorded account signs in");
    let violations = conformance::run(&provider, &fixtures());
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn recordings_contain_no_secrets() {
    let token = std::env::var(TOKEN_VAR)
        .ok()
        .filter(|token| !token.is_empty());
    for file in files(&recorded_dir()) {
        let text = fs::read_to_string(&file).expect("recording is UTF-8 text");
        assert!(
            !text.contains("OAuth "),
            "{}: заголовок авторизации",
            file.display()
        );
        if let Some(token) = &token {
            assert!(
                !text.contains(token.as_str()),
                "{}: записан токен из {TOKEN_VAR}",
                file.display()
            );
        }
        if file.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // Scrubbing a scrubbed recording must change nothing: anything else is account data.
        let record: Value = serde_json::from_str(&text).expect("recording is valid JSON");
        let mut path = record["path"].as_str().unwrap_or_default().to_owned();
        let mut body = record.get("body").cloned().unwrap_or(Value::Null);
        scrub_recording(&mut path, &mut body);
        assert_eq!(
            path,
            record["path"].as_str().unwrap_or_default(),
            "{}: в пути есть данные аккаунта",
            file.display()
        );
        assert_eq!(
            body,
            record.get("body").cloned().unwrap_or(Value::Null),
            "{}: в ответе есть данные аккаунта или лишние элементы",
            file.display()
        );
    }
}

/// Writes the recordings from the live service. Run it by hand, never in CI:
/// `EAGLE_YANDEX_TOKEN=<token> cargo test -p eagle-yandex --test conformance -- --ignored record`
#[test]
#[ignore = "живой API: EAGLE_YANDEX_TOKEN=… cargo test -p eagle-yandex --test conformance -- --ignored record"]
fn record() {
    let token = std::env::var(TOKEN_VAR).expect("задайте EAGLE_YANDEX_TOKEN");
    let dir = recorded_dir();
    for old in json_recordings(&dir) {
        fs::remove_file(old).expect("старую запись можно удалить");
    }
    let live = Client::new().expect("network client starts");
    let recorder = RecordingHttp::new(Arc::new(live), dir, scrub_recording);
    let provider = Yandex::new(Arc::new(recorder));
    provider
        .login(Token::new(token))
        .expect("вход по токену на живом сервисе");
    let violations = conformance::run(&provider, &fixtures());
    assert!(violations.is_empty(), "{violations:#?}");
}

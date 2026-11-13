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
use eagle_yandex::{STUB_UID, Yandex, scrub_recording};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

const TOKEN_VAR: &str = "EAGLE_YANDEX_TOKEN";
/// Text that a recording never contains: token prefixes, the signed download block, and the
/// avatar addresses of the real account.
const FORBIDDEN_TEXT: &[&str] = &[
    "OAuth",
    "y0_",
    "U2FsdGVk",
    "avatars.yandex.net/get-music-content/",
];
/// Parameters whose only value in a recording is the stub one.
const STUB_PARAMS: &[(&str, &str)] = &[("sign=", "stub"), ("ts=", "0")];
/// The longest block that may follow `get-mp3/` in a recording: a stub is far shorter.
const MAX_DOWNLOAD_SEGMENT: usize = 20;
/// File names are short: the number, the method and a path slug of at most 60 characters.
const MAX_FILE_NAME: usize = 80;

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

/// Characters that can make up a token, a signature or a block of a link.
fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Parameters in the text whose value is not the stub one, such as a real `sign=` or `ts=`.
fn stub_param_violations(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for &(key, stub) in STUB_PARAMS {
        for (at, _) in text.match_indices(key) {
            let after_word = text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            if after_word {
                continue;
            }
            let value: String = text[at + key.len()..]
                .chars()
                .take_while(|c| is_value_char(*c))
                .collect();
            if value != *stub {
                found.push(format!("{key}{value}"));
            }
        }
    }
    found
}

/// E-mail addresses in the text other than the stub one.
fn email_violations(text: &str) -> Vec<String> {
    const STUB_EMAIL: &str = "stub@example.invalid";
    text.match_indices('@')
        .filter_map(|(at, _)| {
            let before: String = text[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_alphanumeric() || "._-+".contains(*c))
                .collect::<Vec<char>>()
                .into_iter()
                .rev()
                .collect();
            let after: String = text[at + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || ".-".contains(*c))
                .collect();
            let email = format!("{before}@{after}");
            (email != STUB_EMAIL).then_some(email)
        })
        .collect()
}

/// Blocks that follow `get-mp3/` and are longer than a stub.
fn long_download_segments(text: &str) -> Vec<String> {
    const PREFIX: &str = "get-mp3/";
    text.match_indices(PREFIX)
        .map(|(at, _)| {
            text[at + PREFIX.len()..]
                .chars()
                .take_while(|c| is_value_char(*c))
                .collect::<String>()
        })
        .filter(|segment| segment.len() > MAX_DOWNLOAD_SEGMENT)
        .collect()
}

/// Every `uid` in a recording and the user id in its path must be the stub one.
fn uid_violations(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "uid" && item.as_str() != Some(STUB_UID) {
                    out.push(format!("uid {item}"));
                }
                uid_violations(item, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| uid_violations(item, out)),
        _ => {}
    }
}

fn path_uid_violations(path: &str) -> Vec<String> {
    let mut previous = "";
    let mut found = Vec::new();
    for segment in path.split('/') {
        if previous == "users" && !segment.is_empty() && segment != STUB_UID {
            found.push(format!("путь содержит uid {segment}"));
        }
        previous = segment;
    }
    found
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
        let shown = file.display();
        let text = fs::read_to_string(&file).expect("recording is UTF-8 text");
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        assert!(
            name.len() <= MAX_FILE_NAME && !name.contains("u2fsdg"),
            "{shown}: имя файла содержит блок ссылки или слишком длинное"
        );
        for marker in FORBIDDEN_TEXT {
            assert!(!text.contains(marker), "{shown}: найдено «{marker}»");
        }
        if let Some(token) = &token {
            assert!(
                !text.contains(token.as_str()),
                "{shown}: записан токен из {TOKEN_VAR}"
            );
        }
        let params = stub_param_violations(&text);
        assert!(params.is_empty(), "{shown}: не заглушка: {params:?}");
        let emails = email_violations(&text);
        assert!(
            emails.is_empty(),
            "{shown}: адрес почты не заглушка: {emails:?}"
        );
        let segments = long_download_segments(&text);
        assert!(
            segments.is_empty(),
            "{shown}: блок ссылки скачивания: {segments:?}"
        );
        if file.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // Scrubbing a scrubbed recording must change nothing: anything else is account data.
        let record: Value = serde_json::from_str(&text).expect("recording is valid JSON");
        let path = record["path"].as_str().unwrap_or_default();
        let mut uids = Vec::new();
        uid_violations(&record, &mut uids);
        assert!(uids.is_empty(), "{shown}: uid не заглушка: {uids:?}");
        let path_uids = path_uid_violations(path);
        assert!(path_uids.is_empty(), "{shown}: {path_uids:?}");

        let mut scrub_path = path.to_owned();
        let mut query = record["query"].as_str().unwrap_or_default().to_owned();
        // A binary answer is stored as base64; its stub body is checked through the path only.
        let binary = record.get("body_base64").is_some();
        let mut body = if binary {
            Value::Null
        } else {
            record.get("body").cloned().unwrap_or(Value::Null)
        };
        scrub_recording(&mut scrub_path, &mut query, &mut body);
        assert_eq!(
            scrub_path, path,
            "{shown}: в пути есть данные аккаунта или он не заглушка"
        );
        assert_eq!(
            query,
            record["query"].as_str().unwrap_or_default(),
            "{shown}: в запросе есть подпись или время"
        );
        if !binary {
            assert_eq!(
                body,
                record.get("body").cloned().unwrap_or(Value::Null),
                "{shown}: в ответе есть данные аккаунта или лишние элементы"
            );
        }
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

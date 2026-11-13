// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Records the answers of a real [`Http`] to files and replays them later, so conformance runs
//! do not need a live account.
//!
//! A recording is one `NNN-<method>-<path>.json` file: `method`, `path` (without the query),
//! `query` (sorted, without signature parameters), `status` and either `body` (JSON) or
//! `body_base64` (anything else). Headers and the request token are never written.

use crate::{Http, HttpError, Priority, Request, Response, Token};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Rewrites one recording before it is written: `path` is the request path without the query,
/// `body` the JSON answer. Binary answers pass `Value::Null` and are not rewritten.
pub type Scrub = fn(&mut String, &mut Value);

const WRITE_FAILED: &str = "не удалось записать ответ в каталог записей";
const REDACTED: &str = "[REDACTED]";

/// Forwards every request to `inner` and writes each answer to `dir`.
pub struct RecordingHttp {
    inner: Arc<dyn Http>,
    dir: PathBuf,
    scrub: Scrub,
    next: AtomicUsize,
}

impl RecordingHttp {
    pub fn new(inner: Arc<dyn Http>, dir: PathBuf, scrub: Scrub) -> Self {
        Self {
            inner,
            dir,
            scrub,
            next: AtomicUsize::new(0),
        }
    }

    fn write(
        &self,
        method: &str,
        path: String,
        query: String,
        status: u16,
        bytes: &[u8],
        secret: Option<&str>,
    ) -> Result<(), HttpError> {
        let mut path = path;
        let mut record = match serde_json::from_slice::<Value>(bytes) {
            Ok(mut body) => {
                (self.scrub)(&mut path, &mut body);
                json!({"method": method, "path": path, "query": query, "status": status, "body": body})
            }
            Err(_) => {
                (self.scrub)(&mut path, &mut Value::Null);
                let encoded = base64_encode(bytes);
                json!({"method": method, "path": path, "query": query, "status": status, "body_base64": encoded})
            }
        };
        if let Some(secret) = secret {
            redact(&mut record, secret);
        }
        let name = format!(
            "{}-{}",
            method.to_ascii_lowercase(),
            file_slug(record["path"].as_str().unwrap_or(""))
        );
        let mut file = self.create_file(&name)?;
        serde_json::to_writer_pretty(&mut file, &record)
            .map_err(|_| HttpError::Source(WRITE_FAILED))
    }

    /// Creates the next free `NNN-<name>.json`; existing files are never overwritten.
    fn create_file(&self, name: &str) -> Result<File, HttpError> {
        fs::create_dir_all(&self.dir).map_err(|_| HttpError::Source(WRITE_FAILED))?;
        loop {
            let index = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            let path = self.dir.join(format!("{index:03}-{name}.json"));
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => return Ok(file),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(_) => return Err(HttpError::Source(WRITE_FAILED)),
            }
        }
    }
}

impl Http for RecordingHttp {
    fn request(
        &self,
        priority: Priority,
        request: Request,
        refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
    ) -> Result<Response, HttpError> {
        let method = request.method;
        let secret = request
            .token
            .as_ref()
            .map(|token| token.expose().to_owned())
            .filter(|secret| !secret.is_empty());
        let (path, query) = split_url(request.url.expose());
        let result = self.inner.request(priority, request, refresh);
        let answer = match &result {
            Ok(response) => Some((response.status, response.bytes.clone())),
            Err(HttpError::Status(status)) => Some((*status, Vec::new())),
            // Network and host errors are not answers; the next call must ask again.
            Err(_) => None,
        };
        if let Some((status, bytes)) = answer {
            self.write(method, path, query, status, &bytes, secret.as_deref())?;
        }
        result
    }
}

#[derive(Clone)]
struct Answer {
    status: u16,
    bytes: Vec<u8>,
}

/// Answers from a directory written by [`RecordingHttp`]. Several recordings of one request are
/// answered in their recorded order; the last one repeats once the others are used up.
pub struct ReplayHttp {
    answers: Mutex<HashMap<String, VecDeque<Answer>>>,
}

impl ReplayHttp {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let mut files = fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<PathBuf>>>()?;
        files.retain(|file| file.extension().is_some_and(|ext| ext == "json"));
        files.sort();
        // Recorded order follows the number prefix, which is not zero-padded beyond 999 files.
        files.sort_by_key(|file| recording_index(file));
        let mut answers: HashMap<String, VecDeque<Answer>> = HashMap::new();
        for file in files {
            let record: Value = serde_json::from_slice(&fs::read(&file)?).map_err(|_| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("запись повреждена: {}", file.display()),
                )
            })?;
            let (key, answer) = parse_record(&record).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("неверный формат записи: {}", file.display()),
                )
            })?;
            answers.entry(key).or_default().push_back(answer);
        }
        Ok(Self {
            answers: Mutex::new(answers),
        })
    }
}

impl Http for ReplayHttp {
    fn request(
        &self,
        _priority: Priority,
        request: Request,
        _refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
    ) -> Result<Response, HttpError> {
        let (path, query) = split_url(request.url.expose());
        let key = request_key(request.method, &path, &query);
        let answer = {
            let mut answers = self
                .answers
                .lock()
                .map_err(|_| HttpError::Source("записи воспроизведения недоступны"))?;
            match answers.get_mut(&key) {
                Some(queue) if queue.len() > 1 => queue.pop_front(),
                Some(queue) => queue.front().cloned(),
                None => None,
            }
        };
        let answer = answer.ok_or_else(|| {
            // `HttpError::Source` takes a static text. A miss ends a test run, so leaking the
            // message once per missing request is cheap and keeps the host error type unchanged.
            let message = format!("нет записи для {key}; запишите ответы заново");
            HttpError::Source(Box::leak(message.into_boxed_str()))
        })?;
        if answer.status >= 400 {
            return Err(HttpError::Status(answer.status));
        }
        Ok(Response {
            status: answer.status,
            bytes: answer.bytes,
            content_range: None,
            etag: None,
        })
    }
}

/// The `NNN` of `NNN-<method>-<path>.json`; files without one sort last.
fn recording_index(file: &Path) -> u64 {
    file.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split('-').next())
        .and_then(|index| index.parse().ok())
        .unwrap_or(u64::MAX)
}

fn parse_record(record: &Value) -> Option<(String, Answer)> {
    let key = request_key(
        record["method"].as_str()?,
        record["path"].as_str()?,
        record["query"].as_str().unwrap_or(""),
    );
    let status = u16::try_from(record["status"].as_u64()?).ok()?;
    let bytes = match (record.get("body"), record.get("body_base64")) {
        (Some(body), _) => serde_json::to_vec(body).ok()?,
        (None, Some(encoded)) => base64_decode(encoded.as_str()?)?,
        (None, None) => return None,
    };
    Some((key, Answer { status, bytes }))
}

fn request_key(method: &str, path: &str, query: &str) -> String {
    format!("{method} {path}?{query}")
}

/// Splits `scheme://host/path?query` into the path and the canonical query.
fn split_url(url: &str) -> (String, String) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let start = rest.find('/').unwrap_or(rest.len());
    let (path, query) = rest[start..]
        .split_once('?')
        .unwrap_or((&rest[start..], ""));
    let path = if path.is_empty() { "/" } else { path };
    (path.to_owned(), canonical_query(query))
}

/// Sorted `key=value` pairs without signature parameters, so the same request always matches.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .filter(|(key, _)| !is_signature(key))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn is_signature(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["sig", "token", "auth", "key", "secret", "credential"]
        .iter()
        .any(|word| key.contains(word))
}

fn redact(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => {
            if text.contains(secret) {
                *text = text.replace(secret, REDACTED);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, secret)),
        Value::Object(map) => map.values_mut().for_each(|item| redact(item, secret)),
        _ => {}
    }
}

/// Lower-case ASCII words joined by `-`, at most 60 characters; never empty.
fn file_slug(path: &str) -> String {
    let mut slug = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "root".into()
    } else {
        slug.to_owned()
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in text.bytes().filter(|&c| c != b'=') {
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{RecordingHttp, ReplayHttp};
    use crate::{Http, HttpError, Priority, Request, Response, SignedUrl, Token};
    use serde_json::Value;
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    /// Answers by path and numbers its JSON answers, so repeated requests can be told apart.
    #[derive(Default)]
    struct FakeHttp {
        calls: AtomicUsize,
    }

    impl Http for FakeHttp {
        fn request(
            &self,
            _priority: Priority,
            request: Request,
            _refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
        ) -> Result<Response, HttpError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let url = request.url.expose();
            let bytes = if url.contains("/missing") {
                return Err(HttpError::Status(404));
            } else if url.contains("/binary") {
                vec![0, 159, 146, 150]
            } else if url.contains("/secure") {
                br#"{"echo":"SECRET"}"#.to_vec()
            } else {
                format!(r#"{{"result":{{"call":{call}}}}}"#).into_bytes()
            };
            Ok(Response {
                status: 200,
                bytes,
                content_range: None,
                etag: None,
            })
        }
    }

    fn keep(_path: &mut String, _body: &mut Value) {}

    fn get(url: &str) -> Request {
        Request::get(SignedUrl::new(url.into()))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eagle-replay-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn file_texts(dir: &PathBuf) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }

    #[test]
    fn replay_returns_what_recording_saw() {
        let dir = temp_dir("roundtrip");
        let inner = Arc::new(FakeHttp::default());
        let recorder = RecordingHttp::new(inner.clone(), dir.clone(), keep);
        let recorded = recorder
            .request(
                Priority::Metadata,
                get("https://api.example.test/items?b=2&a=1"),
                None,
            )
            .unwrap();

        let replay = ReplayHttp::open(&dir).unwrap();
        // The same query in another order is the same request.
        let replayed = replay
            .request(
                Priority::Metadata,
                get("https://api.example.test/items?a=1&b=2"),
                None,
            )
            .unwrap();

        assert_eq!(replayed.status, recorded.status);
        let recorded_json: Value = serde_json::from_slice(&recorded.bytes).unwrap();
        let replayed_json: Value = serde_json::from_slice(&replayed.bytes).unwrap();
        assert_eq!(replayed_json, recorded_json);
        // The second answer came from the file, not from the inner client.
        assert_eq!(inner.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn binary_bodies_round_trip_exactly() {
        let dir = temp_dir("binary");
        let recorder = RecordingHttp::new(Arc::new(FakeHttp::default()), dir.clone(), keep);
        recorder
            .request(
                Priority::Metadata,
                get("https://api.example.test/binary"),
                None,
            )
            .unwrap();

        let replay = ReplayHttp::open(&dir).unwrap();
        let replayed = replay
            .request(
                Priority::Metadata,
                get("https://api.example.test/binary"),
                None,
            )
            .unwrap();
        assert_eq!(replayed.bytes, vec![0, 159, 146, 150]);
    }

    #[test]
    fn recording_never_writes_the_token() {
        let dir = temp_dir("token");
        let recorder = RecordingHttp::new(Arc::new(FakeHttp::default()), dir.clone(), keep);
        let mut request = get("https://api.example.test/secure?sig=abc123&page=1");
        request.token = Some(Token::new("SECRET".into()));
        request
            .headers
            .push(("Authorization".into(), "OAuth SECRET".into()));
        recorder.request(Priority::Metadata, request, None).unwrap();

        let texts = file_texts(&dir);
        assert_eq!(texts.len(), 1);
        for text in texts {
            assert!(!text.contains("SECRET"), "{text}");
            assert!(!text.contains("abc123"), "{text}");
            assert!(!text.contains("sig="), "{text}");
        }
    }

    #[test]
    fn replay_reports_a_missing_recording() {
        let dir = temp_dir("missing");
        fs::create_dir_all(&dir).unwrap();
        let replay = ReplayHttp::open(&dir).unwrap();
        let error = replay
            .request(
                Priority::Metadata,
                get("https://api.example.test/nowhere?x=1"),
                None,
            )
            .err()
            .expect("an empty recording must not answer");
        let text = error.to_string();
        assert!(text.contains("нет записи для"), "{text}");
        assert!(text.contains("/nowhere"), "{text}");
    }

    #[test]
    fn scrub_rewrites_path_and_body_before_writing() {
        fn scrub(path: &mut String, body: &mut Value) {
            *path = path.replace("/users/42/", "/users/1/");
            body["result"]["call"] = Value::from(0);
        }
        let dir = temp_dir("scrub");
        let recorder = RecordingHttp::new(Arc::new(FakeHttp::default()), dir.clone(), scrub);
        recorder
            .request(
                Priority::Metadata,
                get("https://api.example.test/users/42/likes"),
                None,
            )
            .unwrap();

        let replay = ReplayHttp::open(&dir).unwrap();
        let replayed = replay
            .request(
                Priority::Metadata,
                get("https://api.example.test/users/1/likes"),
                None,
            )
            .unwrap();
        let json: Value = serde_json::from_slice(&replayed.bytes).unwrap();
        assert_eq!(json["result"]["call"], 0);
    }

    #[test]
    fn repeated_requests_replay_in_recorded_order() {
        let dir = temp_dir("order");
        let recorder = RecordingHttp::new(Arc::new(FakeHttp::default()), dir.clone(), keep);
        for _ in 0..2 {
            recorder
                .request(
                    Priority::Metadata,
                    get("https://api.example.test/items"),
                    None,
                )
                .unwrap();
        }

        let replay = ReplayHttp::open(&dir).unwrap();
        let mut calls = vec![];
        for _ in 0..3 {
            let response = replay
                .request(
                    Priority::Metadata,
                    get("https://api.example.test/items"),
                    None,
                )
                .unwrap();
            let json: Value = serde_json::from_slice(&response.bytes).unwrap();
            calls.push(json["result"]["call"].as_u64().unwrap());
        }
        // The last recorded answer repeats once the list is used up.
        assert_eq!(calls, [1, 2, 2]);
    }
}

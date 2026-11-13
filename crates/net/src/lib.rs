// SPDX-License-Identifier: GPL-3.0-or-later
//! Two blocking workers, priority/FIFO scheduling and bounded HTTP responses.
use eagle_core::{SignedUrl, Token};
pub mod growing;
pub mod policy;
pub mod system_network;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Copy)]
pub enum Priority {
    Playback = 0,
    Preload = 1,
    Metadata = 2,
    Background = 3,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ETag или размер файла изменился; повторите воспроизведение")]
    Changed,
    #[error("{0}")]
    Source(&'static str),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("ошибка сети или таймаут")]
    Network,
    #[error("ответ сервера слишком большой")]
    TooLarge,
    #[error("сетевой агент остановлен")]
    Stopped,
    #[error("очередь сети заполнена")]
    QueueFull,
}
pub struct Request {
    pub url: SignedUrl,
    pub method: &'static str,
    pub headers: Vec<(String, String)>,
    pub token: Option<Token>,
    pub form: Vec<(String, String)>,
    pub range: Option<(u64, u64)>,
    pub limit: usize,
}
impl Request {
    pub fn get(url: SignedUrl) -> Self {
        Self {
            url,
            method: "GET",
            headers: vec![],
            token: None,
            form: vec![],
            range: None,
            limit: 16 * 1024 * 1024,
        }
    }
}
pub struct ResponseHead {
    pub status: u16,
    pub content_range: Option<String>,
    pub etag: Option<String>,
    pub content_length: Option<u64>,
}
/// Streaming body consumers run on a network worker, never the audio thread.
pub trait BodySink: Send + Sync {
    fn begin(&self, head: &ResponseHead) -> Result<(), Error>;
    fn write(&self, bytes: &[u8]) -> Result<(), Error>;
    fn limit(&self) -> u64;
}
pub struct Response {
    pub status: u16,
    pub bytes: Vec<u8>,
    pub content_range: Option<String>,
    pub etag: Option<String>,
}
type Job = Box<dyn FnOnce(&ureq::Agent) + Send>;
#[derive(Default)]
struct State {
    queues: [VecDeque<Job>; 4],
    stopping: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}
#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    response_bytes: AtomicU64,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub requests: u64,
    pub response_bytes: u64,
}
pub struct Client {
    counters: Arc<Counters>,
    shared: Arc<Shared>,
    workers: Vec<thread::JoinHandle<()>>,
}
impl Client {
    pub fn new() -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let mut client = Self {
            counters: Arc::new(Counters::default()),
            shared,
            workers: vec![],
        };
        for i in 0..2 {
            let shared = client.shared.clone();
            client
                .workers
                .push(
                    thread::Builder::new()
                        .name(format!("eg-net-{i}"))
                        .spawn(move || {
                            let agent = ureq::AgentBuilder::new()
                                .timeout_connect(Duration::from_secs(10))
                                .timeout_read(Duration::from_secs(20))
                                .timeout_write(Duration::from_secs(20))
                                .redirects(0)
                                .build();
                            loop {
                                let job = {
                                    let mut s =
                                        shared.state.lock().unwrap_or_else(|p| p.into_inner());
                                    loop {
                                        if let Some(job) =
                                            s.queues.iter_mut().find_map(VecDeque::pop_front)
                                        {
                                            break Some(job);
                                        }
                                        if s.stopping {
                                            break None;
                                        }
                                        s = shared.ready.wait(s).unwrap_or_else(|p| p.into_inner());
                                    }
                                };
                                match job {
                                    Some(job) => job(&agent),
                                    None => break,
                                }
                            }
                        })?,
                );
        }
        Ok(client)
    }
    pub fn stats(&self) -> Stats {
        Stats {
            requests: self.counters.requests.load(Ordering::Relaxed),
            response_bytes: self.counters.response_bytes.load(Ordering::Relaxed),
        }
    }
    pub fn request(
        &self,
        priority: Priority,
        request: Request,
        refresh: Option<&(dyn Fn() -> Result<Token, Error> + Sync)>,
    ) -> Result<Response, Error> {
        self.perform(priority, request, refresh, None)
    }
    pub fn request_into(
        &self,
        priority: Priority,
        request: Request,
        sink: Arc<dyn BodySink>,
    ) -> Result<Response, Error> {
        self.perform(priority, request, None, Some(sink))
    }
    fn perform(
        &self,
        priority: Priority,
        mut request: Request,
        refresh: Option<&(dyn Fn() -> Result<Token, Error> + Sync)>,
        sink: Option<Arc<dyn BodySink>>,
    ) -> Result<Response, Error> {
        for attempt in 0..4 {
            // Refresh happens outside workers to avoid deadlock if the hook uses this client.
            let (tx, rx) = mpsc::sync_channel(1);
            let url = request.url.clone();
            let token = request.token.clone();
            let headers = request.headers.clone();
            let form = request.form.clone();
            let method = request.method;
            let range = request.range;
            let limit = request.limit;
            let counters = self.counters.clone();
            let sink = sink.clone();
            {
                let mut s = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
                if s.stopping {
                    return Err(Error::Stopped);
                }
                if s.queues.iter().map(VecDeque::len).sum::<usize>() >= 256 {
                    return Err(Error::QueueFull);
                }
                s.queues[priority as usize].push_back(Box::new(move |agent| {
                    let result = (|| {
                        let mut current =
                            url::Url::parse(url.expose()).map_err(|_| Error::Network)?;
                        if !matches!(current.scheme(), "http" | "https")
                            || !current.username().is_empty()
                            || current.password().is_some()
                        {
                            return Err(Error::Source(
                                "ожидается HTTP(S)-URL без встроенного пароля",
                            ));
                        }
                        for redirect in 0..=5 {
                            counters.requests.fetch_add(1, Ordering::Relaxed);
                            let mut req = agent
                                .request(method, current.as_str())
                                .set("Accept-Encoding", "identity");
                            for (k, v) in &headers {
                                req = req.set(k, v);
                            }
                            if let Some(token) = &token {
                                req =
                                    req.set("Authorization", &format!("OAuth {}", token.expose()));
                            }
                            if let Some((start, end)) = range {
                                req = req.set("Range", &format!("bytes={start}-{end}"));
                            }
                            let response = if method == "POST" {
                                req.send_form(
                                    &form
                                        .iter()
                                        .map(|(k, v)| (k.as_str(), v.as_str()))
                                        .collect::<Vec<_>>(),
                                )
                            } else {
                                req.call()
                            };
                            let response = response.map_err(|e| match e {
                                ureq::Error::Status(s, _) => Error::Status(s),
                                _ => Error::Network,
                            })?;
                            let status = response.status();
                            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                                // Only credential-free GETs follow CDN redirects; never downgrade TLS.
                                if redirect == 5
                                    || method != "GET"
                                    || token.is_some()
                                    || headers.iter().any(|(k, _)| {
                                        k.eq_ignore_ascii_case("authorization")
                                            || k.eq_ignore_ascii_case("cookie")
                                    })
                                {
                                    return Err(Error::Status(status));
                                }
                                let next = current
                                    .join(response.header("Location").ok_or(Error::Status(status))?)
                                    .map_err(|_| Error::Status(status))?;
                                if !matches!(next.scheme(), "http" | "https")
                                    || (current.scheme() == "https" && next.scheme() != "https")
                                    || !next.username().is_empty()
                                    || next.password().is_some()
                                {
                                    return Err(Error::Status(status));
                                }
                                current = next;
                                continue;
                            }

                            let content_range = response.header("Content-Range").map(str::to_owned);
                            let etag = response.header("ETag").map(str::to_owned);
                            if let Some(sink) = &sink {
                                sink.begin(&ResponseHead {
                                    status,
                                    content_range: content_range.clone(),
                                    etag: etag.clone(),
                                    content_length: response
                                        .header("Content-Length")
                                        .and_then(|s| s.parse().ok()),
                                })?;
                                let max = if status == 206 {
                                    limit as u64
                                } else {
                                    sink.limit()
                                };
                                let mut reader = response.into_reader();
                                let mut buffer = [0u8; 64 * 1024];
                                let mut total = 0u64;
                                loop {
                                    let n = reader.read(&mut buffer).map_err(|_| Error::Network)?;
                                    counters
                                        .response_bytes
                                        .fetch_add(n as u64, Ordering::Relaxed);
                                    if n == 0 {
                                        break;
                                    }
                                    total += n as u64;
                                    if total > max {
                                        return Err(Error::TooLarge);
                                    }
                                    sink.write(&buffer[..n])?;
                                }
                                return Ok(Response {
                                    status,
                                    bytes: Vec::new(),
                                    content_range,
                                    etag,
                                });
                            }
                            let mut bytes = Vec::new();
                            let read_result = response
                                .into_reader()
                                .take(limit as u64 + 1)
                                .read_to_end(&mut bytes);
                            counters
                                .response_bytes
                                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            read_result.map_err(|_| Error::Network)?;
                            if bytes.len() > limit {
                                return Err(Error::TooLarge);
                            }
                            return Ok(Response {
                                status,
                                bytes,
                                content_range,
                                etag,
                            });
                        }
                        Err(Error::Network)
                    })();
                    let _ = tx.send(result);
                }));
                self.shared.ready.notify_one();
            }
            let result = rx.recv().map_err(|_| Error::Stopped)?;
            if attempt < 3 {
                if matches!(result, Err(Error::Status(401)))
                    && let Some(refresh) = refresh
                {
                    request.token = Some(refresh()?);
                    continue;
                }
                if matches!(result, Err(Error::Network | Error::Status(429 | 500..=599)))
                    && method == "GET"
                {
                    let jitter = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_millis()
                        % 100;
                    thread::sleep(Duration::from_millis(
                        200 * (1 << attempt) + u64::from(jitter),
                    ));
                    continue;
                }
            }
            return result;
        }
        Err(Error::Network)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopping = true;
        self.shared.ready.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
    };
    #[test]
    fn priority_is_playback_then_preload_then_metadata_then_background_and_fifo() {
        let mut state = State::default();
        let (tx, rx) = mpsc::channel();
        for (priority, value) in [(3, 4), (2, 3), (1, 2), (0, 0), (0, 1)] {
            let tx = tx.clone();
            state.queues[priority].push_back(Box::new(move |_| {
                tx.send(value).unwrap();
            }));
        }
        let agent = ureq::AgentBuilder::new().build();
        while let Some(job) = state.queues.iter_mut().find_map(VecDeque::pop_front) {
            job(&agent);
        }
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
    }
    #[test]
    fn authenticated_request_does_not_follow_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).unwrap();
            socket.write_all(b"HTTP/1.1 308 Permanent Redirect\r\nLocation: https://example.org/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let client = Client::new().unwrap();
        let mut request = Request::get(SignedUrl::new(format!("http://{addr}/")));
        request.token = Some(Token::new("fixture".into()));
        assert!(matches!(
            client.request(Priority::Metadata, request, None),
            Err(Error::Status(308))
        ));
        assert_eq!(client.stats().requests, 1);
        server.join().unwrap();
    }
    #[test]
    fn retries_get_and_refreshes_token_without_leaking_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for i in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut buf = [0; 1024];
                let n = socket.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                if i == 2 {
                    assert!(request.contains("OAuth refreshed"));
                }
                let response = match i {
                    0 => {
                        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    1 => {
                        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                };
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        let client = Client::new().unwrap();
        let called = AtomicUsize::new(0);
        let hook = || {
            called.fetch_add(1, Ordering::Relaxed);
            Ok(Token::new("refreshed".into()))
        };
        let result = client
            .request(
                Priority::Metadata,
                Request::get(SignedUrl::new(format!("http://{addr}/?token=hidden"))),
                Some(&hook),
            )
            .unwrap();
        assert_eq!(result.bytes, b"ok");
        assert_eq!(called.load(Ordering::Relaxed), 1);
        server.join().unwrap();
    }
}

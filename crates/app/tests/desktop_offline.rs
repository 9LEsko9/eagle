// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises downloads through local HTTP, restart, and offline storage.
use eagle_app::{
    fixture,
    session::{Command, Session},
};
use eagle_core::*;
use eagle_store::{Cache, DownloadState, RangeMap, Store};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

struct TestLogger;
impl log::Log for TestLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{}", record.args());
            let _ = record.key_values().visit(&mut SafeFields);
        }
    }
    fn flush(&self) {}
}
struct SafeFields;
impl<'kvs> log::kv::VisitSource<'kvs> for SafeFields {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        if key.as_str() == "kind" || key.as_str() == "reason" {
            eprintln!("download failure kind: {value}");
        }
        Ok(())
    }
}
static LOGGER: TestLogger = TestLogger;

struct FixtureRoot(std::path::PathBuf);
impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct ServerGuard {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HttpFixture {
    base: Arc<dyn Provider>,
    url: String,
}
impl Provider for HttpFixture {
    fn info(&self) -> &ProviderInfo {
        self.base.info()
    }
    fn login(&self, token: Token) -> Result<Account, ProviderError> {
        self.base.login(token)
    }
    fn account(&self) -> Result<Account, ProviderError> {
        self.base.account()
    }
    fn search(&self, q: &str, p: u32) -> Result<Page<Track>, ProviderError> {
        self.base.search(q, p)
    }
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        let mut track = self.base.track(id)?;
        track.cover = Some("https://fixture.invalid/art".into());
        Ok(track)
    }
    fn album(&self, id: &TrackId) -> Result<Album, ProviderError> {
        self.base.album(id)
    }
    fn playlist(&self, id: &TrackId) -> Result<Playlist, ProviderError> {
        self.base.playlist(id)
    }
    fn playlists(&self, c: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        self.base.playlists(c)
    }
    fn likes(&self, c: Option<&str>) -> Result<Page<Track>, ProviderError> {
        let mut page = self.base.likes(c)?;
        for track in &mut page.items {
            track.cover = Some("https://fixture.invalid/art".into());
        }
        Ok(page)
    }
    fn set_liked(&self, id: &TrackId, b: bool) -> Result<(), ProviderError> {
        self.base.set_liked(id, b)
    }
    fn set_disliked(&self, id: &TrackId, b: bool) -> Result<(), ProviderError> {
        self.base.set_disliked(id, b)
    }
    fn resolve_stream(&self, id: &TrackId, _: Quality) -> Result<Stream, ProviderError> {
        Ok(Stream {
            url: SignedUrl::new(format!("{}/{}", self.url, id.local())),
            bitrate_kbps: Some(128),
            codec: "mp3".into(),
            expires_at: SystemTime::now() + Duration::from_secs(300),
            encryption: Encryption::None,
        })
    }
}

#[test]
fn download_child() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    let (Ok(root), Ok(url)) = (
        std::env::var("EAGLE_TEST_ROOT"),
        std::env::var("EAGLE_TEST_URL"),
    ) else {
        return;
    };
    let base = fixture::provider();
    let tracks = base.likes(None).unwrap().items;
    let provider = Arc::new(HttpFixture { base, url });
    let session = Session::start_with_providers(root.into(), vec![provider], |_| {}).unwrap();
    for track in tracks {
        session
            .sender()
            .send(Command::Save {
                id: track.id,
                quality: Quality::High,
            })
            .unwrap();
    }
    loop {
        thread::park();
    }
}

#[test]
fn downloads_restart_then_survive_offline_reopen() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    let root = std::env::temp_dir().join(format!(
        "eagle-http-offline-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root_guard = FixtureRoot(root.clone());
    let store = Store::open(&root).unwrap();
    store
        .put_setting("account:fixture", "fixture".into())
        .unwrap();
    let base = fixture::provider();
    let tracks = base.likes(None).unwrap().items;
    assert_eq!(tracks.len(), 100);
    let covers = root.join("covers");
    std::fs::create_dir_all(&covers).unwrap();
    for track in &tracks {
        store.put_track(track).unwrap();
        let name = track.id.to_string().replace(':', "_");
        let colors = eagle_palette::placeholder(
            &covers.join(format!("{name}-64.png")),
            &covers.join(format!("{name}-600.png")),
        )
        .unwrap();
        store
            .put_json(
                "colors",
                &track.id.to_string(),
                serde_json::to_string(&colors).unwrap(),
            )
            .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/audio", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let resumed = Arc::new(Mutex::new(Vec::<(String, usize)>::new()));
    let partial_phase = Arc::new(AtomicBool::new(true));
    let (server_stop, server_requests, server_resumed, server_partial) = (
        stop.clone(),
        requests.clone(),
        resumed.clone(),
        partial_phase.clone(),
    );
    let server = thread::spawn(move || {
        while !server_stop.load(Ordering::Acquire) {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 4096 {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                if !request.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let request = String::from_utf8_lossy(&request);
                let start = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("range") {
                            return None;
                        }
                        value
                            .trim()
                            .strip_prefix("bytes=")?
                            .split_once('-')?
                            .0
                            .parse::<usize>()
                            .ok()
                    })
                    .expect("HTTP fixture request must contain a valid Range header");
                server_requests.fetch_add(1, Ordering::Relaxed);
                if start > 0 {
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("");
                    server_resumed
                        .lock()
                        .unwrap()
                        .push((path.to_owned(), start));
                }
                let body = include_bytes!("../../engine/tests/fixtures/long-vbr.mp3");
                if start >= body.len() {
                    continue;
                }
                let end = if server_partial.load(Ordering::Acquire) {
                    (start + 1024).min(body.len())
                } else {
                    body.len()
                };
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
                    end - start,
                    start,
                    end - 1,
                    body.len()
                );
                thread::sleep(Duration::from_millis(1));
                if stream.write_all(response.as_bytes()).is_ok() {
                    let _ = stream.write_all(&body[start..end]);
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    let mut server = ServerGuard {
        stop,
        thread: Some(server),
    };
    let mut child = ChildGuard(
        std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("download_child")
            .arg("--nocapture")
            .env("EAGLE_TEST_ROOT", &root)
            .env("EAGLE_TEST_URL", &url)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let jobs = store.downloads("fixture").unwrap();
        if jobs.len() == 100
            && jobs.iter().any(|job| {
                let path = root
                    .join("audio/downloads")
                    .join(job.id.to_string())
                    .join("ranges.json");
                std::fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<RangeMap>(&bytes).ok())
                    .is_some_and(|map| !map.ranges.is_empty() && !map.complete())
            })
        {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "download child exited early"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let jobs = store.downloads("fixture").unwrap();
    assert_eq!(jobs.len(), 100);
    assert!(jobs.iter().any(|job| job.state == DownloadState::Running));
    let partial = jobs
        .iter()
        .find(|job| {
            let path = root
                .join("audio/downloads")
                .join(job.id.to_string())
                .join("ranges.json");
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RangeMap>(&bytes).ok())
                .is_some_and(|map| !map.ranges.is_empty() && !map.complete())
        })
        .expect("no partial range persisted before crash")
        .track
        .local()
        .to_owned();
    assert!(requests.load(Ordering::Relaxed) > 0);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    partial_phase.store(false, Ordering::Release);
    resumed.lock().unwrap().clear();
    let provider = Arc::new(HttpFixture { base, url });
    let session = Session::start_with_providers(root.clone(), vec![provider], |_| {}).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let jobs = store.downloads("fixture").unwrap();
        if jobs.iter().all(|job| job.state == DownloadState::Saved)
            || jobs.iter().any(|job| job.state == DownloadState::Failed)
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let jobs = store.downloads("fixture").unwrap();
    assert!(
        jobs.iter().all(|job| job.state == DownloadState::Saved),
        "unfinished: {:?}",
        jobs.iter()
            .filter(|job| job.state != DownloadState::Saved)
            .collect::<Vec<_>>()
    );
    assert!(requests.load(Ordering::Relaxed) >= 100);
    assert!(
        resumed
            .lock()
            .unwrap()
            .iter()
            .any(|(path, start)| path.ends_with(&format!("/{partial}")) && *start > 0),
        "partial download was not resumed with Range"
    );
    drop(session);
    server.stop.store(true, Ordering::Release);
    server.thread.take().unwrap().join().unwrap();
    for track in &tracks {
        let json: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT json FROM tracks WHERE id=?1",
                [track.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let saved: Track = serde_json::from_str(&json).unwrap();
        assert_eq!(saved.cover.as_deref(), Some("https://fixture.invalid/art"));
    }
    let offline = Session::start_with_providers(
        root.clone(),
        vec![Arc::new(HttpFixture {
            base: fixture::provider(),
            url: String::new(),
        })],
        |_| {},
    )
    .unwrap();
    assert_eq!(
        store
            .downloads("fixture")
            .unwrap()
            .iter()
            .filter(|job| job.state == DownloadState::Saved)
            .count(),
        100
    );
    let cache = Cache::open(&root.join("audio"), 256 * 1024 * 1024, &store).unwrap();
    for track in &tracks {
        let json: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT json FROM tracks WHERE id=?1",
                [track.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let saved: Track = serde_json::from_str(&json).unwrap();
        assert_eq!(saved.cover.as_deref(), Some("https://fixture.invalid/art"));
        let name = track.id.to_string().replace(':', "_");
        for size in [64, 600] {
            let bytes = std::fs::read(covers.join(format!("{name}-{size}.png"))).unwrap();
            let cover = eagle_palette::decode_cover(&bytes, size).unwrap();
            assert!(cover.width > 0 && cover.height > 0);
        }
        let path = cache
            .lookup(&store, &track.id, Quality::High)
            .unwrap()
            .unwrap();
        let decoded = eagle_engine::inspect(&eagle_engine::Source::file(path), None).unwrap();
        assert!(decoded.frames > 0);
        assert_eq!(decoded.damaged_packets, 0);
    }
    drop(cache);
    drop(offline);
    drop(store);
    drop(root_guard);
}

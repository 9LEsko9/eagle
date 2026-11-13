// SPDX-License-Identifier: GPL-3.0-or-later
use crossterm::{
    event::{self, Event as TerminalEvent, KeyCode, KeyEventKind},
    terminal,
};
use eagle_core::{Encryption, Provider, Quality, SignedUrl, TrackId};
use eagle_engine::{Config, Engine, Event, Interrupt, MediaSource, PositionAnchor, Source, State};
use eagle_net::{
    Client, Priority,
    growing::{Download, GrowingReader, Snapshot},
};
use eagle_store::{Cache, CacheLease, RangeMap, Store};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const LIMIT: u64 = 512 * 1024 * 1024;
#[derive(Default)]
struct Options {
    items: Vec<String>,
    standard: bool,
    save: bool,
    download_only: bool,
    buffer: u32,
    stats: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let mut options = Options {
        buffer: 2,
        ..Default::default()
    };
    let mut args = args.iter();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if positional {
            options.items.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional = true,
            "--standard" => options.standard = true,
            "--save" => options.save = true,
            "--download-only" => options.download_only = true,
            "--stats" => options.stats = true,
            "--buffer-seconds" => {
                options.buffer = args.next().ok_or("укажите размер буфера")?.parse()?;
                if !(2..=20).contains(&options.buffer) {
                    return Err("буфер должен быть от 2 до 20 секунд".into());
                }
            }
            a if a.starts_with("--") => return Err("неизвестный флаг воспроизведения".into()),
            _ => options.items.push(arg.clone()),
        }
    }
    if options.items.is_empty() {
        return Err("укажите файл, HTTP(S)-URL или provider:id".into());
    }
    Ok(options)
}
struct HttpMedia(GrowingReader);
struct LeasedMedia {
    reader: Box<dyn MediaSource>,
    _download: Option<Arc<Download>>,
    _lease: CacheLease,
}
impl Read for LeasedMedia {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.reader.read(data)
    }
}
impl Seek for LeasedMedia {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader.seek(pos)
    }
}
impl MediaSource for LeasedMedia {
    fn is_seekable(&self) -> bool {
        self.reader.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.reader.byte_len()
    }
}
impl Read for HttpMedia {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.0.read(data)
    }
}
impl Seek for HttpMedia {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}
impl MediaSource for HttpMedia {
    fn is_seekable(&self) -> bool {
        self.0.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.0.byte_len()
    }
}
fn cache_hit(
    store: &Store,
    cache: &Cache,
    id: &TrackId,
    quality: Quality,
) -> Result<Option<(PathBuf, Quality)>> {
    let other = if quality == Quality::High {
        Quality::Standard
    } else {
        Quality::High
    };
    for q in [quality, other] {
        if cache.map(id, q)?.is_some_and(|m| m.pinned && m.complete())
            && let Some(path) = cache.lookup(store, id, q)?
        {
            return Ok(Some((path, q)));
        }
    }
    Ok(cache.lookup(store, id, quality)?.map(|p| (p, quality)))
}
pub(crate) fn streamed(
    store: Arc<Store>,
    cache: Arc<Cache>,
    net: Arc<Client>,
    id: TrackId,
    quality: Quality,
    resolve: Arc<eagle_net::growing::Resolve>,
) -> Result<Source> {
    streamed_with_bitrate(store, cache, net, id, quality, resolve, Arc::new(|_| {}))
}
pub(crate) fn streamed_with_bitrate(
    store: Arc<Store>,
    cache: Arc<Cache>,
    net: Arc<Client>,
    id: TrackId,
    quality: Quality,
    resolve: Arc<eagle_net::growing::Resolve>,
    on_bitrate: Arc<dyn Fn(Option<u32>) + Send + Sync>,
) -> Result<Source> {
    let notify = on_bitrate.clone();
    let resolve = Arc::new(move || {
        let location = resolve()?;
        notify(location.bitrate_kbps);
        Ok(location)
    });
    let _ = cache.map(&id, quality)?;
    let slot: Mutex<std::sync::Weak<Download>> = Mutex::default();
    Ok(Source::new(move |interrupt| {
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        let high = cache.lease(&id, Quality::High);
        let standard = cache.lease(&id, Quality::Standard);
        if let Some((path, actual)) =
            cache_hit(&store, &cache, &id, quality).map_err(|e| e.to_string())?
        {
            on_bitrate(cache.map(&id, actual)?.and_then(|m| m.bitrate_kbps));
            let lease = if actual == Quality::High {
                high
            } else {
                standard
            };
            return Ok(Box::new(LeasedMedia {
                reader: Box::new(std::fs::File::open(path)?),
                _download: None,
                _lease: lease,
            }));
        }
        let lease = if quality == Quality::High {
            high
        } else {
            standard
        };
        let mut download = slot.upgrade();
        let priority = if interrupt.preload {
            Priority::Preload
        } else {
            Priority::Playback
        };
        if download.as_ref().is_some_and(|d| d.failed()) {
            download = None;
        }
        if download.is_none() {
            let mut initial = cache.map(&id, quality)?;
            if initial.as_ref().is_some_and(|m| {
                !m.etag
                    .as_deref()
                    .is_some_and(eagle_net::growing::strong_etag)
            }) {
                cache.discard(&store, &id, quality)?;
                initial = None;
            }
            let initial = initial.map_or_else(Snapshot::default, |m| Snapshot {
                bitrate_kbps: m.bitrate_kbps,
                total: Some(m.total),
                ranges: m.ranges,
                etag: m.etag,
            });
            let store = store.clone();
            let cache = cache.clone();
            let id = id.clone();
            let path = cache.path(&id, quality);
            let publish: Arc<eagle_net::growing::Publish> = Arc::new(move |snapshot| {
                let Some(total) = snapshot.total else {
                    return Ok(());
                };
                let map = RangeMap {
                    bitrate_kbps: snapshot.bitrate_kbps,
                    total,
                    ranges: snapshot.ranges.clone(),
                    etag: snapshot.etag.clone(),
                    pinned: false,
                };
                cache
                    .publish(&store, &id, quality, &map)
                    .map_err(|_| eagle_net::Error::Source("не удалось обновить карту аудиокэша"))
            });
            download = Some(Arc::new(Download::start(
                &path,
                net.clone(),
                resolve.clone(),
                initial.clone(),
                priority,
                Some(publish),
            )?));
        }
        let download = download.ok_or("загрузчик не создан")?;
        *slot = Arc::downgrade(&download);
        Ok(Box::new(LeasedMedia {
            reader: Box::new(HttpMedia(
                download
                    .reader(interrupt.generation, interrupt.expected, priority)?
                    .with_seek_guard(interrupt.seeking),
            )),
            _download: Some(download),
            _lease: lease,
        }))
    }))
}
fn complete(source: &Source) -> Result<()> {
    // This path deliberately bypasses decoding and audio output.
    let interrupt = Interrupt {
        generation: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        expected: 0,
        preload: false,
        seeking: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let mut reader = source.open_reader(interrupt).map_err(|e| e.to_string())?;
    io::copy(&mut reader, &mut io::sink())?;
    Ok(())
}
pub fn run(args: &[String], root: &Path, net: Arc<Client>) -> Result<()> {
    let options = options(args)?;
    let quality = if options.standard {
        Quality::Standard
    } else {
        Quality::High
    };
    let store = Arc::new(Store::open(root)?);
    let cache = Arc::new(Cache::open(&root.join("audio"), LIMIT * 4, &store)?);
    let provider = Arc::new(eagle_yandex::Yandex::new(net.clone()));
    let mut sources = Vec::new();
    let mut prepared = HashMap::<String, Source>::new();
    for item in &options.items {
        if let Some(source) = prepared.get(item) {
            sources.push(source.clone());
            continue;
        }
        let source = if Path::new(item).is_file() {
            Source::file(item)
        } else {
            let is_url = item.starts_with("http://") || item.starts_with("https://");
            let id: TrackId = if is_url {
                format!("http:{:x}", Sha256::digest(item.as_bytes())).parse()?
            } else {
                item.parse()?
            };
            {
                let resolve: Arc<eagle_net::growing::Resolve> = if is_url {
                    let url = SignedUrl::new(item.clone());
                    Arc::new(move || Ok(url.clone().into()))
                } else {
                    if id.provider() != "yandex" {
                        return Err("сетевой провайдер ещё не реализован".into());
                    }
                    let provider = provider.clone();
                    let id = id.clone();
                    let store = store.clone();
                    let root = root.to_owned();
                    let metadata_net = net.clone();
                    Arc::new(move || {
                        let stream = provider.resolve_stream(&id, quality).map_err(|_| {
                            eagle_net::Error::Source(
                                "не удалось получить аудиоссылку; проверьте вход и доступ к треку",
                            )
                        })?;
                        if !matches!(stream.encryption, Encryption::None) {
                            return Err(eagle_net::Error::Source(
                                "шифрованные потоки не поддерживаются",
                            ));
                        }
                        // Metadata failures do not make an available audio stream unplayable.
                        if let Ok(track) = provider.track(&id) {
                            let _ = crate::music::metadata(&store, &metadata_net, &root, &track);
                        }
                        Ok(eagle_net::growing::Location {
                            bitrate_kbps: stream.bitrate_kbps,
                            url: stream.url,
                            expires_at: Some(stream.expires_at),
                        })
                    })
                };
                log::info!(target: "eagle_app", provider=id.provider(), track_id=id.local(); "playback");
                let source = streamed(
                    store.clone(),
                    cache.clone(),
                    net.clone(),
                    id.clone(),
                    quality,
                    resolve,
                )?;
                if options.download_only || options.save {
                    complete(&source)?;
                    if options.save {
                        let (_, actual) =
                            cache_hit(&store, &cache, &id, quality)?.ok_or("кэш не заполнен")?;
                        cache.pin(&store, &id, actual, true)?;
                    }
                    source
                } else {
                    source
                }
            }
        };
        prepared.insert(item.clone(), source.clone());
        sources.push(source);
    }
    print_network(&net);
    if options.download_only {
        return Ok(());
    }
    let engine = Engine::new(Config {
        buffer_seconds: options.buffer,
    })?;
    for source in sources {
        engine.enqueue(source)?;
    }
    let result = interact(&engine, &net, options.stats);
    drop(engine);
    print_network(&net);
    result
}
fn print_network(net: &Client) {
    let stats = net.stats();
    println!(
        "Сеть: {} запросов, {} байт ответов",
        stats.requests, stats.response_bytes
    );
}
struct TerminalGuard(bool);
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let enabled = io::stdin().is_terminal() && io::stdout().is_terminal();
        if enabled {
            terminal::enable_raw_mode()?;
        }
        Ok(Self(enabled))
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = terminal::disable_raw_mode();
            eprintln!();
        }
    }
}
fn position(anchor: &Option<PositionAnchor>) -> Duration {
    anchor.as_ref().map_or(Duration::ZERO, |a| {
        if a.state == State::Playing {
            let now = Instant::now();
            if now >= a.audible_at {
                a.position + now.duration_since(a.audible_at)
            } else {
                a.position.saturating_sub(a.audible_at.duration_since(now))
            }
        } else {
            a.position
        }
    })
}
fn interact(engine: &Engine, net: &Client, print_stats: bool) -> Result<()> {
    let terminal = TerminalGuard::enter()?;
    let mut paused = false;
    let mut volume = 1f32;
    let mut anchor = None;
    let mut error = None;
    if terminal.0 {
        eprintln!("Space: пауза  ←/→: ±10 с  n: следующий  +/-: громкость  q: выход\r");
    }
    loop {
        let event = if terminal.0 {
            engine.try_recv().ok()
        } else {
            engine.recv_timeout(Duration::from_secs(1)).ok()
        };
        if let Some(event) = event {
            match event {
                Event::Position(a) => {
                    anchor = Some(a);
                }
                Event::Error { message, .. } => {
                    eprintln!("Аудио: {message}\r");
                    error = Some(message);
                }
                Event::Statistics(s) => {
                    if print_stats {
                        let network = net.stats();
                        println!(
                            "Статистика: consumed={} underruns={} callbacks={} requests={} bytes={} rate={}",
                            s.consumed_frames,
                            s.underruns,
                            s.callbacks,
                            network.requests,
                            network.response_bytes,
                            s.output_rate
                        );
                    }
                    if engine.finished() {
                        println!(
                            "Аудио: {} кадров, {} underruns, {} повреждённых пакетов",
                            s.consumed_frames, s.underruns, s.damaged_packets
                        );
                    }
                }
                _ => {}
            }
        }
        if engine.finished() {
            while let Ok(event) = engine.try_recv() {
                if let Event::Error { message, .. } = event {
                    error = Some(message);
                }
            }
            break;
        }
        if terminal.0
            && event::poll(Duration::from_millis(50))?
            && let TerminalEvent::Key(key) = event::read()?
        {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    engine.stop()?;
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    engine.stop()?;
                    break;
                }
                KeyCode::Char(' ') => {
                    paused = !paused;
                    if paused {
                        engine.pause()?;
                    } else {
                        engine.resume()?;
                    }
                }
                KeyCode::Left => {
                    engine.seek(position(&anchor).saturating_sub(Duration::from_secs(10)))?
                }
                KeyCode::Right => engine.seek(position(&anchor) + Duration::from_secs(10))?,
                KeyCode::Char('n') => engine.next()?,
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    volume = (volume + 0.05).min(1.);
                    engine.volume(volume)?;
                }
                KeyCode::Char('-') => {
                    volume = (volume - 0.05).max(0.);
                    engine.volume(volume)?;
                }
                _ => {}
            }
        }
    }
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_queue_flags_apply_to_the_whole_list_and_paths_can_start_with_dash() {
        let args = [
            "a.flac",
            "--standard",
            "http://localhost/a",
            "yandex:123",
            "--save",
            "--download-only",
            "--",
            "--song.mp3",
        ]
        .map(String::from);
        let options = options(&args).unwrap();
        assert_eq!(
            options.items,
            ["a.flac", "http://localhost/a", "yandex:123", "--song.mp3"]
        );
        assert!(options.standard && options.save && options.download_only);
        assert!(super::options(&["--unknown".into()]).is_err());
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::{
        io::Write,
        net::TcpListener,
        sync::atomic::{AtomicBool, Ordering},
    };
    #[test]
    fn offline_source_reports_selected_file_bitrate_without_resolving() {
        let root =
            std::env::temp_dir().join(format!("eagle-offline-bitrate-{}", std::process::id()));
        let store = Arc::new(Store::open(&root).unwrap());
        let cache = Arc::new(Cache::open(&root.join("audio"), LIMIT, &store).unwrap());
        let id: TrackId = "yandex:1".parse().unwrap();
        let mut map = RangeMap::new(1);
        map.bitrate_kbps = Some(128);
        cache
            .write_chunk(&store, &id, Quality::Standard, &mut map, 0, b"x")
            .unwrap();
        cache.pin(&store, &id, Quality::Standard, true).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let source = streamed_with_bitrate(
            store.clone(),
            cache.clone(),
            Arc::new(Client::new().unwrap()),
            id,
            Quality::High,
            Arc::new(|| panic!("offline file must not resolve")),
            Arc::new(move |bitrate| {
                tx.send(bitrate).unwrap();
            }),
        )
        .unwrap();
        let mut reader = source
            .open_reader(eagle_engine::Interrupt {
                generation: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                expected: 0,
                preload: false,
                seeking: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"x");
        assert_eq!(rx.recv().unwrap(), Some(128));
        drop(reader);
        drop(source);
        drop(cache);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn every_format_decodes_over_http_and_complete_cache_never_resolves_again() {
        let root = std::env::temp_dir().join(format!("eagle-http-formats-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let data_root = fixtures.clone();
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut header = Vec::new();
                let mut b = [0; 1];
                while !header.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut b).unwrap_or(0) == 0 {
                        break;
                    }
                    header.push(b[0]);
                }
                let request = String::from_utf8_lossy(&header);
                let name = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/');
                let sequential = name.starts_with("sequential/");
                let name = name.strip_prefix("sequential/").unwrap_or(name);
                assert!(!name.contains(".."));
                let bytes = std::fs::read(data_root.join(name)).unwrap();
                let range = request
                    .lines()
                    .find_map(|s| {
                        s.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_owned)
                    })
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end = (end.parse::<usize>().unwrap() + 1).min(bytes.len());
                let (start, end) = if sequential {
                    (0, bytes.len())
                } else {
                    (start, end)
                };
                let response = if sequential {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"fixture\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        start,
                        end - 1,
                        bytes.len(),
                        end - start
                    )
                };
                let _ = socket.write_all(response.as_bytes());
                for block in bytes[start..end].chunks(128) {
                    if socket.write_all(block).is_err() {
                        break;
                    }
                    if sequential {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        });
        let store = Arc::new(Store::open(&root).unwrap());
        let cache = Arc::new(Cache::open(&root.join("audio"), LIMIT, &store).unwrap());
        let net = Arc::new(Client::new().unwrap());
        for sequential in [false, true] {
            for rate in [44100, 48000] {
                for channels in [1, 2] {
                    for extension in ["mp3", "aac", "m4a", "flac", "ogg", "opus"] {
                        let name = format!("tone-{rate}-{channels}.{extension}");
                        let id: TrackId =
                            format!("fixture:{rate}_{channels}_{extension}_{sequential}")
                                .parse()
                                .unwrap();
                        let mode = if sequential { "sequential/" } else { "" };
                        let url = SignedUrl::new(format!("http://{address}/{mode}{name}"));
                        let source = streamed(
                            store.clone(),
                            cache.clone(),
                            net.clone(),
                            id.clone(),
                            Quality::High,
                            Arc::new(move || Ok(url.clone().into())),
                        )
                        .unwrap();
                        let actual = eagle_engine::inspect(&source, Some(48000)).unwrap();
                        let expected =
                            eagle_engine::inspect(&Source::file(fixtures.join(&name)), Some(48000))
                                .unwrap();
                        assert_eq!(actual.frames, expected.frames, "{name}");
                        complete(&source).unwrap();
                        let before = net.stats();
                        assert_eq!(
                            eagle_engine::inspect(&source, Some(48000)).unwrap().frames,
                            expected.frames
                        );
                        assert_eq!(net.stats().requests, before.requests);
                        if !sequential && rate == 44100 && channels == 1 && extension == "mp3" {
                            cache.discard(&store, &id, Quality::High).unwrap();
                            assert_eq!(
                                eagle_engine::inspect(&source, Some(48000)).unwrap().frames,
                                expected.frames
                            );
                            complete(&source).unwrap();
                            assert_eq!(net.stats().requests, before.requests + 1);
                        }
                    }
                }
            }
        }
        // Reopen without the HTTP server, retaining cache publication and byte accounting.
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert_eq!(net.stats().requests, 49);
        drop(cache);
        drop(store);
        drop(net);
        let _ = std::fs::remove_dir_all(root);
    }
}

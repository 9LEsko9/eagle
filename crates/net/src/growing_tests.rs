// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use std::{
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, AtomicUsize},
    time::Instant,
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    url: String,
}
impl Fixture {
    fn new(handler: impl Fn(&mut TcpStream, &str) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut header = Vec::new();
                let mut b = [0u8; 1];
                while !header.ends_with(b"\r\n\r\n") && header.len() < 8192 {
                    if socket.read(&mut b).unwrap_or(0) == 0 {
                        break;
                    }
                    header.push(b[0]);
                }
                handler(&mut socket, &String::from_utf8_lossy(&header));
            }
        });
        let path = std::env::temp_dir().join(format!(
            "eagle-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            path,
            stop,
            server: Some(server),
            url,
        }
    }
    fn download(&self) -> Download {
        let url = SignedUrl::new(self.url.clone());
        Download::start(
            &self.path,
            Arc::new(Client::new().unwrap()),
            Arc::new(move || Ok(url.clone().into())),
            Snapshot::default(),
            Priority::Playback,
            None,
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
fn reader(download: &Download) -> GrowingReader {
    download
        .reader(Arc::new(AtomicU32::new(0)), 0, Priority::Playback)
        .unwrap()
}
fn request_range(request: &str) -> (usize, usize) {
    let range = request
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("range: bytes=")
                .map(str::to_owned)
        })
        .unwrap();
    let (a, b) = range.split_once('-').unwrap();
    (a.parse().unwrap(), b.parse().unwrap())
}
#[test]
fn progressive_ranges_and_seek_into_hole_are_byte_exact() {
    let data: Arc<Vec<u8>> = Arc::new((0..6 * 1024 * 1024).map(|n| (n % 251) as u8).collect());
    let bytes = data.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let calls = requests.clone();
    let fixture = Fixture::new(move |socket, request| {
        calls.fetch_add(1, Ordering::Relaxed);
        let (a, b) = request_range(request);
        let end = (b + 1).min(bytes.len());
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"same\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            a,
            end - 1,
            bytes.len(),
            end - a
        );
        let _ = socket.write_all(header.as_bytes());
        for chunk in bytes[a..end].chunks(8192) {
            if socket.write_all(chunk).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let download = fixture.download();
    let mut input = reader(&download);
    let mut first = [0; 73];
    input.read_exact(&mut first).unwrap();
    assert_eq!(&first, &data[..73]);
    assert!(download.snapshot().ranges.last().unwrap().1 < data.len() as u64);
    input.seek(SeekFrom::Start(4 * 1024 * 1024 + 37)).unwrap();
    let mut output = [0; 733];
    input.read_exact(&mut output).unwrap();
    assert_eq!(&output, &data[4 * 1024 * 1024 + 37..4 * 1024 * 1024 + 770]);
    assert!(requests.load(Ordering::Relaxed) >= 2);
    input.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(input.read(&mut first).unwrap(), 0);
}
#[test]
fn sequential_unknown_size_waits_until_confirmed_eof_and_rejects_seek_ahead() {
    let fixture = Fixture::new(|socket, _| {
        let _ = socket.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        for _ in 0..20 {
            let _ = socket.write_all(b"1000\r\n");
            let _ = socket.write_all(&[7u8; 4096]);
            let _ = socket.write_all(b"\r\n");
            thread::sleep(Duration::from_millis(3));
        }
        let _ = socket.write_all(b"0\r\n\r\n");
    });
    let download = fixture.download();
    let mut input = reader(&download);
    let mut first = [0; 20];
    input.read_exact(&mut first).unwrap();
    assert_eq!(input.byte_len(), None);
    assert!(!input.is_seekable());
    assert_eq!(
        input.seek(SeekFrom::Start(1_000_000)).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    let mut rest = Vec::new();
    input.read_to_end(&mut rest).unwrap();
    assert_eq!(rest.len() + 20, 20 * 4096);
    assert!(rest.iter().all(|b| *b == 7));
    assert_eq!(input.byte_len(), Some(20 * 4096));
}
#[test]
fn changed_etag_fails_without_mixing_versions() {
    let fixture = Fixture::new(|socket, request| {
        let (a, b) = request_range(request);
        let end = (b + 1).min(3 * 1024 * 1024);
        let etag = if a == 0 { "one" } else { "two" };
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            a,
            end - 1,
            3 * 1024 * 1024,
            etag,
            end - a
        );
        let _ = socket.write_all(header.as_bytes());
        let _ = socket.write_all(&vec![1; end - a]);
    });
    let download = fixture.download();
    let mut input = reader(&download);
    let mut block = vec![0; CHUNK as usize];
    input.read_exact(&mut block).unwrap();
    let mut next = [0; 10];
    let error = input.read(&mut next).unwrap_err();
    assert!(error.to_string().contains("ETag"));
    assert_eq!(download.snapshot().ranges, [(0, CHUNK)]);
}
#[test]
fn cancellation_wakes_a_blocked_decoder_without_waiting_for_http() {
    let fixture = Fixture::new(|socket, _| {
        let _ =
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n");
        thread::sleep(Duration::from_millis(800));
    });
    let download = fixture.download();
    let epoch = Arc::new(AtomicU32::new(0));
    let mut input = download
        .reader(epoch.clone(), 0, Priority::Playback)
        .unwrap();
    let waiting = thread::spawn(move || input.read(&mut [0; 10]));
    thread::sleep(Duration::from_millis(40));
    let started = Instant::now();
    epoch.store(1, Ordering::Release);
    assert_eq!(
        waiting.join().unwrap().unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(started.elapsed() < Duration::from_millis(300));
    download.cancel();
}
#[test]
fn expired_audio_link_is_resolved_again() {
    for status in [401, 403, 410] {
        let fixture = Fixture::new(move |socket, request| {
            if request.starts_with("GET /expired ") {
                let _ = socket.write_all(
                format!("HTTP/1.1 {status} Expired\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            );
            } else {
                let _ = socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nETag: \"same\"\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest");
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let url = fixture.url.clone();
        let resolve: Arc<Resolve> = Arc::new(move || {
            let first = counter.fetch_add(1, Ordering::Relaxed) == 0;
            Ok(SignedUrl::new(if first {
                url.replace("/audio", "/expired")
            } else {
                url.clone()
            })
            .into())
        });
        let download = Download::start(
            &fixture.path,
            Arc::new(Client::new().unwrap()),
            resolve,
            Snapshot::default(),
            Priority::Playback,
            None,
        )
        .unwrap();
        let mut output = Vec::new();
        reader(&download).read_to_end(&mut output).unwrap();
        assert_eq!(output, b"test");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
#[test]
fn truncated_body_is_an_error_not_eof() {
    let fixture = Fixture::new(|socket, _| {
        let _ = socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-99/100\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort");
    });
    let download = fixture.download();
    let mut output = Vec::new();
    assert!(reader(&download).read_to_end(&mut output).is_err());
}

#[test]
fn deadline_refreshes_location_before_the_next_range() {
    let fixture = Fixture::new(|socket, request| {
        let (start, _) = request_range(request);
        let end = (start + CHUNK as usize).min(CHUNK as usize + 4);
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nETag: \"same\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            start,
            end - 1,
            CHUNK + 4,
            end - start
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&vec![7; end - start]).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let url = fixture.url.clone();
    let download = Download::start(
        &fixture.path,
        Arc::new(Client::new().unwrap()),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(Location {
                bitrate_kbps: Some(320),
                url: SignedUrl::new(url.clone()),
                expires_at: Some(SystemTime::UNIX_EPOCH),
            })
        }),
        Snapshot::default(),
        Priority::Playback,
        None,
    )
    .unwrap();
    let mut output = Vec::new();
    reader(&download).read_to_end(&mut output).unwrap();
    assert_eq!(output.len(), CHUNK as usize + 4);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(download.snapshot().bitrate_kbps, Some(320));
}

#[test]
fn opening_a_preload_reader_does_not_demote_playback() {
    let fixture = Fixture::new(|_, _| {});
    let download = fixture.download();
    let current = reader(&download);
    let preload = download
        .reader(Arc::new(AtomicU32::new(0)), 0, Priority::Preload)
        .unwrap();
    assert!(matches!(current.priority, Priority::Playback));
    assert!(matches!(preload.priority, Priority::Preload));
    assert!(matches!(
        download.shared.state.lock().unwrap().priority,
        Priority::Playback
    ));
}

#[test]
fn weak_or_missing_validator_never_joins_ranges() {
    assert!(strong_etag("\"version\""));
    assert!(strong_etag("\"\""));
    for invalid in ["W/\"version\"", "version", "\"a\"b\"", "\"a\nb\""] {
        assert!(!strong_etag(invalid));
    }
    for tag in ["", "ETag: W/\"weak\"\r\n"] {
        let fixture = Fixture::new(move |socket, request| {
            let (start, _) = request_range(request);
            assert_eq!(start, 0, "unverified second request must not be sent");
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                CHUNK - 1,
                CHUNK + 1,
                tag,
                CHUNK
            );
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(&vec![1; CHUNK as usize]);
        });
        let download = fixture.download();
        let mut r = reader(&download);
        let mut first = vec![0; CHUNK as usize];
        r.read_exact(&mut first).unwrap();
        assert!(r.read(&mut [0; 1]).is_err());
    }
}

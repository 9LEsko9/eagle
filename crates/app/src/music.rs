// SPDX-License-Identifier: GPL-3.0-or-later
use crate::providers::Providers;
use eagle_core::{Encryption, Provider, Quality, SignedUrl, Stream, Token, TrackId};
use eagle_net::{Client, Priority, Request};
use eagle_store::{Cache, RangeMap, Store};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CHUNK: u64 = 2 * 1024 * 1024;
const LIMIT: u64 = 512 * 1024 * 1024;
/// Cache-first boundary: even token retrieval is deferred until a network miss.
pub fn audio(
    store: &Store,
    cache: &Cache,
    net: &Client,
    id: &TrackId,
    q: Quality,
    mut resolve: impl FnMut() -> Result<Stream>,
) -> Result<PathBuf> {
    for saved in [
        q,
        if q == Quality::High {
            Quality::Standard
        } else {
            Quality::High
        },
    ] {
        if cache
            .map(id, saved)?
            .is_some_and(|m| m.pinned && m.complete())
            && let Some(path) = cache.lookup(store, id, saved)?
        {
            return Ok(path);
        }
    }
    if let Some(path) = cache.lookup(store, id, q)? {
        return Ok(path);
    }
    let mut map = cache.map(id, q)?;
    if map.as_ref().is_some_and(|m| m.etag.is_none()) {
        cache.discard(store, id, q)?;
        map = None;
    }
    let mut stream = resolve()?;
    let mut renewals = 0;
    let mut restarts = 0;
    loop {
        if !matches!(stream.encryption, Encryption::None) {
            return Err("шифрование потока не поддерживается".into());
        }
        let (start, end) = map
            .as_ref()
            .and_then(|m| m.missing(CHUNK))
            .unwrap_or((0, CHUNK));
        if stream.expires_at <= SystemTime::now() {
            stream = resolve()?;
        }
        let mut request = Request::get(stream.url.clone());
        request.range = Some((start, end - 1));
        request.limit = CHUNK as usize;
        if let Some(etag) = map.as_ref().and_then(|m| m.etag.as_ref()) {
            request.headers.push(("If-Range".into(), etag.clone()));
        }
        let response = match net.request(Priority::Playback, request, None) {
            Ok(r) => r,
            Err(eagle_net::Error::Status(401 | 403 | 410)) if renewals < 2 => {
                stream = resolve()?;
                renewals += 1;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let (a, b, total) = parse_range(response.content_range.as_deref().ok_or_else(|| {
            format!(
                "сервер не вернул Content-Range (HTTP {}, {} байт)",
                response.status,
                response.bytes.len()
            )
        })?)?;
        if response.status != 206
            || a != start
            || b > end
            || b - a != response.bytes.len() as u64
            || total > LIMIT
        {
            return Err("неверный диапазон в ответе сервера".into());
        }
        let m = map.get_or_insert_with(|| RangeMap::new(total));
        if m.total != total
            || m.etag
                .as_ref()
                .is_some_and(|old| Some(old) != response.etag.as_ref())
        {
            if restarts >= 1 {
                return Err("файл на сервере постоянно меняется".into());
            }
            cache.discard(store, id, q)?;
            map = None;
            restarts += 1;
            continue;
        }
        m.etag = response.etag;
        m.bitrate_kbps = stream.bitrate_kbps;
        cache.write_chunk(store, id, q, m, start, &response.bytes)?;
        if m.complete() {
            return Ok(cache.path(id, q));
        }
        // Finish the whole file in one burst, then workers park throughout playback.
    }
}
fn parse_range(value: &str) -> Result<(u64, u64, u64)> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or("неверный Content-Range")?;
    let (range, total) = value.split_once('/').ok_or("неверный Content-Range")?;
    let (a, b) = range.split_once('-').ok_or("неверный Content-Range")?;
    let a = a.parse::<u64>()?;
    let b = b
        .parse::<u64>()?
        .checked_add(1)
        .ok_or("переполнение диапазона")?;
    let total = total.parse::<u64>()?;
    if a >= b || b > total {
        return Err("неверный Content-Range".into());
    }
    Ok((a, b, total))
}
pub(crate) fn metadata(
    store: &Store,
    net: &Client,
    root: &Path,
    track: &eagle_core::Track,
) -> Result<()> {
    store.put_track(track)?;
    let dir = root.join("covers");
    fs::create_dir_all(&dir)?;
    let name = track.id.to_string().replace(':', "_");
    let small = dir.join(format!("{name}-64.png"));
    let large = dir.join(format!("{name}-600.png"));
    let colors: Option<String> = store.read()?.query_row(
        "SELECT (SELECT json FROM colors WHERE id=?1)",
        [track.id.to_string()],
        |r| r.get(0),
    )?;
    let valid_cover = |path: &Path, size| {
        fs::metadata(path)
            .ok()
            .filter(|m| m.len() <= 8 * 1024 * 1024)
            .and_then(|_| fs::read(path).ok())
            .and_then(|bytes| eagle_palette::decode_cover(&bytes, size).ok())
            .is_some()
    };
    if colors
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<[u8; 3]>>(json).ok())
        .is_some_and(|colors| !colors.is_empty())
        && valid_cover(&small, 64)
        && valid_cover(&large, 600)
    {
        return Ok(());
    }
    // Unique staging paths allow playback and two qualities to prepare the same cover.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_small = dir.join(format!("{name}-{}-{next}-64.png", std::process::id()));
    let temp_large = dir.join(format!("{name}-{}-{next}-600.png", std::process::id()));
    let prepared = (|| -> Result<Vec<[u8; 3]>> {
        let colors = if let Some(cover) = &track.cover {
            let url = if cover.starts_with("https://") {
                cover.replace("%%", "600x600")
            } else {
                format!("https://{}", cover.replace("%%", "600x600"))
            };
            let mut request = Request::get(SignedUrl::new(url));
            request.limit = 8 * 1024 * 1024;
            let response = net.request(Priority::Metadata, request, None)?;
            eagle_palette::process(&response.bytes, &temp_small, &temp_large)?
        } else {
            eagle_palette::placeholder(&temp_small, &temp_large)?
        };
        for path in [&temp_small, &temp_large] {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all()?;
        }
        fs::rename(&temp_small, &small)?;
        fs::rename(&temp_large, &large)?;
        #[cfg(unix)]
        fs::File::open(&dir)?.sync_all()?;
        Ok(colors)
    })();
    for path in [&temp_small, &temp_large] {
        let _ = fs::remove_file(path);
    }
    let colors = prepared?;
    store.put_json(
        "colors",
        &track.id.to_string(),
        serde_json::to_string(&colors)?,
    )?;
    Ok(())
}
/// Full sync for the CLI: likes, then every playlist with its tracks.
pub(crate) fn sync(store: &Store, provider: &dyn Provider) -> Result<()> {
    sync_likes(store, provider)?;
    let mut playlists = Vec::new();
    for p in provider.playlists(None)?.items {
        let p = provider.playlist(&p.id)?;
        store.put_tracks(&p.tracks)?;
        playlists.push((p.id.to_string(), serde_json::to_string(&p)?));
    }
    store.replace_playlists(provider.info().id, playlists)?;
    Ok(())
}
/// Writes metadata page by page; merges the provider's likes only after every page succeeds.
pub(crate) fn sync_likes(store: &Store, provider: &dyn Provider) -> Result<()> {
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = provider.likes(cursor.as_deref())?;
        store.put_tracks(&page.items)?;
        ids.extend(page.items.iter().map(|track| track.id.clone()));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    store.sync_provider_likes(provider.info().id, &ids)?;
    Ok(())
}
/// Playlist titles only (one request), so the sidebar fills before the slow likes sync.
pub(crate) fn sync_playlists(store: &Store, provider: &dyn Provider) -> Result<()> {
    let playlists = provider
        .playlists(None)?
        .items
        .iter()
        .map(|p| Ok((p.id.to_string(), serde_json::to_string(p)?)))
        .collect::<Result<Vec<_>>>()?;
    store.replace_playlists(provider.info().id, playlists)?;
    Ok(())
}
/// Asks for the provider token without echoing it.
fn prompt_token() -> std::io::Result<Token> {
    rpassword::prompt_password("Токен: ").map(Token::new)
}
pub fn run(args: &[String], root: &Path) -> Result<()> {
    if args.is_empty() {
        println!(
            "eagle-cli [провайдер] login|status|search <текст>|play <provider:id>|likes|playlists|album <id>|playlist <id>|like <id>|unlike <id>|sync\neagle-cli play <provider:id> [--standard] [--save]"
        );
        return Ok(());
    }
    let net = Arc::new(Client::new()?);
    if args[0] == "play" {
        return crate::playback::run(&args[1..], root, net);
    }
    let providers = Providers::new(crate::providers::builtin(net.clone()), &[])
        .with_tokens(Arc::new(crate::tokens::Keyring));
    // Without a provider id the command goes to the first provider by priority.
    let (provider, words) = match providers.get(&args[0]) {
        Some(provider) => (provider, &args[1..]),
        None => (providers.all().first().ok_or("нет провайдеров")?, args),
    };
    let (command, tail) = words
        .split_first()
        .map(|(command, tail)| (command.as_str(), tail))
        .ok_or("укажите команду провайдера")?;
    if command == "play" {
        return crate::playback::run(tail, root, net);
    }
    if command == "login" {
        let token = prompt_token()?;
        let account = providers.sign_in(provider.as_ref(), token)?;
        println!("Вход выполнен: {}", account.id);
        sync(&Store::open(root)?, provider.as_ref())?;
        return Ok(());
    }
    if command == "status" {
        let a = providers.restore(provider.as_ref())?;
        println!("Аккаунт: {}, Plus: {}", a.id, a.subscription);
        return Ok(());
    }
    if command == "play" {
        return crate::playback::run(tail, root, net);
    }
    // Other commands work only with the saved token; a missing one fails with `Auth` below.
    let _ = providers.restore(provider.as_ref());
    let store = Store::open(root)?;
    let tid =
        || -> Result<TrackId> { Ok(tail.first().ok_or("укажите provider:id")?.parse()?) };
    match command {
        "search" => {
            if tail.is_empty() {
                return Err("укажите поисковый запрос".into());
            }
            for track in provider.search(&tail.join(" "), 0)?.items {
                store.put_track(&track)?;
                println!(
                    "{}\t{}\t{}{}",
                    track.id,
                    track
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    track.title,
                    if track.availability == eagle_core::Availability::Available {
                        ""
                    } else {
                        " [недоступен]"
                    }
                );
            }
        }
        "likes" => {
            let mut cursor = None;
            loop {
                let p = provider.likes(cursor.as_deref())?;
                for t in p.items {
                    println!("{}\t{}", t.id, t.title);
                }
                cursor = p.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        "playlists" => {
            for p in provider.playlists(None)?.items {
                println!("{}\t{}", p.id, p.title);
            }
        }
        "album" => {
            let a = provider.album(&tid()?)?;
            store.put_json("albums", &a.id.to_string(), serde_json::to_string(&a)?)?;
            for t in a.tracks {
                store.put_track(&t)?;
                println!("{}\t{}", t.id, t.title);
            }
        }
        "playlist" => {
            let p = provider.playlist(&tid()?)?;
            store.put_json("playlists", &p.id.to_string(), serde_json::to_string(&p)?)?;
            for t in p.tracks {
                store.put_track(&t)?;
                println!("{}\t{}", t.id, t.title);
            }
        }
        "like" | "unlike" => {
            provider.set_liked(&tid()?, command == "like")?;
            sync(&store, provider.as_ref())?;
        }
        "dislike" | "undislike" => {
            provider.set_disliked(&tid()?, command == "dislike")?;
            sync(&store, provider.as_ref())?;
        }
        "sync" => sync(&store, provider.as_ref())?,
        _ => return Err("неизвестная команда; запустите eagle-cli без аргументов".into()),
    }
    Ok(())
}

/// Fixed offline storage fixture for reproducible memory measurements.
pub fn bench_store(root: &Path) -> Result<()> {
    let store = Store::open(root)?;
    let _cache = Cache::open(&root.join("audio"), LIMIT * 4, &store)?;
    for n in 0..1000 {
        store.put_track(&eagle_core::Track {
            id: format!("fixture:{n}").parse()?,
            title: "Benchmark fixture".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
            isrc: None,
        })?;
    }
    // Hold 100 decoded RGB thumbnails (64 x 64) as a defined cover-cache workload.
    let _covers = std::hint::black_box(vec![vec![128u8; 64 * 64 * 3]; 100]);
    fs::write(root.join("bench-ready"), b"ready")?;
    loop {
        std::thread::park();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };
    #[test]
    fn expired_url_renews_and_second_play_never_calls_resolver() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for i in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut buf = [0; 2048];
                let _ = socket.read(&mut buf).unwrap();
                if i == 0 {
                    socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else if i == 1 {
                    socket.write_all(b"HTTP/1.1 308 Permanent Redirect\r\nLocation: /cdn/audio\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nETag: fixture\r\nConnection: close\r\n\r\ntest").unwrap();
                }
            }
        });
        let root = std::env::temp_dir().join(format!(
            "eg-app-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let id = "yandex:123".parse().unwrap();
        let net = Client::new().unwrap();
        let store = Store::open(&root).unwrap();
        let cache = Cache::open(&root.join("audio"), 100, &store).unwrap();
        let mut calls = 0;
        let path = audio(&store, &cache, &net, &id, Quality::High, || {
            calls += 1;
            Ok(Stream {
                bitrate_kbps: None,
                url: SignedUrl::new(format!("http://{addr}/audio")),
                codec: "mp3".into(),
                expires_at: SystemTime::now() + Duration::from_secs(60),
                encryption: Encryption::None,
            })
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(net.stats().requests, 3);
        assert_eq!(fs::read(&path).unwrap(), b"test");
        server.join().unwrap();
        assert_eq!(
            audio(&store, &cache, &net, &id, Quality::High, || panic!(
                "offline cache hit must not resolve or retrieve a token"
            ))
            .unwrap(),
            path
        );
        assert_eq!(net.stats().requests, 3);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_invalid_ranges() {
        for s in [
            "bytes 4-3/9",
            "bytes 0-9/5",
            "bytes 0-18446744073709551615/9",
            "garbage",
        ] {
            assert!(parse_range(s).is_err());
        }
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    #[test]
    fn missing_artwork_is_durable_and_corrupt_artwork_is_repaired_offline() {
        let root = std::env::temp_dir().join(format!("eg-metadata-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let net = Client::new().unwrap();
        let track = eagle_core::Track {
            id: "yandex:coverless".parse().unwrap(),
            title: "fixture".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
            isrc: None,
        };
        metadata(&store, &net, &root, &track).unwrap();
        let cover = root.join("covers/yandex_coverless-600.png");
        let original = fs::read(&cover).unwrap();
        let decoded = eagle_palette::decode_cover(&original, 600).unwrap();
        assert_eq!((decoded.width, decoded.height), (600, 600));
        let modified = fs::metadata(&cover).unwrap().modified().unwrap();
        metadata(&store, &net, &root, &track).unwrap();
        assert_eq!(fs::metadata(&cover).unwrap().modified().unwrap(), modified);
        fs::write(&cover, b"broken").unwrap();
        metadata(&store, &net, &root, &track).unwrap();
        assert_eq!(fs::read(&cover).unwrap(), original);
        assert_eq!(store.track(&track.id).unwrap().unwrap(), track);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}

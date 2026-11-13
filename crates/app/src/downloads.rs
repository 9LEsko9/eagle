// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent audio download queue. No window or audio device is required.
use crate::session::Event;
use eagle_core::{Encryption, Provider};
use eagle_net::{
    Client, Priority,
    growing::{Download, Location, Snapshot},
};
use eagle_store::{Cache, DownloadJob, DownloadState, RangeMap, Store};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

enum Request {
    Account(String, mpsc::SyncSender<()>),
    Wake,
    NetworkPolicy(eagle_net::policy::NetworkPolicy),
    SystemNetwork(eagle_net::policy::NetworkPolicy),
    Done(i64, i64, bool),
    Shutdown,
}
#[derive(Default)]
struct Attempt {
    cancelled: AtomicBool,
    unsupported_quality: Arc<AtomicBool>,
    download: Mutex<Option<Arc<Download>>>,
}
impl Attempt {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(download) = &*self.download.lock().unwrap_or_else(|p| p.into_inner()) {
            download.cancel();
        }
    }
}
struct Active {
    job: DownloadJob,
    attempt: Arc<Attempt>,
    thread: thread::JoinHandle<()>,
}
pub(crate) struct Downloads {
    tx: mpsc::Sender<Request>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Downloads {
    pub(crate) fn start(
        root: PathBuf,
        store: Arc<Store>,
        cache: Arc<Cache>,
        net: Arc<Client>,
        provider: Arc<dyn Provider>,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let replies = tx.clone();
        let worker = thread::Builder::new()
            .name("eg-downloads".into())
            .spawn(move || {
                let mut account = String::new();
                let allow_metered = store.setting("allow_metered").ok().flatten().as_deref() == Some("true");
                let fixture = cfg!(test) || provider.name() == "fixture";
                let mut network_policy = if fixture {
                    eagle_net::policy::NetworkPolicy { allow_metered, ..Default::default() }
                } else {
                    eagle_net::system_network::query(allow_metered)
                };
                #[cfg(not(test))]
                let _monitor = (!fixture).then(|| {
                    let tx = replies.clone();
                    eagle_net::system_network::NetworkMonitor::start(move |policy| {
                        let _ = tx.send(Request::SystemNetwork(policy));
                    })
                });
                let mut active: Vec<Active> = Vec::new();
                for request in rx {
                    match request {
                        Request::Account(next, ready) => {
                            stop(&mut active);
                            if let Err(error) = store.recover_downloads(&account) {
                                emit(Event::Error(error.to_string()));
                            }
                            account = next;
                            if let Err(error) = store.recover_downloads(&account) {
                                emit(Event::Error(error.to_string()));
                            }
                            let _ = ready.send(());
                        }
                        Request::Shutdown => {
                            stop(&mut active);
                            if let Err(error) = store.recover_downloads(&account) {
                                emit(Event::Error(error.to_string()));
                            }
                            break;
                        }
                        Request::Done(id, revision, success) => {
                            if let Some(index) = active
                                .iter()
                                .position(|a| a.job.id == id && a.job.revision == revision)
                            {
                                let a = active.remove(index);
                                let _ = a.thread.join();
                                if let Err(error) = if success
                                    && a.job.state == DownloadState::AudioReady
                                {
                                    store.finish_saved_download(&a.job)
                                } else if success {
                                    Ok(true)
                                } else if a.attempt.unsupported_quality.load(Ordering::Acquire) {
                                    store.reject_download_quality(&a.job)
                                } else {
                                    store.finish_download(&a.job, false)
                                } {
                                    emit(Event::Error(error.to_string()));
                                }
                            }
                        }
                        Request::NetworkPolicy(policy) => {
                            network_policy.allow_metered = policy.allow_metered;
                            emit(Event::NetworkPolicy(network_policy));
                            if !network_policy.allows_background() {
                                for a in &active {
                                    if let Err(error) = store.control_download(
                                        &account,
                                        a.job.id,
                                        DownloadState::Queued,
                                    ) {
                                        emit(Event::Error(error.to_string()));
                                    }
                                    a.attempt.cancel();
                                }
                            }
                        }
                        Request::SystemNetwork(policy) => {
                            network_policy.online = policy.online;
                            network_policy.metered = policy.metered;
                            emit(Event::NetworkPolicy(network_policy));
                            if !network_policy.allows_background() {
                                for a in &active {
                                    if let Err(error) = store.control_download(&account, a.job.id, DownloadState::Queued) { emit(Event::Error(error.to_string())); }
                                    a.attempt.cancel();
                                }
                            }
                        }
                        Request::Wake => {}
                    }
                    if account.is_empty() {
                        emit(Event::Downloads(Vec::new()));
                        continue;
                    }
                    let jobs = match store.downloads(&account) {
                        Ok(jobs) => jobs,
                        Err(error) => {
                            emit(Event::Error(error.to_string()));
                            continue;
                        }
                    };
                    for a in &active {
                        if !jobs
                            .iter()
                            .any(|j| j.id == a.job.id && j.revision == a.job.revision)
                        {
                            a.attempt.cancel();
                        }
                    }
                    // Keep cancelled attempts in the two physical slots until their threads exit.
                    while network_policy.allows_background()
                        && active.len() < 2
                        && !active
                            .iter()
                            .any(|a| a.attempt.cancelled.load(Ordering::Acquire))
                    {
                        let job = match store.claim_download(&account) {
                            Ok(Some(job)) => job,
                            Ok(None) => break,
                            Err(error) => {
                                emit(Event::Error(error.to_string()));
                                break;
                            }
                        };
                        let attempt = Arc::new(Attempt::default());
                        let (root, task_store, cache, net, provider, tx, cancel, task) = (
                            root.clone(),
                            store.clone(),
                            cache.clone(),
                            net.clone(),
                            provider.clone(),
                            replies.clone(),
                            attempt.clone(),
                            job.clone(),
                        );
                        match thread::Builder::new()
                            .name("eg-save-audio".into())
                            .spawn(move || {
                                if task.state != DownloadState::AudioReady {
                                    let result = audio(
                                        &root,
                                        &task_store,
                                        &cache,
                                        net.clone(),
                                        provider.clone(),
                                        &task,
                                        &cancel,
                                    );
                                    if let Err(error) = &result {
                                        log::warn!(provider = task.track.provider(), track_id = task.track.local(), kind = download_error_kind(error.as_ref()); "audio download failed");
                                    }
                                    let success = result.is_ok()
                                        && !cancel.cancelled.load(Ordering::Acquire)
                                        && matches!(
                                            task_store.finish_download(&task, true),
                                            Ok(true)
                                        );
                                    let _ = tx.send(Request::Done(task.id, task.revision, success));
                                    return;
                                }
                                let success = (|| -> Result<()> {
                                    if cancel.cancelled.load(Ordering::Acquire) {
                                        return Err("загрузка отменена".into());
                                    }
                                    let track = match task_store.track(&task.track)? {
                                        Some(track) => track,
                                        None => {
                                            if provider.name() != task.track.provider()
                                                || provider.account()?.id != task.account
                                            {
                                                return Err("аккаунт загрузки изменился".into());
                                            }
                                            provider.track(&task.track)?
                                        }
                                    };
                                    if track.id != task.track {
                                        return Err("неверные метаданные трека".into());
                                    }
                                    crate::music::metadata(&task_store, &net, &root, &track)
                                })()
                                .is_ok()
                                    && !cancel.cancelled.load(Ordering::Acquire);
                                let _ = tx.send(Request::Done(task.id, task.revision, success));
                            }) {
                            Ok(thread) => active.push(Active {
                                job,
                                attempt,
                                thread,
                            }),
                            Err(error) => {
                                let _ = store.finish_download(&job, false);
                                emit(Event::Error(error.to_string()));
                                break;
                            }
                        }
                    }
                    if let Ok(jobs) = store.downloads(&account) {
                        emit(Event::Downloads(jobs));
                    }
                }
            })?;
        Ok(Self {
            tx,
            thread: Some(worker),
        })
    }
    pub(crate) fn account(&self, account: String) {
        let (ready, wait) = mpsc::sync_channel(0);
        if self.tx.send(Request::Account(account, ready)).is_ok() {
            let _ = wait.recv();
        }
    }
    pub(crate) fn network_policy(&self, policy: eagle_net::policy::NetworkPolicy) {
        let _ = self.tx.send(Request::NetworkPolicy(policy));
    }
    pub(crate) fn wake(&self) {
        let _ = self.tx.send(Request::Wake);
    }
}
impl Drop for Downloads {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn stop(active: &mut Vec<Active>) {
    for a in active.iter() {
        a.attempt.cancel();
    }
    for a in active.drain(..) {
        let _ = a.thread.join();
    }
}
fn save_map(path: &Path, map: &RangeMap) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(map)?)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
fn download_error_kind(error: &(dyn std::error::Error + 'static)) -> &'static str {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        return match error.kind() {
            io::ErrorKind::Interrupted => "cancelled",
            io::ErrorKind::UnexpectedEof => "incomplete_audio",
            io::ErrorKind::PermissionDenied => "permission_denied",
            io::ErrorKind::StorageFull => "storage_full",
            io::ErrorKind::Other => {
                let message = error.to_string();
                match message.as_str() {
                    "ошибка сети или таймаут" => "network",
                    "ETag или размер файла изменился; повторите воспроизведение" => {
                        "resource_changed"
                    }
                    "неверный диапазон ответа" | "нет Content-Range" => {
                        "invalid_range"
                    }
                    "не удалось сохранить ход загрузки" | "не удалось сохранить аудиокэш" => {
                        "cache_write"
                    }
                    text if text.starts_with("HTTP ")
                        && text[5..].bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        "http_status"
                    }
                    _ => "io_other",
                }
            }
            _ => "io",
        };
    }
    if let Some(error) = error.downcast_ref::<eagle_net::Error>() {
        return match error {
            eagle_net::Error::Changed => "resource_changed",
            eagle_net::Error::Status(_) => "http_status",
            eagle_net::Error::Network => "network",
            eagle_net::Error::TooLarge => "too_large",
            eagle_net::Error::Stopped => "network_stopped",
            eagle_net::Error::QueueFull => "network_queue_full",
            eagle_net::Error::Source(_) => "provider",
        };
    }
    "other"
}
fn stream_error(error: eagle_core::ProviderError, unsupported: &AtomicBool) -> eagle_net::Error {
    if matches!(error, eagle_core::ProviderError::UnsupportedQuality) {
        unsupported.store(true, Ordering::Release);
    }
    eagle_net::Error::Source("не удалось получить адрес аудио")
}
fn audio(
    root: &Path,
    store: &Store,
    cache: &Cache,
    net: Arc<Client>,
    provider: Arc<dyn Provider>,
    job: &DownloadJob,
    attempt: &Attempt,
) -> Result<()> {
    let (id, quality, account) = (job.track.clone(), job.quality, job.account.clone());
    let unsupported_quality = attempt.unsupported_quality.clone();
    let resolve = Arc::new(move || {
        // Credentials can also be changed by another entry point between launches.
        if provider.name() != id.provider()
            || provider
                .account()
                .map_err(|_| eagle_net::Error::Source("не удалось проверить аккаунт загрузки"))?
                .id
                != account
        {
            return Err(eagle_net::Error::Source("аккаунт загрузки изменился"));
        }
        let stream = provider
            .resolve_stream(&id, quality)
            .map_err(|error| stream_error(error, &unsupported_quality))?;
        if !matches!(stream.encryption, Encryption::None) {
            return Err(eagle_net::Error::Source(
                "шифрование аудио не поддерживается",
            ));
        }
        Ok(Location {
            bitrate_kbps: stream.bitrate_kbps,
            url: stream.url,
            expires_at: Some(stream.expires_at),
        })
    });
    audio_with_resolver(root, store, cache, net, job, attempt, resolve)
}
fn audio_with_resolver(
    root: &Path,
    store: &Store,
    cache: &Cache,
    net: Arc<Client>,
    job: &DownloadJob,
    attempt: &Attempt,
    resolve: Arc<eagle_net::growing::Resolve>,
) -> Result<()> {
    let first = staged_audio(
        root,
        store,
        cache,
        net.clone(),
        job,
        attempt,
        resolve.clone(),
    );
    // Changed resources and a server dropping Range invalidate the durable map.
    // Restart once from zero; ordinary network/disk failures remain explicit retries.
    let invalidated = fs::read(
        root.join("audio/downloads")
            .join(job.id.to_string())
            .join("ranges.json"),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<RangeMap>(&bytes).ok())
    .is_some_and(|map| map.ranges.is_empty() && map.etag.is_none());
    if first.is_err() && invalidated && !attempt.cancelled.load(Ordering::Acquire) {
        attempt
            .download
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        return staged_audio(root, store, cache, net, job, attempt, resolve);
    }
    first
}
// Interrupted means a cancelled GrowingReader, not a transient read to retry.
fn drain_download(reader: &mut impl Read) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    while reader.read(&mut buffer)? != 0 {}
    Ok(())
}
fn staged_audio(
    root: &Path,
    store: &Store,
    cache: &Cache,
    net: Arc<Client>,
    job: &DownloadJob,
    attempt: &Attempt,
    resolve: Arc<eagle_net::growing::Resolve>,
) -> Result<()> {
    if attempt.cancelled.load(Ordering::Acquire) {
        return Err("загрузка отменена".into());
    }
    {
        let _lease = cache.lease(&job.track, job.quality);
        if cache.lookup(store, &job.track, job.quality)?.is_some() {
            let track = job.track.to_string();
            if store.requires_shared_authorization(&job.account, &track)? {
                resolve()?;
                if attempt.cancelled.load(Ordering::Acquire) {
                    return Err("загрузка отменена".into());
                }
            }
            cache.pin(store, &job.track, job.quality, true)?;
            store.record_audio_owner(&job.account, &track)?;
            return Ok(());
        }
    }
    // Job IDs are unique across accounts. Staging never writes into a playing file.
    let dir = root
        .join("audio")
        .join("downloads")
        .join(job.id.to_string());
    fs::create_dir_all(&dir)?;
    let path = dir.join("audio");
    let map_path = dir.join("ranges.json");
    let initial: Option<RangeMap> = fs::read(&map_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|m: &RangeMap| fs::metadata(&path).is_ok_and(|f| m.valid(f.len())))
        .filter(|m| {
            m.complete()
                || m.etag
                    .as_deref()
                    .is_some_and(eagle_net::growing::strong_etag)
        });
    if let Some(map) = initial.as_ref().filter(|m| m.complete()) {
        let track = job.track.to_string();
        if store.requires_shared_authorization(&job.account, &track)? {
            resolve()?;
            if attempt.cancelled.load(Ordering::Acquire) {
                return Err("загрузка отменена".into());
            }
        }
        cache.install_pinned(store, &job.track, job.quality, &path, map.clone())?;
        store.record_audio_owner(&job.account, &track)?;
        return Ok(());
    }
    let initial = initial.map_or_else(Snapshot::default, |m| Snapshot {
        bitrate_kbps: m.bitrate_kbps,
        total: Some(m.total),
        ranges: m.ranges,
        etag: m.etag,
    });
    let publish = Arc::new(move |s: &Snapshot| {
        if let Some(total) = s.total {
            save_map(
                &map_path,
                &RangeMap {
                    bitrate_kbps: s.bitrate_kbps,
                    total,
                    ranges: s.ranges.clone(),
                    etag: s.etag.clone(),
                    pinned: false,
                },
            )
            .map_err(|_| eagle_net::Error::Source("не удалось сохранить ход загрузки"))?;
        }
        Ok(())
    });
    let download = Arc::new(Download::start(
        &path,
        net,
        resolve,
        initial,
        Priority::Background,
        Some(publish),
    )?);
    {
        let mut slot = attempt.download.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(download.clone());
        if attempt.cancelled.load(Ordering::Acquire) {
            download.cancel();
        }
    }
    let mut reader = download.reader(Arc::new(AtomicU32::new(0)), 0, Priority::Background)?;
    drain_download(&mut reader)?;
    if attempt.cancelled.load(Ordering::Acquire) {
        return Err("загрузка отменена".into());
    }
    let snapshot = download.snapshot();
    drop(reader);
    attempt
        .download
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    drop(download);
    cache.install_pinned(
        store,
        &job.track,
        job.quality,
        &path,
        RangeMap {
            bitrate_kbps: snapshot.bitrate_kbps,
            total: snapshot.total.ok_or("размер аудио не подтверждён")?,
            ranges: snapshot.ranges,
            etag: snapshot.etag,
            pinned: false,
        },
    )?;
    store.record_audio_owner(&job.account, &job.track.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_core::{Quality, SignedUrl};
    use eagle_store::DownloadState;
    use std::{net::TcpListener, sync::atomic::AtomicUsize, time::Duration};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn fixture() -> (PathBuf, Store, Cache, DownloadJob) {
        let root = std::env::temp_dir().join(format!(
            "eg-save-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::open(&root).unwrap();
        let cache = Cache::open(&root.join("audio"), 1, &store).unwrap();
        store
            .enqueue_download("a", &"yandex:1".parse().unwrap(), Quality::High)
            .unwrap();
        let job = store.claim_download("a").unwrap().unwrap();
        (root, store, cache, job)
    }
    #[test]
    fn cancelled_growing_reader_does_not_retry_interrupted_forever() {
        let root = std::env::temp_dir().join(format!(
            "eg-cancel-reader-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let download = Download::start(
            &root.join("audio"),
            Arc::new(Client::new().unwrap()),
            Arc::new(|| Err(eagle_net::Error::Network)),
            Snapshot::default(),
            Priority::Background,
            None,
        )
        .unwrap();
        let mut reader = download
            .reader(Arc::new(AtomicU32::new(0)), 0, Priority::Background)
            .unwrap();
        download.cancel();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = drain_download(&mut reader);
            drop(download);
            let _ = tx.send(result.map_err(|error| error.kind()));
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(io::ErrorKind::Interrupted)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_explicit_quality_failure_marks_the_attempt() {
        for error in [
            eagle_core::ProviderError::Network,
            eagle_core::ProviderError::Auth,
            eagle_core::ProviderError::Unavailable,
        ] {
            let flag = AtomicBool::new(false);
            let _ = stream_error(error, &flag);
            assert!(!flag.load(Ordering::Acquire));
        }
        let flag = AtomicBool::new(false);
        let _ = stream_error(eagle_core::ProviderError::UnsupportedQuality, &flag);
        assert!(flag.load(Ordering::Acquire));
    }
    #[test]
    fn cached_audio_requires_explicit_authorization_for_another_account() {
        let (root, store, cache, job) = fixture();
        cache
            .write_chunk(
                &store,
                &job.track,
                job.quality,
                &mut RangeMap::new(1),
                0,
                b"a",
            )
            .unwrap();
        store
            .record_audio_owner("a", &job.track.to_string())
            .unwrap();
        store
            .enqueue_download("b", &job.track, job.quality)
            .unwrap();
        let b = store.claim_download("b").unwrap().unwrap();
        let net = Arc::new(Client::new().unwrap());
        assert!(
            audio_with_resolver(
                &root,
                &store,
                &cache,
                net.clone(),
                &b,
                &Attempt::default(),
                Arc::new(|| Err(eagle_net::Error::Network))
            )
            .is_err()
        );
        assert!(!store.owns_audio("b", &job.track.to_string()).unwrap());
        assert!(
            !store
                .allows_local_playback("b", &job.track.to_string())
                .unwrap()
        );
        audio_with_resolver(
            &root,
            &store,
            &cache,
            net,
            &b,
            &Attempt::default(),
            Arc::new(|| Ok(SignedUrl::new("https://example.invalid/audio".into()).into())),
        )
        .unwrap();
        assert!(store.owns_audio("b", &job.track.to_string()).unwrap());
        assert!(
            store
                .allows_local_playback("b", &job.track.to_string())
                .unwrap()
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn legacy_pinned_audio_requires_authorization_once_and_survives_restart() {
        let (root, store, cache, job) = fixture();
        let mut map = RangeMap::new(1);
        cache
            .write_chunk(&store, &job.track, job.quality, &mut map, 0, b"x")
            .unwrap();
        cache.pin(&store, &job.track, job.quality, true).unwrap();
        let path = cache.path(&job.track, job.quality);
        let net = Arc::new(Client::new().unwrap());
        for error in [
            eagle_net::Error::Network,
            eagle_net::Error::Source("denied"),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            assert!(
                audio_with_resolver(
                    &root,
                    &store,
                    &cache,
                    net.clone(),
                    &job,
                    &Attempt::default(),
                    Arc::new(move || {
                        count.fetch_add(1, Ordering::Relaxed);
                        Err(match error {
                            eagle_net::Error::Network => eagle_net::Error::Network,
                            _ => eagle_net::Error::Source("denied"),
                        })
                    })
                )
                .is_err()
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(!store.owns_audio("a", &job.track.to_string()).unwrap());
            assert_eq!(fs::read(&path).unwrap(), b"x");
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        audio_with_resolver(
            &root,
            &store,
            &cache,
            net.clone(),
            &job,
            &Attempt::default(),
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(SignedUrl::new("https://example.invalid/audio".into()).into())
            }),
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(net.stats().requests, 0);
        drop(cache);
        drop(store);
        let store = Store::open(&root).unwrap();
        let cache = Cache::open(&root.join("audio"), 1, &store).unwrap();
        assert!(
            store
                .allows_local_playback("a", &job.track.to_string())
                .unwrap()
        );
        assert_eq!(
            fs::read(
                cache
                    .lookup(&store, &job.track, job.quality)
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            b"x"
        );
        drop(cache);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn complete_cache_pins_in_place_without_resolving_or_copying() {
        let (root, store, cache, job) = fixture();
        // A complete unpinned file may be larger than the current limit after a settings change.
        let mut map = RangeMap::new(1);
        cache
            .write_chunk(&store, &job.track, job.quality, &mut map, 0, b"a")
            .unwrap();
        store
            .record_audio_owner("a", &job.track.to_string())
            .unwrap();
        let before = fs::metadata(cache.path(&job.track, job.quality))
            .unwrap()
            .modified()
            .unwrap();
        audio_with_resolver(
            &root,
            &store,
            &cache,
            Arc::new(Client::new().unwrap()),
            &job,
            &Attempt::default(),
            Arc::new(|| panic!("offline cache hit must not resolve")),
        )
        .unwrap();
        assert_eq!(cache.usage(&store).unwrap(), (0, 1));
        assert_eq!(
            before,
            fs::metadata(cache.path(&job.track, job.quality))
                .unwrap()
                .modified()
                .unwrap()
        );
        assert!(!root.join("audio/downloads").exists());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn staged_resume_uses_fresh_location_and_never_overwrites_active_playback() {
        let (root, store, cache, job) = fixture();
        let dir = root.join("audio/downloads").join(job.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("audio"), b"abcd").unwrap();
        save_map(
            &dir.join("ranges.json"),
            &RangeMap {
                bitrate_kbps: None,
                total: 8,
                ranges: vec![(0, 4)],
                etag: Some("\"v1\"".into()),
                pinned: false,
            },
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = SignedUrl::new(format!("http://{}/fresh", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.contains("range: bytes=4-"), "{request}");
            assert!(request.contains("if-range: \"v1\""), "{request}");
            socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nefgh").unwrap();
        });
        let resolves = Arc::new(AtomicUsize::new(0));
        let count = resolves.clone();
        let resolve = Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(url.clone().into())
        });
        cache
            .write_chunk(
                &store,
                &job.track,
                job.quality,
                &mut RangeMap::new(8),
                0,
                b"x",
            )
            .unwrap();
        let lease = cache.lease(&job.track, job.quality);
        assert!(
            audio_with_resolver(
                &root,
                &store,
                &cache,
                Arc::new(Client::new().unwrap()),
                &job,
                &Attempt::default(),
                resolve
            )
            .is_err()
        );
        server.join().unwrap();
        assert_eq!(resolves.load(Ordering::Relaxed), 1);
        assert_eq!(fs::read(cache.path(&job.track, job.quality)).unwrap(), b"x");
        assert_eq!(
            cache.map(&job.track, job.quality).unwrap().unwrap().ranges,
            [(0, 1)]
        );
        assert_eq!(fs::read(dir.join("audio")).unwrap(), b"abcdefgh");
        drop(lease);
        audio_with_resolver(
            &root,
            &store,
            &cache,
            Arc::new(Client::new().unwrap()),
            &job,
            &Attempt::default(),
            Arc::new(|| Ok(SignedUrl::new("https://example.invalid/audio".into()).into())),
        )
        .unwrap();
        assert_eq!(
            fs::read(cache.path(&job.track, job.quality)).unwrap(),
            b"abcdefgh"
        );
        assert_eq!(cache.usage(&store).unwrap(), (0, 8));
        assert!(store.finish_download(&job, true).unwrap());
        assert_eq!(
            store.downloads("a").unwrap()[0].state,
            DownloadState::AudioReady
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn changed_resource_or_lost_range_restarts_without_joining_old_bytes() {
        for changed in [false, true] {
            let (root, store, cache, job) = fixture();
            let dir = root.join("audio/downloads").join(job.id.to_string());
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("audio"), b"OLD!").unwrap();
            save_map(
                &dir.join("ranges.json"),
                &RangeMap {
                    bitrate_kbps: None,
                    total: 8,
                    ranges: vec![(0, 4)],
                    etag: Some("\"v1\"".into()),
                    pinned: false,
                },
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = SignedUrl::new(format!("http://{}/audio", listener.local_addr().unwrap()));
            let server = thread::spawn(move || {
                for n in 0..2 {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        assert_eq!(socket.read(&mut byte).unwrap(), 1);
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap().to_lowercase();
                    assert!(request.contains(if n == 0 {
                        "range: bytes=4-"
                    } else {
                        "range: bytes=0-"
                    }));
                    let response = if n == 0 && changed {
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"v2\"\r\nConnection: close\r\n\r\nNEW!"
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nNEW!NEW!"
                    };
                    let _ = socket.write_all(response.as_bytes());
                }
            });
            let count = Arc::new(AtomicUsize::new(0));
            let calls = count.clone();
            audio_with_resolver(
                &root,
                &store,
                &cache,
                Arc::new(Client::new().unwrap()),
                &job,
                &Attempt::default(),
                Arc::new(move || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(url.clone().into())
                }),
            )
            .unwrap();
            server.join().unwrap();
            assert_eq!(count.load(Ordering::Relaxed), 2);
            assert_eq!(
                fs::read(cache.path(&job.track, job.quality)).unwrap(),
                b"NEW!NEW!"
            );
            drop(store);
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn network_policy_defers_background_work_until_allowed() {
        use eagle_net::policy::{NetworkPolicy, NetworkStatus};
        let (root, store, cache, job) = fixture();
        cache
            .write_chunk(
                &store,
                &job.track,
                job.quality,
                &mut RangeMap::new(1),
                0,
                b"x",
            )
            .unwrap();
        let track = eagle_core::Track {
            id: job.track.clone(),
            title: "fixture".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
        };
        store.put_track(&track).unwrap();
        store
            .record_audio_owner("a", &job.track.to_string())
            .unwrap();
        let store = Arc::new(store);
        let net = Arc::new(Client::new().unwrap());
        let (tx, rx) = mpsc::channel();
        let service = Downloads::start(
            root.clone(),
            store.clone(),
            Arc::new(cache),
            net.clone(),
            Arc::new(eagle_yandex::Yandex::new(net)),
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
        service.network_policy(NetworkPolicy {
            allow_metered: true,
            ..NetworkPolicy::default()
        });
        service
            .tx
            .send(Request::SystemNetwork(NetworkPolicy {
                metered: NetworkStatus::Yes,
                ..NetworkPolicy::default()
            }))
            .unwrap();
        loop {
            if let Event::NetworkPolicy(policy) = rx.recv_timeout(Duration::from_secs(5)).unwrap()
                && policy.metered == NetworkStatus::Yes
            {
                assert!(policy.allow_metered);
                break;
            }
        }
        service.network_policy(NetworkPolicy::default());
        service
            .tx
            .send(Request::SystemNetwork(NetworkPolicy {
                metered: NetworkStatus::Yes,
                ..NetworkPolicy::default()
            }))
            .unwrap();
        service.account("a".into());
        assert_eq!(
            store.downloads("a").unwrap()[0].state,
            DownloadState::Queued
        );
        service
            .tx
            .send(Request::SystemNetwork(NetworkPolicy::default()))
            .unwrap();
        loop {
            if let Event::Downloads(jobs) = rx.recv_timeout(Duration::from_secs(5)).unwrap()
                && jobs.iter().any(|j| j.state == DownloadState::Saved)
            {
                break;
            }
        }
        drop(service);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn coordinator_recovers_running_jobs_and_only_processes_current_account() {
        let (root, store, cache, job) = fixture();
        cache
            .write_chunk(
                &store,
                &job.track,
                job.quality,
                &mut RangeMap::new(1),
                0,
                b"x",
            )
            .unwrap();
        let track = eagle_core::Track {
            id: job.track.clone(),
            title: "fixture".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
        };
        store.put_track(&track).unwrap();
        store
            .record_audio_owner("a", &job.track.to_string())
            .unwrap();
        store
            .enqueue_download("b", &job.track, job.quality)
            .unwrap();
        let store = Arc::new(store);
        let net = Arc::new(Client::new().unwrap());
        let (tx, rx) = mpsc::channel();
        let service = Downloads::start(
            root.clone(),
            store.clone(),
            Arc::new(cache),
            net.clone(),
            Arc::new(eagle_yandex::Yandex::new(net)),
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap();
        service.account("a".into());
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if let Event::Downloads(jobs) = event
                && jobs.iter().any(|j| j.state == DownloadState::Saved)
            {
                break;
            }
        }
        assert_eq!(
            store.downloads("b").unwrap()[0].state,
            DownloadState::Queued
        );
        assert!(!store.finish_download(&job, true).unwrap());
        drop(service);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}

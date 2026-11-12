// SPDX-License-Identifier: GPL-3.0-or-later
//! Background artwork for lists and cards.
//!
//! Requests come in batches for what is on screen. The newest batch goes first and the
//! queue is bounded, so fast scrolling never builds a backlog. Downloads use background
//! priority and are written once to the cover cache next to the player artwork.
use crate::session::Event;
use eagle_core::SignedUrl;
use eagle_net::{Client, Priority, Request};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Most pending jobs kept; older ones are dropped (they scrolled away).
const PENDING: usize = 64;
/// Decoded covers kept in memory to make scrolling back free.
const DECODED: usize = 160;
const DECODED_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct Job {
    /// Routing key for the UI, e.g. `t:yandex:1` for a track row.
    pub key: String,
    /// File stem in the cover cache.
    pub stem: String,
    /// Provider URI template (`%%` is the size); `None` uses only what is on disk.
    pub uri: Option<String>,
    /// Edge of the decoded image in pixels.
    pub size: u32,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Job>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

pub(crate) struct Covers {
    shared: Arc<Shared>,
}

impl Covers {
    pub(crate) fn start(
        root: PathBuf,
        net: Arc<Client>,
        emit: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        // Detached on purpose: a download in flight must not delay shutdown.
        thread::Builder::new()
            .name("eg-covers".into())
            .spawn(move || run(&root.join("covers"), &net, emit.as_ref(), &worker))?;
        Ok(Self { shared })
    }
    pub(crate) fn request(&self, jobs: Vec<Job>) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        enqueue(&mut queue.pending, jobs);
        self.shared.ready.notify_one();
    }
}

impl Drop for Covers {
    fn drop(&mut self) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = true;
        queue.pending.clear();
        self.shared.ready.notify_one();
    }
}

fn enqueue(pending: &mut VecDeque<Job>, batch: Vec<Job>) {
    for job in batch.into_iter().rev() {
        if job.key.starts_with("h:") {
            pending.retain(|p| !p.key.starts_with("h:"));
        } else {
            pending.retain(|p| p.key != job.key || p.size != job.size);
        }
        pending.push_front(job);
        if pending.len() > PENDING {
            let index = pending.iter().rposition(|p| !p.key.starts_with("h:"));
            if let Some(index) = index {
                pending.remove(index);
            }
        }
    }
}

fn next_job(pending: &mut VecDeque<Job>) -> Option<Job> {
    let index = pending
        .iter()
        .position(|job| job.key.starts_with("h:"))
        .unwrap_or(0);
    pending.remove(index)
}

#[derive(Default)]
struct Decoded {
    entries: VecDeque<(String, u32, Arc<eagle_palette::Cover>)>,
    bytes: usize,
}

impl Decoded {
    fn get(&mut self, key: &str, size: u32) -> Option<Arc<eagle_palette::Cover>> {
        let index = self
            .entries
            .iter()
            .position(|(k, s, _)| k == key && *s == size)?;
        let entry = self.entries.remove(index)?;
        let cover = entry.2.clone();
        self.entries.push_front(entry);
        Some(cover)
    }

    fn insert(&mut self, key: String, size: u32, cover: Arc<eagle_palette::Cover>) {
        let bytes = cover.rgba.len();
        if bytes > DECODED_BYTES {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|(k, s, _)| *k == key && *s == size)
            && let Some(entry) = self.entries.remove(index)
        {
            self.bytes -= entry.2.rgba.len();
        }
        while self.entries.len() >= DECODED || self.bytes + bytes > DECODED_BYTES {
            if let Some(entry) = self.entries.pop_back() {
                self.bytes -= entry.2.rgba.len();
            }
        }
        self.bytes += bytes;
        self.entries.push_front((key, size, cover));
    }
}

fn run(dir: &Path, net: &Client, emit: &(dyn Fn(Event) + Send + Sync), shared: &Shared) {
    run_with(shared, emit, |job| load(dir, net, job));
}

fn run_with(
    shared: &Shared,
    emit: &(dyn Fn(Event) + Send + Sync),
    mut loader: impl FnMut(&Job) -> Result<Option<eagle_palette::Cover>>,
) {
    let mut decoded = Decoded::default();
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            while queue.pending.is_empty() && !queue.closed {
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
            if queue.closed {
                return;
            }
            let Some(job) = next_job(&mut queue.pending) else {
                continue;
            };
            job
        };
        if let Some(cover) = decoded.get(&job.key, job.size) {
            emit(Event::Thumbnail(job.key, cover));
            continue;
        }
        match loader(&job) {
            Ok(Some(cover)) => {
                let cover = Arc::new(cover);
                decoded.insert(job.key.clone(), job.size, cover.clone());
                emit(Event::Thumbnail(job.key, cover));
            }
            Ok(None) => {}
            Err(error) => log::debug!(error = error.to_string().as_str(); "cover unavailable"),
        }
    }
}

fn load(dir: &Path, net: &Client, job: &Job) -> Result<Option<eagle_palette::Cover>> {
    let small = dir.join(format!("{}-64.png", job.stem));
    let large = dir.join(format!("{}-600.png", job.stem));
    let path = if job.size <= 64 { &small } else { &large };
    if !path.exists() {
        let Some(uri) = &job.uri else {
            return Ok(None);
        };
        download(net, uri, &small, &large)?;
    }
    Ok(Some(eagle_palette::decode_cover(
        &std::fs::read(path)?,
        job.size,
    )?))
}

/// Fetches the 600 px artwork once and writes both cached sizes.
pub(crate) fn download(net: &Client, uri: &str, small: &Path, large: &Path) -> Result<()> {
    let sized = uri.replace("%%", "600x600");
    let url = if sized.starts_with("https://") {
        sized
    } else {
        format!("https://{sized}")
    };
    let mut request = Request::get(SignedUrl::new(url));
    request.limit = 8 * 1024 * 1024;
    let response = net.request(Priority::Background, request, None)?;
    if let Some(dir) = small.parent() {
        std::fs::create_dir_all(dir)?;
    }
    eagle_palette::process(&response.bytes, small, large)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(key: &str) -> Job {
        Job {
            key: key.into(),
            stem: key.into(),
            uri: None,
            size: 64,
        }
    }
    #[test]
    fn newest_batch_first_without_duplicates_and_bounded() {
        let mut pending = VecDeque::new();
        enqueue(&mut pending, vec![job("a"), job("b")]);
        enqueue(&mut pending, vec![job("c"), job("a")]);
        let keys: Vec<_> = pending.iter().map(|j| j.key.as_str()).collect();
        assert_eq!(keys, ["c", "a", "b"]);
        enqueue(
            &mut pending,
            (0..100).map(|i| job(&i.to_string())).collect(),
        );
        assert_eq!(pending.len(), PENDING);
        assert_eq!(pending[0].key, "0");
    }
    #[test]
    fn header_survives_scrolling_and_new_header_replaces_it() {
        let mut pending = VecDeque::new();
        enqueue(&mut pending, vec![job("h:old")]);
        enqueue(
            &mut pending,
            (0..200).map(|i| job(&i.to_string())).collect(),
        );
        assert_eq!(pending.len(), PENDING);
        assert!(pending.iter().any(|j| j.key == "h:old"));
        assert_eq!(next_job(&mut pending).unwrap().key, "h:old");
        enqueue(&mut pending, vec![job("h:old")]);
        enqueue(&mut pending, vec![job("h:new")]);
        assert_eq!(pending.len(), PENDING);
        assert!(pending.iter().any(|j| j.key == "h:new"));
        assert!(!pending.iter().any(|j| j.key == "h:old"));
    }

    fn cover(bytes: usize) -> Arc<eagle_palette::Cover> {
        Arc::new(eagle_palette::Cover {
            rgba: vec![0; bytes],
            width: 1,
            height: 1,
            dominant: [0; 3],
        })
    }

    #[test]
    fn decoded_cache_evicts_lru_by_bytes_and_count() {
        let mut cache = Decoded::default();
        cache.insert("a".into(), 64, cover(DECODED_BYTES / 2));
        cache.insert("b".into(), 64, cover(DECODED_BYTES / 2));
        assert!(cache.get("a", 64).is_some());
        cache.insert("c".into(), 64, cover(1));
        assert!(cache.get("b", 64).is_none());
        assert!(cache.get("a", 64).is_some());
        cache.insert("huge".into(), 64, cover(DECODED_BYTES + 1));
        assert!(cache.get("huge", 64).is_none());
        let mut cache = Decoded::default();
        for i in 0..=DECODED {
            cache.insert(i.to_string(), 64, cover(1));
        }
        assert_eq!(cache.entries.len(), DECODED);
        assert!(cache.get("0", 64).is_none());
    }

    #[test]
    fn dropping_service_wakes_waiter() {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        let waiter = thread::spawn(move || {
            let mut queue = worker.queue.lock().unwrap();
            while !queue.closed {
                queue = worker.ready.wait(queue).unwrap();
            }
        });
        drop(Covers { shared });
        waiter.join().unwrap();
    }
    #[test]
    fn pending_is_bounded_while_worker_is_blocked() {
        use std::sync::mpsc;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let worker = thread::spawn(move || {
            run_with(&worker_shared, &|_| {}, |_| {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
                Ok(None)
            });
        });
        let service = Covers { shared };
        service.request(vec![job("first")]);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        service.request(vec![job("h:current")]);
        for batch in 0..20 {
            service.request((0..100).map(|n| job(&format!("{batch}-{n}"))).collect());
        }
        {
            let queue = service.shared.queue.lock().unwrap();
            assert_eq!(queue.pending.len(), PENDING);
            assert!(queue.pending.iter().any(|job| job.key == "h:current"));
            assert!(queue.pending.iter().any(|job| job.key == "19-0"));
        }
        drop(service);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }
}

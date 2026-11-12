// SPDX-License-Identifier: GPL-3.0-or-later
//! Demand-driven disk-backed HTTP input. Only verified byte ranges are readable.
use crate::{BodySink, Client, Error, Priority, Request, ResponseHead};
use eagle_core::SignedUrl;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};
const CHUNK: u64 = 2 * 1024 * 1024;
const LIMIT: u64 = 512 * 1024 * 1024;
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub total: Option<u64>,
    pub ranges: Vec<(u64, u64)>,
    pub etag: Option<String>,
}
pub struct Location {
    pub url: SignedUrl,
    pub expires_at: Option<SystemTime>,
}
impl From<SignedUrl> for Location {
    fn from(url: SignedUrl) -> Self {
        Self {
            url,
            expires_at: None,
        }
    }
}
pub type Resolve = dyn Fn() -> Result<Location, Error> + Send + Sync;
pub type Publish = dyn Fn(&Snapshot) -> Result<(), Error> + Send + Sync;
struct State {
    snapshot: Snapshot,
    demand: Option<u64>,
    active: Option<(u64, u64)>,
    request_generation: u64,
    range: Option<bool>,
    error: Option<String>,
    stopped: bool,
    finished: bool,
    priority: Priority,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    file: Mutex<File>,
}
pub struct Download {
    shared: Arc<Shared>,
    worker: Option<thread::JoinHandle<()>>,
    path: std::path::PathBuf,
}
pub struct GrowingReader {
    shared: Arc<Shared>,
    file: File,
    position: u64,
    generation: Arc<AtomicU32>,
    expected: u32,
    seeking: Option<Arc<AtomicBool>>,
    priority: Priority,
}
impl Download {
    pub fn start(
        path: &Path,
        net: Arc<Client>,
        resolve: Arc<Resolve>,
        initial: Snapshot,
        priority: Priority,
        publish: Option<Arc<Publish>>,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if initial.ranges.is_empty() {
            file.set_len(0)?;
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                snapshot: initial,
                demand: None,
                active: None,
                request_generation: 0,
                range: None,
                error: None,
                stopped: false,
                finished: false,
                priority,
            }),
            changed: Condvar::new(),
            file: Mutex::new(file),
        });
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("eg-download".into())
            .spawn(move || run(state, net, resolve, publish))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            path: path.to_owned(),
        })
    }
    pub fn reader(
        &self,
        generation: Arc<AtomicU32>,
        expected: u32,
        priority: Priority,
    ) -> io::Result<GrowingReader> {
        Ok(GrowingReader {
            shared: self.shared.clone(),
            file: File::open(&self.path)?,
            position: 0,
            generation,
            expected,
            seeking: None,
            priority,
        })
    }
    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot
            .clone()
    }
    pub fn failed(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .is_some()
    }
    pub fn cancel(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.shared.changed.notify_all();
    }
}
impl Drop for Download {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl GrowingReader {
    pub fn with_seek_guard(mut self, seeking: Arc<AtomicBool>) -> Self {
        self.seeking = Some(seeking);
        self
    }
    pub fn byte_len(&self) -> Option<u64> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot
            .total
    }
    pub fn is_seekable(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        state.range != Some(false)
            || state
                .snapshot
                .total
                .is_some_and(|n| state.snapshot.ranges == [(0, n)])
    }
    fn interrupted(&self) -> bool {
        self.generation.load(Ordering::Acquire) != self.expected
    }
}
impl Read for GrowingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if self.interrupted() || state.stopped {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "чтение отменено",
                ));
            }
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }
            if let Some(total) = state.snapshot.total
                && self.position >= total
            {
                if state.snapshot.ranges != [(0, total)] || state.finished {
                    return Ok(0);
                }
                state = self
                    .shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|p| p.into_inner())
                    .0;
                continue;
            }
            if let Some(&(_, end)) = state
                .snapshot
                .ranges
                .iter()
                .find(|&&(a, b)| a <= self.position && self.position < b)
            {
                let count = buffer.len().min((end - self.position) as usize);
                drop(state);
                self.file.seek(SeekFrom::Start(self.position))?;
                let n = self.file.read(&mut buffer[..count])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "кэш обрезан"));
                }
                self.position += n as u64;
                return Ok(n);
            }
            if state.range == Some(false)
                && self
                    .seeking
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::Acquire))
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "сервер не поддерживает Range; перемотка за загруженные данные недоступна",
                ));
            }
            if state.finished {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "загрузка завершена с дырой",
                ));
            }
            if !state
                .active
                .is_some_and(|(a, b)| a <= self.position && self.position < b)
                && state.demand != Some(self.position)
            {
                state.priority = self.priority;
                state.demand = Some(self.position);
                state.request_generation += 1;
                self.shared.changed.notify_all();
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}
impl Seek for GrowingReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        let target = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => {
                state.snapshot.total.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "размер потока ещё неизвестен")
                })? as i128
                    + n as i128
            }
        };
        if !(0..=u64::MAX as i128).contains(&target) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "неверная позиция",
            ));
        }
        let target = target as u64;
        let high = state
            .snapshot
            .ranges
            .first()
            .filter(|&&(a, _)| a == 0)
            .map_or(0, |&(_, b)| b);
        if state.range == Some(false) && target > high {
            if self
                .seeking
                .as_ref()
                .is_some_and(|s| !s.load(Ordering::Acquire))
            {
                // Container probing may need tail metadata. Wait for sequential download;
                // explicit user seeks are still rejected beyond available bytes.
                loop {
                    if self.interrupted() || state.stopped {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "чтение отменено",
                        ));
                    }
                    if let Some(error) = &state.error {
                        return Err(io::Error::other(error.clone()));
                    }
                    let high = state
                        .snapshot
                        .ranges
                        .first()
                        .filter(|&&(a, _)| a == 0)
                        .map_or(0, |&(_, b)| b);
                    if target <= high || state.finished {
                        break;
                    }
                    state = self
                        .shared
                        .changed
                        .wait_timeout(state, Duration::from_millis(100))
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "сервер не поддерживает Range; перемотка за загруженные данные недоступна",
                ));
            }
        }
        let available = state
            .snapshot
            .ranges
            .iter()
            .any(|&(a, b)| a <= target && target < b);
        let active = state.active.is_some_and(|(a, b)| a <= target && target < b);
        if target != self.position && !available && !active {
            state.request_generation += 1;
            state.demand = None;
            self.shared.changed.notify_all();
        }
        self.position = target;
        Ok(target)
    }
}
struct Sink {
    shared: Arc<Shared>,
    request_start: u64,
    generation: u64,
    offset: Mutex<u64>,
    expected_end: Mutex<Option<u64>>,
}
impl BodySink for Sink {
    fn limit(&self) -> u64 {
        LIMIT
    }
    fn begin(&self, head: &ResponseHead) -> Result<(), Error> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped {
            return Err(Error::Source("загрузка отменена"));
        }
        if state
            .snapshot
            .etag
            .as_ref()
            .is_some_and(|old| head.etag.as_ref() != Some(old))
        {
            return Err(Error::Changed);
        }
        let (start, end, total) = if head.status == 206 {
            let (start, end, total) = parse_range(
                head.content_range
                    .as_deref()
                    .ok_or(Error::Source("нет Content-Range"))?,
            )?;
            if start != self.request_start || end - start > CHUNK {
                return Err(Error::Source("неверный диапазон ответа"));
            }
            state.range = Some(true);
            (start, Some(end), total)
        } else if head.status == 200 && self.request_start == 0 {
            state.range = Some(false);
            (0, head.content_length, head.content_length)
        } else {
            return Err(Error::Changed);
        };
        if total.is_some_and(|n| n > LIMIT || n == 0) {
            return Err(Error::TooLarge);
        }
        if state.snapshot.total.is_some() && total.is_some() && state.snapshot.total != total {
            return Err(Error::Changed);
        }
        state.snapshot.total = total.or(state.snapshot.total);
        state.snapshot.etag = head.etag.clone();
        *self.offset.lock().unwrap_or_else(|p| p.into_inner()) = start;
        *self.expected_end.lock().unwrap_or_else(|p| p.into_inner()) = end;
        Ok(())
    }
    fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        let mut state = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped
            || (state.range == Some(true) && self.generation != state.request_generation)
        {
            return Err(Error::Source("устаревший запрос отменён"));
        }
        let mut offset = self.offset.lock().unwrap_or_else(|p| p.into_inner());
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::TooLarge)?;
        if end > LIMIT
            || self
                .expected_end
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some_and(|n| end > n)
        {
            return Err(Error::Source("ответ вышел за границы"));
        }
        {
            let mut file = self.shared.file.lock().unwrap_or_else(|p| p.into_inner());
            file.seek(SeekFrom::Start(*offset))
                .and_then(|_| file.write_all(bytes))
                .map_err(|_| Error::Source("не удалось записать аудиокэш"))?;
        }
        add_range(&mut state.snapshot.ranges, *offset, end);
        *offset = end;
        self.shared.changed.notify_all();
        Ok(())
    }
}
fn run(
    shared: Arc<Shared>,
    net: Arc<Client>,
    resolve: Arc<Resolve>,
    publish: Option<Arc<Publish>>,
) {
    let mut url: Option<Location> = None;
    loop {
        let (start, generation, priority, etag) = {
            let mut s = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            while s.demand.is_none() && !s.stopped {
                s = shared.changed.wait(s).unwrap_or_else(|p| p.into_inner());
            }
            if s.stopped {
                return;
            }
            let start = s.demand.take().unwrap_or(0);
            s.active = Some((start, start.saturating_add(CHUNK)));
            (
                start,
                s.request_generation,
                s.priority,
                s.snapshot.etag.clone(),
            )
        };
        let result = (|| {
            if url
                .as_ref()
                .is_none_or(|u| u.expires_at.is_some_and(|t| t <= SystemTime::now()))
            {
                url = Some(resolve()?);
            }
            let sink = Arc::new(Sink {
                shared: shared.clone(),
                request_start: start,
                generation,
                offset: Mutex::new(start),
                expected_end: Mutex::new(None),
            });
            let mut response = None;
            for attempt in 0..3 {
                let mut request = Request::get(url.as_ref().ok_or(Error::Network)?.url.clone());
                request.range = Some((start, start.saturating_add(CHUNK) - 1));
                request.limit = CHUNK as usize;
                if let Some(etag) = &etag {
                    request.headers.push(("If-Range".into(), etag.clone()));
                }
                match net.request_into(priority, request, sink.clone()) {
                    Err(Error::Status(401 | 403 | 410)) if attempt < 2 => {
                        url = Some(resolve()?);
                    }
                    other => {
                        response = Some(other);
                        break;
                    }
                }
            }
            let response = response.ok_or(Error::Network)?;
            let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            // Seeks invalidate requests, but never invalidate already verified bytes.
            if state.stopped
                || (state.range == Some(true) && generation != state.request_generation)
            {
                return Ok(());
            }
            if matches!(response, Err(Error::Status(416))) && state.snapshot.total.is_none() {
                if state.snapshot.ranges == [(0, start)] {
                    state.snapshot.total = Some(start);
                    state.finished = true;
                } else {
                    return Err(Error::Source("416 не подтверждает конец файла"));
                }
            } else {
                let response = response?;
                let end = *sink.offset.lock().unwrap_or_else(|p| p.into_inner());
                if sink
                    .expected_end
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some_and(|n| end != n)
                {
                    return Err(Error::Source("оборванное тело HTTP"));
                }
                if response.status == 200 {
                    state.snapshot.total = Some(end);
                    state.finished = true;
                }
                if state
                    .snapshot
                    .total
                    .is_some_and(|n| state.snapshot.ranges == [(0, n)])
                {
                    state.finished = true;
                }
            }
            shared
                .file
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .sync_data()
                .map_err(|_| Error::Source("не удалось сохранить аудиокэш"))?;
            let snapshot = state.snapshot.clone();
            let finished = state.finished;
            state.finished = false;
            drop(state);
            if let Some(publish) = &publish {
                publish(&snapshot)?;
            }
            shared
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .finished = finished;
            Ok(())
        })();
        if matches!(result, Err(Error::Changed))
            && let Some(publish) = &publish
        {
            let mut invalid = shared
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .snapshot
                .clone();
            invalid.ranges.clear();
            invalid.etag = None;
            let _ = publish(&invalid);
        }
        let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active = None;
        if let Err(error) = result
            && generation == state.request_generation
            && !state.stopped
        {
            state.error = Some(error.to_string());
        }
        shared.changed.notify_all();
        if state.finished || state.error.is_some() {
            return;
        }
    }
}
fn add_range(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    ranges.push((start, end));
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges.iter() {
        if let Some(last) = merged.last_mut()
            && a <= last.1
        {
            last.1 = last.1.max(b);
            continue;
        }
        merged.push((a, b));
    }
    *ranges = merged;
}
fn parse_range(value: &str) -> Result<(u64, u64, Option<u64>), Error> {
    let fail = || Error::Source("неверный Content-Range");
    let (range, total) = value
        .strip_prefix("bytes ")
        .ok_or_else(fail)?
        .split_once('/')
        .ok_or_else(fail)?;
    let (a, b) = range.split_once('-').ok_or_else(fail)?;
    let a: u64 = a.parse().map_err(|_| fail())?;
    let b = b
        .parse::<u64>()
        .map_err(|_| fail())?
        .checked_add(1)
        .ok_or_else(fail)?;
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().map_err(|_| fail())?)
    };
    if a >= b || total.is_some_and(|n| b > n) {
        return Err(fail());
    }
    Ok((a, b, total))
}
#[cfg(test)]
#[path = "growing_tests.rs"]
mod tests;

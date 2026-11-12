// SPDX-License-Identifier: GPL-3.0-or-later
//! SQLite metadata and durable range maps. Only eg-store owns a writable connection.
use eagle_core::{Quality, Track, TrackId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SQLite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("ошибка диска: {0}")]
    Io(#[from] io::Error),
    #[error("повреждённые метаданные")]
    Corrupt,
    #[error("база создана более новой версией eagle")]
    FutureSchema,
    #[error("поток SQLite остановлен")]
    Stopped,
    #[error("лимит кэша занят закреплёнными файлами")]
    CacheFull,
}
type Job = Box<dyn FnOnce(&mut Connection) + Send>;
pub struct Store {
    path: PathBuf,
    tx: Option<mpsc::Sender<Job>>,
    worker: Option<thread::JoinHandle<()>>,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn initialize(path: &Path) -> Result<Connection, Error> {
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > 1 {
        return Err(Error::FutureSchema);
    }
    if version == 0 {
        c.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE tracks(id TEXT PRIMARY KEY,json TEXT NOT NULL);
        CREATE TABLE albums(id TEXT PRIMARY KEY,json TEXT NOT NULL);
        CREATE TABLE artists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
        CREATE TABLE playlists(id TEXT PRIMARY KEY,json TEXT NOT NULL);
        CREATE TABLE likes(id TEXT PRIMARY KEY);
        CREATE TABLE cache(id TEXT,quality TEXT,path TEXT NOT NULL,size INTEGER NOT NULL,access INTEGER NOT NULL,pinned INTEGER NOT NULL,PRIMARY KEY(id,quality));
        CREATE TABLE colors(id TEXT PRIMARY KEY,json TEXT NOT NULL);
        CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        PRAGMA user_version=1; COMMIT;")?;
    }
    let check: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::Corrupt);
    }
    Ok(c)
}
impl Store {
    pub fn open(root: &Path) -> Result<Self, Error> {
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let path = root.join("metadata.sqlite");
        let connection = match initialize(&path) {
            Ok(c) => c,
            Err(e)
                if matches!(&e, Error::Corrupt)
                    || matches!(&e, Error::Sql(e) if matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))) =>
            {
                // Preserve corrupt metadata for diagnosis; pinned audio is recovered from sidecars.
                let stamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                for suffix in ["", "-wal", "-shm"] {
                    let source = PathBuf::from(format!("{}{suffix}", path.display()));
                    if source.exists() {
                        fs::rename(
                            &source,
                            root.join(format!("corrupt-{stamp}{suffix}.sqlite")),
                        )?;
                    }
                }
                initialize(&path)?
            }
            Err(e) => return Err(e),
        };
        let (tx, rx) = mpsc::channel::<Job>();
        let worker = thread::Builder::new()
            .name("eg-store".into())
            .spawn(move || {
                let mut c = connection;
                for job in rx {
                    job(&mut c);
                }
            })?;
        Ok(Self {
            path,
            tx: Some(tx),
            worker: Some(worker),
        })
    }
    pub fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .as_ref()
            .ok_or(Error::Stopped)?
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| Error::Stopped)?;
        rx.recv().map_err(|_| Error::Stopped)?
    }
    pub fn read(&self) -> Result<Connection, Error> {
        let c =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(c)
    }
    pub fn put_track(&self, track: &Track) -> Result<(), Error> {
        let id = track.id.to_string();
        let json = serde_json::to_string(track).map_err(|_| Error::Corrupt)?;
        let artists = track
            .artists
            .iter()
            .map(|a| {
                Ok((
                    a.id.to_string(),
                    serde_json::to_string(a).map_err(|_| Error::Corrupt)?,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        self.write(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT OR REPLACE INTO tracks VALUES(?1,?2)",
                params![id, json],
            )?;
            for (id, json) in artists {
                tx.execute(
                    "INSERT OR REPLACE INTO artists VALUES(?1,?2)",
                    params![id, json],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }
    pub fn track(&self, id: &TrackId) -> Result<Option<Track>, Error> {
        let s: Option<String> = self
            .read()?
            .query_row(
                "SELECT json FROM tracks WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| serde_json::from_str(&s).map_err(|_| Error::Corrupt))
            .transpose()
    }
    pub fn put_json(&self, table: &str, id: &str, json: String) -> Result<(), Error> {
        if !matches!(table, "albums" | "artists" | "playlists" | "colors") {
            return Err(Error::Corrupt);
        }
        let sql = format!("INSERT OR REPLACE INTO {table} VALUES(?1,?2)");
        let id = id.to_owned();
        self.write(move |c| {
            c.execute(&sql, params![id, json])?;
            Ok(())
        })
    }
    pub fn replace_likes(&self, ids: Vec<String>) -> Result<(), Error> {
        self.write(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM likes WHERE id LIKE 'yandex:%'", [])?;
            for id in ids {
                tx.execute("INSERT OR IGNORE INTO likes VALUES(?1)", [id])?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeMap {
    pub total: u64,
    pub ranges: Vec<(u64, u64)>,
    pub etag: Option<String>,
    pub pinned: bool,
}
impl RangeMap {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            ranges: vec![],
            etag: None,
            pinned: false,
        }
    }
    pub fn valid(&self, len: u64) -> bool {
        self.total > 0
            && len <= self.total
            && self.ranges.iter().all(|&(a, b)| a < b && b <= len)
            && self.ranges.windows(2).all(|w| w[0].1 < w[1].0)
    }
    pub fn complete(&self) -> bool {
        self.ranges == [(0, self.total)]
    }
    pub fn add(&mut self, start: u64, end: u64) -> Result<(), Error> {
        if start >= end || end > self.total {
            return Err(Error::Corrupt);
        }
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = vec![];
        for &(a, b) in &self.ranges {
            if let Some(last) = merged.last_mut()
                && a <= last.1
            {
                last.1 = last.1.max(b);
                continue;
            }
            merged.push((a, b));
        }
        self.ranges = merged;
        Ok(())
    }
    pub fn missing(&self, chunk: u64) -> Option<(u64, u64)> {
        let mut start = 0;
        for &(a, b) in &self.ranges {
            if start < a {
                return Some((start, a.min(start.saturating_add(chunk))));
            }
            start = b;
        }
        (start < self.total).then(|| (start, self.total.min(start.saturating_add(chunk))))
    }
}
pub struct Cache {
    root: PathBuf,
    pub limit: u64,
    leases: Arc<Mutex<HashMap<PathBuf, usize>>>,
}

/// Prevents LRU eviction until the active reader is dropped.
pub struct CacheLease {
    path: PathBuf,
    leases: Arc<Mutex<HashMap<PathBuf, usize>>>,
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = leases.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                leases.remove(&self.path);
            }
        }
    }
}
impl Cache {
    pub fn open(root: &Path, limit: u64, store: &Store) -> Result<Self, Error> {
        fs::create_dir_all(root)?;
        let cache = Self {
            root: root.to_owned(),
            limit,
            leases: Arc::default(),
        };
        // The disk is authoritative after DB loss. Invalid maps never become cache hits.
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|x| x == "map") {
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some((key, q)) = stem.rsplit_once('.') else {
                    continue;
                };
                let Some((provider, id)) = key.split_once('_') else {
                    continue;
                };
                let Ok(id) = format!("{provider}:{id}").parse::<TrackId>() else {
                    continue;
                };
                let quality = match q {
                    "high" => Quality::High,
                    "standard" => Quality::Standard,
                    _ => continue,
                };
                if let Some(map) = cache.map(&id, quality)? {
                    cache.index(store, &id, quality, &map)?;
                }
            }
        }
        let rows = {
            let c = store.read()?;
            let mut stmt = c.prepare("SELECT id,quality FROM cache")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, q) in rows {
            let tid = id.parse::<TrackId>().map_err(|_| Error::Corrupt)?;
            let quality = if q == "high" {
                Quality::High
            } else {
                Quality::Standard
            };
            if cache.map(&tid, quality)?.is_none() {
                cache.discard(store, &tid, quality)?;
            }
        }
        Ok(cache)
    }
    pub fn lease(&self, id: &TrackId, q: Quality) -> CacheLease {
        let path = self.path(id, q);
        *self
            .leases
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(path.clone())
            .or_default() += 1;
        CacheLease {
            path,
            leases: self.leases.clone(),
        }
    }
    pub fn path(&self, id: &TrackId, q: Quality) -> PathBuf {
        self.root
            .join(format!("{}_{}.{}", id.provider(), id.local(), q.key()))
    }
    fn sidecar(&self, id: &TrackId, q: Quality) -> PathBuf {
        self.path(id, q).with_extension(format!("{}.map", q.key()))
    }
    pub fn map(&self, id: &TrackId, q: Quality) -> Result<Option<RangeMap>, Error> {
        let path = self.path(id, q);
        let sidecar = self.sidecar(id, q);
        let data = match fs::read(&sidecar) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let map = serde_json::from_slice::<RangeMap>(&data)
            .ok()
            .filter(|m| fs::metadata(&path).is_ok_and(|s| m.valid(s.len())));
        if map.is_none() {
            if path.exists() {
                fs::remove_file(&path)?;
            }
            fs::remove_file(sidecar)?;
        }
        Ok(map)
    }
    fn save_map(&self, id: &TrackId, q: Quality, map: &RangeMap) -> Result<(), Error> {
        let sidecar = self.sidecar(id, q);
        let temp = sidecar.with_extension("tmp");
        let data = serde_json::to_vec(map).map_err(|_| Error::Corrupt)?;
        let mut f = File::create(&temp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        fs::rename(temp, sidecar)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn write_chunk(
        &self,
        store: &Store,
        id: &TrackId,
        q: Quality,
        map: &mut RangeMap,
        start: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let end = start
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Corrupt)?;
        if start >= end || end > map.total {
            return Err(Error::Corrupt);
        }
        let mut updated = map.clone();
        updated.add(start, end)?;
        let old_size = map.ranges.iter().map(|(a, b)| b - a).sum::<u64>();
        let new_size = updated.ranges.iter().map(|(a, b)| b - a).sum::<u64>();
        self.make_room(store, new_size - old_size, id, q)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path(id, q))?;
        file.seek(SeekFrom::Start(start))?;
        file.write_all(bytes)?;
        file.sync_data()?;
        self.save_map(id, q, &updated)?;
        *map = updated;
        self.index(store, id, q, map)
    }
    fn index(&self, store: &Store, id: &TrackId, q: Quality, map: &RangeMap) -> Result<(), Error> {
        let path = self.path(id, q).to_string_lossy().into_owned();
        let id = id.to_string();
        let q = q.key().to_owned();
        let size = i64::try_from(map.ranges.iter().map(|(a, b)| b - a).sum::<u64>())
            .map_err(|_| Error::Corrupt)?;
        let pinned = map.pinned;
        store.write(move|c|{c.execute("INSERT INTO cache VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id,quality) DO UPDATE SET size=excluded.size,pinned=excluded.pinned",params![id,q,path,size,now(),pinned])?;Ok(())})
    }
    /// Publish ranges already written and synced by the streaming downloader.
    pub fn publish(
        &self,
        store: &Store,
        id: &TrackId,
        q: Quality,
        map: &RangeMap,
    ) -> Result<(), Error> {
        if !map.valid(fs::metadata(self.path(id, q))?.len()) {
            return Err(Error::Corrupt);
        }
        let previous = self
            .map(id, q)?
            .map_or(0, |m| m.ranges.iter().map(|(a, b)| b - a).sum::<u64>());
        let size = map.ranges.iter().map(|(a, b)| b - a).sum::<u64>();
        self.make_room(store, size.saturating_sub(previous), id, q)?;
        self.save_map(id, q, map)?;
        self.index(store, id, q, map)
    }
    pub fn lookup(
        &self,
        store: &Store,
        id: &TrackId,
        q: Quality,
    ) -> Result<Option<PathBuf>, Error> {
        if let Some(map) = self.map(id, q)?
            && map.complete()
        {
            self.index(store, id, q, &map)?;
            let key = id.to_string();
            let quality = q.key().to_owned();
            store.write(move |c| {
                c.execute(
                    "UPDATE cache SET access=?1 WHERE id=?2 AND quality=?3",
                    params![now(), key, quality],
                )?;
                Ok(())
            })?;
            return Ok(Some(self.path(id, q)));
        }
        Ok(None)
    }
    pub fn pin(&self, store: &Store, id: &TrackId, q: Quality, pinned: bool) -> Result<(), Error> {
        let mut map = self.map(id, q)?.ok_or(Error::Corrupt)?;
        if !map.complete() {
            return Err(Error::Corrupt);
        }
        map.pinned = pinned;
        self.save_map(id, q, &map)?;
        self.index(store, id, q, &map)
    }
    pub fn discard(&self, store: &Store, id: &TrackId, q: Quality) -> Result<(), Error> {
        for path in [self.path(id, q), self.sidecar(id, q)] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let id = id.to_string();
        let q = q.key().to_owned();
        store.write(move |c| {
            c.execute(
                "DELETE FROM cache WHERE id=?1 AND quality=?2",
                params![id, q],
            )?;
            Ok(())
        })
    }
    fn make_room(
        &self,
        store: &Store,
        incoming: u64,
        current: &TrackId,
        q: Quality,
    ) -> Result<(), Error> {
        let leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        let c = store.read()?;
        let mut total = u64::try_from(c.query_row(
            "SELECT COALESCE(SUM(size),0) FROM cache",
            [],
            |r| r.get::<_, i64>(0),
        )?)
        .map_err(|_| Error::Corrupt)?;
        let mut stmt=c.prepare("SELECT id,quality,size FROM cache WHERE pinned=0 AND NOT(id=?1 AND quality=?2) ORDER BY access,rowid")?;
        let rows = stmt
            .query_map(params![current.to_string(), q.key()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, quality, size) in rows {
            let size = u64::try_from(size).map_err(|_| Error::Corrupt)?;
            if total.saturating_add(incoming) <= self.limit {
                return Ok(());
            }
            let tid = id.parse::<TrackId>().map_err(|_| Error::Corrupt)?;
            let quality_enum = if quality == "high" {
                Quality::High
            } else {
                Quality::Standard
            };
            if leases.contains_key(&self.path(&tid, quality_enum)) {
                continue;
            }
            for path in [
                self.path(&tid, quality_enum),
                self.sidecar(&tid, quality_enum),
            ] {
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            store.write(move |c| {
                c.execute(
                    "DELETE FROM cache WHERE id=?1 AND quality=?2",
                    params![id, quality],
                )?;
                Ok(())
            })?;
            total = total.saturating_sub(size);
        }
        if total.saturating_add(incoming) > self.limit {
            Err(Error::CacheFull)
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "eg-store-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    #[test]
    fn active_leases_protect_lru_until_the_last_reader_drops() {
        let d = dir();
        let store = Store::open(&d).unwrap();
        let cache = Cache::open(&d.join("audio"), 4, &store).unwrap();
        let a = "yandex:1".parse().unwrap();
        let b = "yandex:2".parse().unwrap();
        cache
            .write_chunk(&store, &a, Quality::High, &mut RangeMap::new(4), 0, b"test")
            .unwrap();
        let first = cache.lease(&a, Quality::High);
        let second = cache.lease(&a, Quality::High);
        drop(first);
        assert!(matches!(
            cache.write_chunk(&store, &b, Quality::High, &mut RangeMap::new(4), 0, b"next"),
            Err(Error::CacheFull)
        ));
        assert!(!cache.map(&a, Quality::High).unwrap().unwrap().pinned);
        drop(second);
        cache
            .write_chunk(&store, &b, Quality::High, &mut RangeMap::new(4), 0, b"next")
            .unwrap();
        assert!(cache.lookup(&store, &a, Quality::High).unwrap().is_none());
        drop(store);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn lru_evicts_unpinned_and_full_cache_preserves_pins() {
        let d = dir();
        let s = Store::open(&d).unwrap();
        let c = Cache::open(&d.join("audio"), 8, &s).unwrap();
        let a = "yandex:1".parse().unwrap();
        let b = "vk:2".parse().unwrap();
        let other = "yandex:3".parse().unwrap();
        for id in [&a, &b] {
            c.write_chunk(&s, id, Quality::High, &mut RangeMap::new(4), 0, b"test")
                .unwrap();
        }
        c.pin(&s, &a, Quality::High, true).unwrap();
        c.write_chunk(&s, &other, Quality::High, &mut RangeMap::new(4), 0, b"next")
            .unwrap();
        assert!(c.lookup(&s, &a, Quality::High).unwrap().is_some());
        assert!(c.lookup(&s, &b, Quality::High).unwrap().is_none());
        c.pin(&s, &other, Quality::High, true).unwrap();
        assert!(matches!(
            c.write_chunk(&s, &b, Quality::High, &mut RangeMap::new(4), 0, b"full"),
            Err(Error::CacheFull)
        ));
        assert!(c.lookup(&s, &a, Quality::High).unwrap().is_some());
        drop(s);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn failed_audio_write_does_not_publish_a_range_or_database_row() {
        let d = dir();
        let s = Store::open(&d).unwrap();
        let c = Cache::open(&d.join("audio"), 100, &s).unwrap();
        let id = "yandex:1".parse().unwrap();
        fs::create_dir(c.path(&id, Quality::High)).unwrap();
        let mut map = RangeMap::new(4);
        assert!(
            c.write_chunk(&s, &id, Quality::High, &mut map, 0, b"test")
                .is_err()
        );
        assert!(map.ranges.is_empty());
        assert_eq!(
            s.read()
                .unwrap()
                .query_row("SELECT count(*) FROM cache", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            s.read()
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        drop(s);
        fs::remove_dir_all(d).unwrap();
    }
    #[test]
    fn holes_are_not_complete() {
        let mut m = RangeMap::new(100);
        m.add(50, 100).unwrap();
        assert_eq!(m.missing(20), Some((0, 20)));
        assert!(!m.complete());
        m.add(0, 50).unwrap();
        assert!(m.complete());
        assert!(m.valid(100));
        assert!(!m.valid(99));
    }
    #[test]
    fn cache_survives_reopen_and_pins_survive_database_loss() {
        let d = dir();
        let id = "yandex:123".parse().unwrap();
        {
            let s = Store::open(&d).unwrap();
            let c = Cache::open(&d.join("audio"), 100, &s).unwrap();
            let mut m = RangeMap::new(4);
            c.write_chunk(&s, &id, Quality::High, &mut m, 0, b"test")
                .unwrap();
            c.pin(&s, &id, Quality::High, true).unwrap();
        }
        fs::write(d.join("metadata.sqlite"), b"broken database").unwrap();
        let s = Store::open(&d).unwrap();
        let c = Cache::open(&d.join("audio"), 100, &s).unwrap();
        assert!(c.lookup(&s, &id, Quality::High).unwrap().is_some());
        assert!(c.map(&id, Quality::High).unwrap().unwrap().pinned);
        fs::write(c.sidecar(&id, Quality::High), b"bad map").unwrap();
        assert!(c.lookup(&s, &id, Quality::High).unwrap().is_none());
        assert!(!c.path(&id, Quality::High).exists());
        drop(s);
        fs::remove_dir_all(d).unwrap();
    }
}

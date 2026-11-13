// SPDX-License-Identifier: GPL-3.0-or-later
//! Copy-and-verify relocation journal stored outside the movable data directory.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const TEMP_DIR: &str = ".eagle-relocation";
const TEMP_MARKER: &str = ".eagle-owned";
const TEMP_MARKER_CONTENTS: &[u8] = b"eagle relocation temporary directory v1\n";

fn is_relocation_temp(path: &Path) -> io::Result<bool> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Ok(false);
    }
    let marker = path.join(TEMP_MARKER);
    if !fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file())
        || fs::read(&marker)? != TEMP_MARKER_CONTENTS
    {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == TEMP_MARKER {
            continue;
        }
        let name = entry.file_name();
        let Some((pid, sequence)) = name
            .to_str()
            .and_then(|name| name.strip_prefix(".eagle-relocation-"))
            .and_then(|name| name.split_once('-'))
        else {
            return Ok(false);
        };
        if pid.is_empty()
            || sequence.is_empty()
            || !pid.bytes().all(|b| b.is_ascii_digit())
            || !sequence.bytes().all(|b| b.is_ascii_digit())
            || !entry.file_type()?.is_file()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Exclusive ownership of a relocation destination without managed content.
/// The lease must outlive every writer using the destination.
#[derive(Debug)]
pub struct StorageLease {
    root: PathBuf,
    _lock: File,
}
impl StorageLease {
    pub fn claim_empty(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let lock = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(root.join("desktop.lock"))?;
        lock.try_lock()
            .map_err(|e| io::Error::other(e.to_string()))?;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_name() == TEMP_DIR && is_relocation_temp(&entry.path())? {
                continue;
            }
            if !matches!(
                entry.file_name().to_str(),
                Some("desktop.lock" | "desktop.port")
            ) || !entry.file_type()?.is_file()
            {
                return Err(io::Error::other("destination is not empty"));
            }
        }
        Ok(Self { root, _lock: lock })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    source: PathBuf,
    destination: PathBuf,
    complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActivePath {
    path: PathBuf,
}

/// Proof that the process has released its last Store and Cache handles.
/// The caller must also hold the application single-instance lock.
pub struct MovePermit(PathBuf);
pub fn quiesce(
    store: std::sync::Arc<crate::Store>,
    cache: std::sync::Arc<crate::Cache>,
) -> Result<MovePermit, (std::sync::Arc<crate::Store>, std::sync::Arc<crate::Cache>)> {
    let store = match std::sync::Arc::try_unwrap(store) {
        Ok(store) => store,
        Err(store) => return Err((store, cache)),
    };
    match std::sync::Arc::try_unwrap(cache) {
        Ok(cache) => {
            let readers_remain = !cache
                .leases
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty();
            let source = store.path.parent().and_then(|p| p.canonicalize().ok());
            let cache_source = cache.root.parent().and_then(|p| p.canonicalize().ok());
            if readers_remain || source.is_none() || source != cache_source {
                return Err((std::sync::Arc::new(store), std::sync::Arc::new(cache)));
            }
            drop(cache);
            drop(store);
            Ok(MovePermit(source.unwrap_or_default()))
        }
        Err(cache) => Err((std::sync::Arc::new(store), cache)),
    }
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // MoveFileExW replaces the destination and waits for the move to be flushed.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sync_dir(_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(_path)?.sync_all()?;
    Ok(())
}
fn persist(path: &Path, journal: &Journal) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("journal needs a parent"))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("tmp");
    let mut file = File::create(&temp)?;
    serde_json::to_writer(&mut file, journal)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    replace(&temp, path)?;
    sync_dir(parent)
}
fn digest(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().into())
}
fn copy_tree(source: &Path, destination: &Path, temp_root: &Path, top: bool) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if top && entry.file_name() == TEMP_DIR {
            if is_relocation_temp(&entry.path())? {
                continue;
            }
            return Err(io::Error::other(
                "source temporary directory is not owned by Eagle",
            ));
        }
        if top
            && matches!(
                entry.file_name().to_str(),
                Some("desktop.lock" | "desktop.port" | "desktop.port.tmp")
            )
        {
            continue;
        }
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if let Ok(metadata) = fs::symlink_metadata(&destination)
                && !metadata.file_type().is_dir()
            {
                return Err(io::Error::other(
                    "destination directory changed during relocation",
                ));
            }
            copy_tree(&source, &destination, temp_root, false)?;
        } else if kind.is_file() {
            if let Ok(metadata) = fs::symlink_metadata(&destination) {
                if metadata.file_type().is_file() && digest(&source)? == digest(&destination)? {
                    continue;
                }
                return Err(io::Error::other("destination changed during relocation"));
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let (temp, mut output) = loop {
                let temp = temp_root.join(format!(
                    ".eagle-relocation-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                match OpenOptions::new().write(true).create_new(true).open(&temp) {
                    Ok(output) => break (temp, output),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            };
            let result = (|| {
                let mut input = File::open(&source)?;
                io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                drop(output);
                if digest(&source)? != digest(&temp)? {
                    return Err(io::Error::other("relocation checksum mismatch"));
                }
                fs::rename(&temp, &destination)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result?;
        } else {
            return Err(io::Error::other(
                "relocation does not follow links or special files",
            ));
        }
    }
    sync_dir(destination)
}
/// Run with all writers stopped. The journal and source remain intact after errors.
/// Destination must be a new, empty directory, outside source and bootstrap.
pub fn relocate(
    permit: MovePermit,
    bootstrap: &Path,
    source: &Path,
    destination: &Path,
    lease: Option<&StorageLease>,
) -> io::Result<()> {
    if source.canonicalize()? != permit.0 {
        return Err(io::Error::other("move permit does not match source"));
    }
    relocate_with_lease(bootstrap, source, destination, lease, |_| Ok(()))
}
#[cfg(test)]
fn relocate_quiesced(bootstrap: &Path, source: &Path, destination: &Path) -> io::Result<()> {
    relocate_with_fault(bootstrap, source, destination, |_| Ok(()))
}

#[cfg(test)]
fn relocate_with_fault(
    bootstrap: &Path,
    source: &Path,
    destination: &Path,
    fault: impl Fn(&str) -> io::Result<()>,
) -> io::Result<()> {
    relocate_with_lease(bootstrap, source, destination, None, fault)
}
fn relocate_with_lease(
    bootstrap: &Path,
    source: &Path,
    destination: &Path,
    lease: Option<&StorageLease>,
    fault: impl Fn(&str) -> io::Result<()>,
) -> io::Result<()> {
    let journal_path = &bootstrap.join("relocation.json");
    let active_path = bootstrap.join("active.json");
    let source = source.canonicalize()?;
    if active_path.exists() {
        let active: ActivePath = serde_json::from_reader(File::open(&active_path)?)?;
        let current = active.path.canonicalize()?;
        if current != source && current != destination.canonicalize()? {
            return Err(io::Error::other("active storage does not match relocation"));
        }
    }
    let journal_parent = journal_path
        .parent()
        .ok_or_else(|| io::Error::other("journal needs a parent"))?
        .canonicalize()?;
    let destination = destination.canonicalize()?;
    if lease.is_some_and(|lease| lease.root != destination) {
        return Err(io::Error::other("destination lease does not match"));
    }
    if source.starts_with(&destination)
        || destination.starts_with(&source)
        || journal_parent.starts_with(&source)
        || journal_parent.starts_with(&destination)
    {
        return Err(io::Error::other("relocation paths overlap"));
    }
    let mut journal = if journal_path.exists() {
        let journal: Journal = serde_json::from_reader(File::open(journal_path)?)?;
        if journal.source != source || journal.destination != destination {
            return Err(io::Error::other("another relocation is recorded"));
        }
        journal
    } else {
        let owned = lease.is_some_and(|lease| lease.root == destination);
        if fs::read_dir(&destination)?.any(|entry| {
            entry.map_or(true, |entry| {
                !(entry.file_name() == TEMP_DIR
                    && is_relocation_temp(&entry.path()).unwrap_or(false))
                    && (!owned
                        || !matches!(
                            entry.file_name().to_str(),
                            Some("desktop.lock" | "desktop.port")
                        ))
            })
        }) {
            return Err(io::Error::other("destination is not empty"));
        }
        let journal = Journal {
            source: source.clone(),
            destination: destination.clone(),
            complete: false,
        };
        persist(journal_path, &journal)?;
        journal
    };
    fault("journal")?;
    {
        let temp_root = destination.join(TEMP_DIR);
        match fs::create_dir(&temp_root) {
            Ok(()) => {
                let mut marker = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(temp_root.join(TEMP_MARKER))?;
                marker.write_all(TEMP_MARKER_CONTENTS)?;
                marker.sync_all()?;
                sync_dir(&temp_root)?;
            }
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && is_relocation_temp(&temp_root)? => {}
            Err(error) => return Err(error),
        }
        copy_tree(&source, &destination, &temp_root, true)?;
        if !is_relocation_temp(&temp_root)? {
            return Err(io::Error::other("relocation temporary directory changed"));
        }
        if let Err(error) = fs::remove_dir_all(&temp_root) {
            log::warn!("could not remove relocation temporary directory: {error}");
        }
        fault("copied")?;
        if !journal.complete {
            journal.complete = true;
            persist(journal_path, &journal)?;
        }
    }
    fault("verified")?;
    let temp = bootstrap.join("active.tmp");
    let mut file = File::create(&temp)?;
    serde_json::to_writer(&mut file, &ActivePath { path: destination })?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    replace(&temp, &active_path)?;
    fault("switched")?;
    sync_dir(bootstrap)?;
    fault("synced")?;
    fs::remove_file(journal_path)?;
    sync_dir(bootstrap)?;
    Ok(())
}

/// Resume an interrupted move before opening Store or Cache handles.
pub fn recover(bootstrap: &Path) -> io::Result<Option<PathBuf>> {
    let path = bootstrap.join("relocation.json");
    let journal = match File::open(&path) {
        Ok(file) => serde_json::from_reader::<_, Journal>(file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return active_path(bootstrap),
        Err(error) => return Err(error),
    };
    let active = active_path(bootstrap)?;
    let selected = match active {
        Some(path) if path == journal.destination => journal.destination,
        Some(path) if path == journal.source => journal.source,
        None => journal.source,
        Some(_) => return Err(io::Error::other("active storage does not match relocation")),
    };
    // A failed cleanup must not prevent opening the still-authoritative root.
    if fs::remove_file(path).is_ok() {
        let _ = sync_dir(bootstrap);
    }
    Ok(Some(selected))
}

/// Resolve the active data root after recovery has completed.
pub fn active_path(bootstrap: &Path) -> io::Result<Option<PathBuf>> {
    match File::open(bootstrap.join("active.json")) {
        Ok(file) => Ok(Some(serde_json::from_reader::<_, ActivePath>(file)?.path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relocation_never_writes_outside_destination() {
        let root =
            std::env::temp_dir().join(format!("eg-relocate-contained-{}", std::process::id()));
        let (boot, src, parent) = (root.join("boot"), root.join("src"), root.join("parent"));
        let dst = parent.join("dest");
        for dir in [&boot, &src, &dst] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(src.join("audio"), b"source").unwrap();
        assert!(
            relocate_with_fault(&boot, &src, &dst, |point| {
                if point == "copied" {
                    Err(io::Error::other("injected fault"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        assert!(dst.join("audio").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rerun_after_crash_ignores_leftover_temp_dir() {
        let root =
            std::env::temp_dir().join(format!("eg-relocate-leftover-{}", std::process::id()));
        let (boot, src, dst) = (root.join("boot"), root.join("src"), root.join("dst"));
        for dir in [&boot, &src, &dst] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(src.join("audio"), b"source").unwrap();
        fs::create_dir(src.join(".eagle-relocation")).unwrap();
        fs::write(
            src.join(".eagle-relocation").join(TEMP_MARKER),
            TEMP_MARKER_CONTENTS,
        )
        .unwrap();
        fs::create_dir(dst.join(".eagle-relocation")).unwrap();
        fs::write(
            dst.join(".eagle-relocation").join(TEMP_MARKER),
            TEMP_MARKER_CONTENTS,
        )
        .unwrap();
        fs::write(
            dst.join(".eagle-relocation/.eagle-relocation-1-0"),
            b"partial",
        )
        .unwrap();
        let lease = StorageLease::claim_empty(&dst).unwrap();
        relocate_with_lease(&boot, &src, &dst, Some(&lease), |_| Ok(())).unwrap();
        assert!(!dst.join(".eagle-relocation").exists());
        assert_eq!(fs::read(dst.join("audio")).unwrap(), b"source");
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrecognized_temp_directories_are_preserved() {
        let root =
            std::env::temp_dir().join(format!("eg-relocate-foreign-temp-{}", std::process::id()));
        let (boot, src, dst) = (root.join("boot"), root.join("src"), root.join("dst"));
        for dir in [&boot, &src, &dst] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(src.join("audio"), b"source").unwrap();
        let foreign = dst.join(TEMP_DIR);
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("personal"), b"keep").unwrap();
        assert!(StorageLease::claim_empty(&dst).is_err());
        assert!(relocate_quiesced(&boot, &src, &dst).is_err());
        assert_eq!(fs::read(foreign.join("personal")).unwrap(), b"keep");
        fs::write(foreign.join(TEMP_MARKER), TEMP_MARKER_CONTENTS).unwrap();
        assert!(StorageLease::claim_empty(&dst).is_err());
        assert!(relocate_quiesced(&boot, &src, &dst).is_err());
        assert_eq!(fs::read(foreign.join("personal")).unwrap(), b"keep");
        fs::remove_dir_all(&foreign).unwrap();
        fs::create_dir(src.join(TEMP_DIR)).unwrap();
        fs::write(src.join(TEMP_DIR).join("personal"), b"keep").unwrap();
        assert!(relocate_quiesced(&boot, &src, &dst).is_err());
        assert_eq!(
            fs::read(src.join(TEMP_DIR).join("personal")).unwrap(),
            b"keep"
        );
        assert!(active_path(&boot).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn destination_without_writable_parent_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("eg-relocate-readonly-{}", std::process::id()));
        let (boot, src, parent) = (root.join("boot"), root.join("src"), root.join("parent"));
        let dst = parent.join("dest");
        for dir in [&boot, &src, &dst] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(src.join("audio"), b"source").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        let result = relocate_quiesced(&boot, &src, &dst);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert_eq!(fs::read(dst.join("audio")).unwrap(), b"source");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn destination_lease_is_required_and_survives_switch() {
        let base = std::env::temp_dir().join(format!("eagle-lease-{}", std::process::id()));
        let source = base.join("source");
        let destination = base.join("destination");
        let bootstrap = base.join("bootstrap");
        for path in [&source, &destination, &bootstrap] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(source.join("data"), b"data").unwrap();
        fs::write(source.join("desktop.port"), b"old").unwrap();
        let lease = StorageLease::claim_empty(&destination).unwrap();
        fs::write(destination.join("desktop.port"), b"new").unwrap();
        assert!(relocate_with_lease(&bootstrap, &source, &destination, None, |_| Ok(())).is_err());
        relocate_with_lease(&bootstrap, &source, &destination, Some(&lease), |_| Ok(())).unwrap();
        assert_eq!(fs::read(destination.join("desktop.port")).unwrap(), b"new");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination.join("desktop.lock"))
            .unwrap();
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(lease);
        assert!(lock.try_lock().is_ok());
        drop(lock);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn destination_lease_allows_stale_reserved_files_and_rejects_content() {
        let base = std::env::temp_dir().join(format!("eagle-lease-foreign-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("desktop.lock"), b"stale").unwrap();
        fs::write(base.join("desktop.port"), b"old").unwrap();
        let lease = StorageLease::claim_empty(&base).unwrap();
        assert!(StorageLease::claim_empty(&base).is_err());
        drop(lease);
        assert!(StorageLease::claim_empty(&base).is_ok());
        fs::write(base.join("metadata.sqlite"), b"keep").unwrap();
        assert!(StorageLease::claim_empty(&base).is_err());
        assert_eq!(fs::read(base.join("metadata.sqlite")).unwrap(), b"keep");
        assert_eq!(fs::read(base.join("desktop.lock")).unwrap(), b"stale");
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn permit_requires_all_cache_readers_and_handles_to_be_released() {
        let root = std::env::temp_dir().join(format!("eg-relocate-permit-{}", std::process::id()));
        let store = std::sync::Arc::new(crate::Store::open(&root).unwrap());
        let cache =
            std::sync::Arc::new(crate::Cache::open(&root.join("audio"), 100, &store).unwrap());
        let lease = cache.lease(&"yandex:1".parse().unwrap(), eagle_core::Quality::High);
        let (store, cache) = match quiesce(store, cache) {
            Err(handles) => handles,
            Ok(_) => panic!("live reader accepted"),
        };
        let extra = store.clone();
        let (store, cache) = match quiesce(store, cache) {
            Err(handles) => handles,
            Ok(_) => panic!("live store accepted"),
        };
        drop(extra);
        drop(lease);
        let permit = quiesce(store, cache).ok().unwrap();
        let other = root.join("other");
        let dest = root.join("dest");
        let boot = root.join("boot");
        for dir in [&other, &dest, &boot] {
            fs::create_dir_all(dir).unwrap();
        }
        assert!(relocate(permit, &boot, &other, &dest, None).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn faults_at_real_boundaries_keep_the_correct_active_root() {
        for stage in ["journal", "copied", "verified", "switched", "synced"] {
            let root = std::env::temp_dir()
                .join(format!("eg-relocate-fault-{}-{stage}", std::process::id()));
            let (boot, src, dst) = (root.join("boot"), root.join("src"), root.join("dst"));
            for dir in [&boot, &src, &dst] {
                fs::create_dir_all(dir).unwrap();
            }
            fs::write(src.join("audio"), b"source").unwrap();
            let result = relocate_with_fault(&boot, &src, &dst, |point| {
                if point == stage {
                    Err(io::Error::other("injected fault"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "{stage}");
            let expected = if matches!(stage, "switched" | "synced") {
                &dst
            } else {
                &src
            };
            assert_eq!(
                recover(&boot).unwrap(),
                Some(expected.canonicalize().unwrap()),
                "{stage}"
            );
            assert_eq!(fs::read(src.join("audio")).unwrap(), b"source", "{stage}");
            if matches!(stage, "copied" | "verified" | "switched" | "synced") {
                assert_eq!(fs::read(dst.join("audio")).unwrap(), b"source", "{stage}");
            }
            assert!(!boot.join("relocation.json").exists());
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn interrupted_copy_resumes_and_switch_keeps_source() {
        let root = std::env::temp_dir().join(format!("eg-relocate-{}", std::process::id()));
        let (boot, src, dst) = (root.join("boot"), root.join("src"), root.join("dst"));
        for dir in [&boot, &src, &dst] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(src.join("audio"), b"original").unwrap();
        fs::write(dst.join("audio"), b"damaged").unwrap();
        persist(
            &boot.join("relocation.json"),
            &Journal {
                source: src.canonicalize().unwrap(),
                destination: dst.canonicalize().unwrap(),
                complete: false,
            },
        )
        .unwrap();
        assert!(relocate_quiesced(&boot, &src, &dst).is_err());
        assert_eq!(recover(&boot).unwrap(), Some(src.canonicalize().unwrap()));
        assert_eq!(fs::read(dst.join("audio")).unwrap(), b"damaged");
        assert_eq!(fs::read(src.join("audio")).unwrap(), b"original");
        fs::remove_file(dst.join("audio")).unwrap();
        relocate_quiesced(&boot, &src, &dst).unwrap();
        assert_eq!(
            active_path(&boot).unwrap(),
            Some(dst.canonicalize().unwrap())
        );
        assert_eq!(fs::read(dst.join("audio")).unwrap(), b"original");
        assert_eq!(fs::read(src.join("audio")).unwrap(), b"original");
        // A crash after journal completion but before active-path replacement is recoverable.
        persist(
            &boot.join("relocation.json"),
            &Journal {
                source: src.canonicalize().unwrap(),
                destination: dst.canonicalize().unwrap(),
                complete: true,
            },
        )
        .unwrap();
        fs::remove_file(boot.join("active.json")).unwrap();
        assert_eq!(recover(&boot).unwrap(), Some(src.canonicalize().unwrap()));
        // Re-establish an active destination to test a second successful move.
        let file = File::create(boot.join("active.json")).unwrap();
        serde_json::to_writer(
            file,
            &ActivePath {
                path: dst.canonicalize().unwrap(),
            },
        )
        .unwrap();
        let next = root.join("next");
        fs::create_dir_all(&next).unwrap();
        relocate_quiesced(&boot, &dst, &next).unwrap();
        assert_eq!(
            active_path(&boot).unwrap(),
            Some(next.canonicalize().unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }
}

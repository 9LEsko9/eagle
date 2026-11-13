// SPDX-License-Identifier: GPL-3.0-or-later
//! Storage move plumbing; caller stops and joins every source-root writer first.
use eagle_store::{Cache, Store, relocation};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) enum MoveOutcome {
    /// The handles remain usable; an outstanding owner or reader prevented a move.
    Busy {
        store: Arc<Store>,
        cache: Arc<Cache>,
    },
    /// Reopen this root. `error` reports a move failure even when a switch happened.
    Reopen {
        root: PathBuf,
        error: Option<io::Error>,
    },
}

/// The process single-instance lock must remain held until the new Worker is ready.
pub(crate) fn relocate(
    store: Arc<Store>,
    cache: Arc<Cache>,
    source: &Path,
    destination: &Path,
    bootstrap: &Path,
    lease: Option<&eagle_store::relocation::StorageLease>,
) -> io::Result<MoveOutcome> {
    let permit = match relocation::quiesce(store, cache) {
        Ok(permit) => permit,
        Err((store, cache)) => return Ok(MoveOutcome::Busy { store, cache }),
    };
    let error = relocation::relocate(permit, bootstrap, source, destination, lease).err();
    // Recovery decides whether the durable switch happened. Never guess on read failure.
    let root = relocation::recover(bootstrap)?.unwrap_or(source.canonicalize()?);
    Ok(MoveOutcome::Reopen { root, error })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_move_reopens_source_and_keeps_destination() {
        let root =
            std::env::temp_dir().join(format!("eg-move-helper-failed-{}", std::process::id()));
        let (source, destination, bootstrap) = (
            root.join("source"),
            root.join("destination"),
            root.join("bootstrap"),
        );
        for dir in [&source, &destination, &bootstrap] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(destination.join("existing"), b"keep").unwrap();
        let store = Arc::new(Store::open(&source).unwrap());
        let cache = Arc::new(Cache::open(&source.join("audio"), 100, &store).unwrap());
        match relocate(store, cache, &source, &destination, &bootstrap, None).unwrap() {
            MoveOutcome::Reopen {
                root,
                error: Some(_),
            } => assert_eq!(root, source.canonicalize().unwrap()),
            _ => panic!("failed move did not preserve source"),
        }
        assert_eq!(
            std::fs::read(destination.join("existing")).unwrap(),
            b"keep"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn busy_keeps_handles_and_success_reopens_destination() {
        let root = std::env::temp_dir().join(format!("eg-move-helper-{}", std::process::id()));
        let (source, destination, bootstrap) = (
            root.join("source"),
            root.join("destination"),
            root.join("bootstrap"),
        );
        for dir in [&source, &destination, &bootstrap] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let store = Arc::new(Store::open(&source).unwrap());
        let cache = Arc::new(Cache::open(&source.join("audio"), 100, &store).unwrap());
        let extra = store.clone();
        let (store, cache) =
            match relocate(store, cache, &source, &destination, &bootstrap, None).unwrap() {
                MoveOutcome::Busy { store, cache } => (store, cache),
                _ => panic!("unexpected move while in use"),
            };
        assert!(Arc::ptr_eq(&store, &extra));
        drop(extra);
        match relocate(store, cache, &source, &destination, &bootstrap, None).unwrap() {
            MoveOutcome::Reopen { root, error: None } => {
                assert_eq!(root, destination.canonicalize().unwrap())
            }
            _ => panic!("move did not complete"),
        }
        assert!(source.join("metadata.sqlite").exists());
        assert!(destination.join("metadata.sqlite").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

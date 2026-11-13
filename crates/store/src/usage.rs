// SPDX-License-Identifier: GPL-3.0-or-later
//! Logical file sizes in the managed directory, including staging and SQLite.
use crate::{Cache, Error, Store};
use std::{fs, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUsage {
    pub cache: u64,
    pub saved: u64,
    pub staging: u64,
    pub other: u64,
}
impl StorageUsage {
    pub fn total(self) -> u64 {
        self.cache + self.saved + self.staging + self.other
    }
}
impl Cache {
    pub fn storage_usage(&self, store: &Store) -> Result<StorageUsage, Error> {
        let mut usage = StorageUsage::default();
        let root = store.path.parent().ok_or(Error::Corrupt)?;
        let connection = store.read()?;
        let mut statement = connection.prepare("SELECT path,pinned FROM cache")?;
        let audio = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
            .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
        fn scan(
            root: &Path,
            audio: &std::collections::HashMap<String, bool>,
            usage: &mut StorageUsage,
        ) -> Result<(), Error> {
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    scan(&entry.path(), audio, usage)?;
                } else {
                    let bytes = fs::symlink_metadata(entry.path())?.len();
                    let path = entry.path().to_string_lossy().into_owned();
                    let bucket = match audio.get(&path) {
                        Some(true) => &mut usage.saved,
                        Some(false) => &mut usage.cache,
                        None if entry
                            .path()
                            .components()
                            .any(|c| c.as_os_str() == "downloads") =>
                        {
                            &mut usage.staging
                        }
                        None => &mut usage.other,
                    };
                    *bucket = bucket.checked_add(bytes).ok_or(Error::Corrupt)?;
                }
            }
            Ok(())
        }
        scan(root, &audio, &mut usage)?;
        Ok(usage)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accounting_includes_database_and_staging_without_following_links() {
        let root = std::env::temp_dir().join(format!("eagle-usage-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let cache = Cache::open(&root.join("audio"), 1024, &store).unwrap();
        fs::create_dir_all(root.join("downloads")).unwrap();
        fs::write(root.join("downloads/partial"), b"staging").unwrap();
        fs::write(root.join("misc"), b"other").unwrap();
        let usage = cache.storage_usage(&store).unwrap();
        assert_eq!(usage.staging, 7);
        assert!(usage.other > 5);
        assert_eq!(usage.total(), usage.other + 7);
        drop(cache);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}

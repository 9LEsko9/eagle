// SPDX-License-Identifier: GPL-3.0-or-later
//! Durable offline access decisions, scoped to an account and track.
use crate::{Error, Store};
use rusqlite::OptionalExtension;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccessState {
    #[default]
    Unknown,
    Confirmed,
    Revoked,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessRecord {
    pub state: AccessState,
    /// Application-defined reason, never a raw provider response.
    pub reason: Option<String>,
    pub last_checked: Option<i64>,
}
impl AccessRecord {
    pub fn access_due(&self, now: i64) -> bool {
        self.last_checked
            .is_none_or(|last| now.saturating_sub(last) >= 86_400)
    }
    pub fn allows_playback(&self) -> bool {
        self.state != AccessState::Revoked
    }
}

impl Store {
    pub fn access(&self, account: &str, track: &str) -> Result<AccessRecord, Error> {
        if account.is_empty() || track.is_empty() {
            return Ok(AccessRecord::default());
        }
        let record: Option<(String, Option<String>, i64)> = self
            .read()?
            .query_row(
                "SELECT state,reason,last_checked FROM access WHERE account=?1 AND track=?2",
                rusqlite::params![account, track],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match record {
            None => Ok(AccessRecord::default()),
            Some((state, reason, last_checked)) => Ok(AccessRecord {
                state: match state.as_str() {
                    "unknown" => AccessState::Unknown,
                    "confirmed" => AccessState::Confirmed,
                    "revoked" => AccessState::Revoked,
                    _ => return Err(Error::Corrupt),
                },
                reason,
                last_checked: Some(last_checked),
            }),
        }
    }

    /// Unknown rights permit cached playback only for a durable owner.
    pub fn allows_local_playback(&self, account: &str, track: &str) -> Result<bool, Error> {
        match self.access(account, track)?.state {
            AccessState::Revoked => Ok(false),
            AccessState::Confirmed => Ok(true),
            AccessState::Unknown => self.owns_audio(account, track),
        }
    }

    pub fn owns_audio(&self, account: &str, track: &str) -> Result<bool, Error> {
        if account.is_empty() || track.is_empty() {
            return Ok(false);
        }
        Ok(self.read()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM offline_owners WHERE account=?1 AND track=?2)",
            rusqlite::params![account, track],
            |row| row.get(0),
        )?)
    }

    /// Unowned cached bytes require online authorization before reuse.
    pub fn requires_shared_authorization(&self, account: &str, track: &str) -> Result<bool, Error> {
        Ok(!self.owns_audio(account, track)?)
    }

    /// Call only after the provider has authorized this account for this track.
    pub fn record_audio_owner(&self, account: &str, track: &str) -> Result<(), Error> {
        if account.is_empty() || track.is_empty() {
            return Err(Error::Corrupt);
        }
        let (account, track) = (account.to_owned(), track.to_owned());
        self.write(move |db| {
            db.execute(
                "INSERT INTO offline_owners(account,track) VALUES(?1,?2) ON CONFLICT DO NOTHING",
                rusqlite::params![account, track],
            )?;
            Ok(())
        })
    }

    /// Remember an inconclusive check without changing the last confirmed decision.
    pub fn note_access_attempt(
        &self,
        account: &str,
        track: &str,
        checked_at: i64,
    ) -> Result<(), Error> {
        if account.is_empty() || track.is_empty() || checked_at < 0 {
            return Err(Error::Corrupt);
        }
        let (account, track) = (account.to_owned(), track.to_owned());
        self.write(move |connection| {
            connection.execute("INSERT INTO access(account,track,state,reason,last_checked) VALUES(?1,?2,'unknown',NULL,?3) ON CONFLICT(account,track) DO UPDATE SET last_checked=MAX(last_checked,excluded.last_checked)", rusqlite::params![account,track,checked_at])?;
            Ok(())
        })
    }

    /// Record only an explicit provider confirmation, never a network or authentication error.
    pub fn record_access(
        &self,
        account: &str,
        track: &str,
        state: AccessState,
        reason: Option<String>,
        checked_at: i64,
    ) -> Result<(), Error> {
        let state = match state {
            AccessState::Confirmed => "confirmed",
            AccessState::Revoked => "revoked",
            AccessState::Unknown => return Err(Error::Corrupt),
        };
        if account.is_empty() || track.is_empty() || checked_at < 0 {
            return Err(Error::Corrupt);
        }
        let account = account.to_owned();
        let track = track.to_owned();
        self.write(move |connection| {
            connection.execute(
                "INSERT INTO access(account,track,state,reason,last_checked) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(account,track) DO UPDATE SET state=excluded.state,reason=excluded.reason,last_checked=excluded.last_checked WHERE excluded.last_checked>=access.last_checked",
                rusqlite::params![account, track, state, reason, checked_at],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_pinned_file_without_download_owner_remains_unowned_after_upgrade() {
        let root =
            std::env::temp_dir().join(format!("eg-access-legacy-pin-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        store.put_setting("account_uid:yandex", "a".into()).unwrap();
        let cache = crate::Cache::open(&root.join("audio"), 100, &store).unwrap();
        let track = "yandex:1".parse().unwrap();
        let mut map = crate::RangeMap::new(1);
        cache
            .write_chunk(&store, &track, eagle_core::Quality::High, &mut map, 0, b"x")
            .unwrap();
        cache
            .pin(&store, &track, eagle_core::Quality::High, true)
            .unwrap();
        drop(cache);
        drop(store);
        let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        db.execute_batch(
            "DROP TABLE radio_play_intents; DROP TABLE offline_owners; PRAGMA user_version=6;",
        )
        .unwrap();
        drop(db);
        let store = Store::open(&root).unwrap();
        let cache = crate::Cache::open(&root.join("audio"), 100, &store).unwrap();
        assert!(
            cache
                .map(&track, eagle_core::Quality::High)
                .unwrap()
                .unwrap()
                .pinned
        );
        assert!(!store.allows_local_playback("a", "yandex:1").unwrap());
        drop(cache);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn v6_upgrade_imports_only_completed_and_audio_ready_owners() {
        let root = std::env::temp_dir().join(format!("eg-access-v6-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = "yandex:1".parse().unwrap();
        for account in ["saved", "ready", "running"] {
            store
                .enqueue_download(account, &track, eagle_core::Quality::High)
                .unwrap();
            let job = store.claim_download(account).unwrap().unwrap();
            match account {
                "saved" => {
                    store.finish_saved_download(&job).unwrap();
                }
                "ready" => {
                    store.finish_download(&job, true).unwrap();
                }
                _ => (),
            }
        }
        drop(store);
        let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        db.execute_batch(
            "DROP TABLE radio_play_intents; DROP TABLE offline_owners; PRAGMA user_version=6;",
        )
        .unwrap();
        drop(db);
        let store = Store::open(&root).unwrap();
        assert!(store.owns_audio("saved", "yandex:1").unwrap());
        assert!(store.owns_audio("ready", "yandex:1").unwrap());
        assert!(!store.owns_audio("running", "yandex:1").unwrap());
        assert!(!store.owns_audio("other", "yandex:1").unwrap());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shared_saved_file_does_not_grant_another_account_access() {
        let root = std::env::temp_dir().join(format!("eg-access-ownership-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = "yandex:1".parse().unwrap();
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        store.record_audio_owner("a", "yandex:1").unwrap();
        store
            .enqueue_download("a", &track, eagle_core::Quality::High)
            .unwrap();
        let job = store.claim_download("a").unwrap().unwrap();
        store.finish_download(&job, true).unwrap();
        assert!(store.allows_local_playback("a", "yandex:1").unwrap());
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        let running = store.claim_download("a").unwrap().unwrap();
        assert_eq!(running.state, crate::DownloadState::AudioReady);
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        store
            .control_download("a", running.id, crate::DownloadState::Paused)
            .unwrap();
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        store
            .control_download("a", running.id, crate::DownloadState::Cancelled)
            .unwrap();
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        store.recover_downloads("a").unwrap();
        assert!(!store.allows_local_playback("b", "yandex:1").unwrap());
        store
            .record_access("b", "yandex:1", AccessState::Confirmed, None, 1)
            .unwrap();
        assert!(store.allows_local_playback("b", "yandex:1").unwrap());
        store
            .record_access("a", "yandex:1", AccessState::Revoked, None, 1)
            .unwrap();
        assert!(!store.allows_local_playback("a", "yandex:1").unwrap());
        store
            .record_access("a", "yandex:1", AccessState::Confirmed, None, 2)
            .unwrap();
        assert!(store.allows_local_playback("a", "yandex:1").unwrap());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn access_is_durable_track_scoped_and_due_daily() {
        let root = std::env::temp_dir().join(format!("eg-access-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        assert!(store.access("a", "t").unwrap().allows_playback());
        assert!(store.access("a", "t").unwrap().access_due(0));
        store.note_access_attempt("b", "t", 50).unwrap();
        assert!(!store.access("b", "t").unwrap().access_due(50));
        store
            .record_access(
                "a",
                "t",
                AccessState::Revoked,
                Some("unavailable".into()),
                100,
            )
            .unwrap();
        store
            .record_access("a", "u", AccessState::Confirmed, None, 100)
            .unwrap();
        assert!(
            store
                .record_access("a", "t", AccessState::Unknown, None, 200)
                .is_err()
        );
        drop(store);
        let store = Store::open(&root).unwrap();
        let record = store.access("a", "t").unwrap();
        assert!(!record.allows_playback());
        store.note_access_attempt("a", "t", 100).unwrap();
        assert!(!store.access("a", "t").unwrap().allows_playback());
        assert_eq!(record.reason.as_deref(), Some("unavailable"));
        assert!(!record.access_due(86_499));
        assert!(record.access_due(86_500));
        assert!(store.access("a", "u").unwrap().allows_playback());
        assert_eq!(store.access("b", "t").unwrap().state, AccessState::Unknown);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

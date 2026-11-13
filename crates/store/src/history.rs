// SPDX-License-Identifier: GPL-3.0-or-later
//! Local playback history, isolated by account and independent of provider availability.
use crate::{Error, Store, now};
use eagle_core::Track;
use rusqlite::params;

impl Store {
    pub fn record_play(&self, account: &str, track: &Track) -> Result<(), Error> {
        if account.is_empty() {
            return Ok(());
        }
        let account = account.to_owned();
        let json = serde_json::to_string(track).map_err(|_| Error::Corrupt)?;
        let track = track.id.to_string();
        self.write(move |c| {
            let transaction = c.transaction()?;
            let played_at = now();
            transaction.execute(
                "INSERT INTO history(account,played_at,json) VALUES(?1,?2,?3)",
                params![account, played_at, json],
            )?;
            transaction.execute(
                "INSERT INTO play_outbox(id,account,track,played_at,state) VALUES(lower(hex(randomblob(16))),?1,?2,?3,'pending')",
                params![account, track, played_at],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }
    pub fn history(&self, account: &str) -> Result<Vec<Track>, Error> {
        let connection = self.read()?;
        let mut query = connection
            .prepare("SELECT json FROM history WHERE account=?1 ORDER BY id DESC LIMIT 1000")?;
        query
            .query_map([account], |row| row.get::<_, String>(0))?
            .map(|json| serde_json::from_str(&json?).map_err(|_| Error::Corrupt))
            .collect()
    }
    pub fn saved_tracks(&self, account: &str) -> Result<Vec<Track>, Error> {
        let connection = self.read()?;
        let mut query = connection.prepare("SELECT DISTINCT tracks.json FROM downloads JOIN tracks ON tracks.id=downloads.track WHERE downloads.account=?1 AND downloads.state='saved' ORDER BY downloads.id DESC")?;
        query
            .query_map([account], |row| row.get::<_, String>(0))?
            .map(|json| serde_json::from_str(&json?).map_err(|_| Error::Corrupt))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn track() -> Track {
        serde_json::from_value(serde_json::json!({"id":"yandex:1", "title":"local", "artists":[], "album":null, "album_title":null, "duration_ms":10000, "cover":null, "availability":"Available"})).unwrap()
    }
    #[test]
    fn upgrades_complete_v4_without_losing_data() {
        let root = std::env::temp_dir().join(format!("eg-history-v4-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = track();
        store.put_track(&track).unwrap();
        store.put_setting("fixture", "retained".into()).unwrap();
        store.record_play("a", &track).unwrap();
        store
            .enqueue_download("a", &track.id, eagle_core::Quality::High)
            .unwrap();
        drop(store);
        let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        db.execute_batch("DROP TABLE access; DROP TABLE play_outbox; DROP TABLE offline_owners; PRAGMA user_version=4;")
            .unwrap();
        drop(db);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.history("a").unwrap()[0].id, track.id);
        assert_eq!(store.downloads("a").unwrap().len(), 1);
        assert_eq!(
            store.setting("fixture").unwrap().as_deref(),
            Some("retained")
        );
        assert_eq!(
            store.access("a", "yandex:1").unwrap().state,
            crate::AccessState::Unknown
        );
        assert!(store.play_events("a").unwrap().is_empty());
        let version: i64 = store
            .read()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 7);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn outbox_failure_rolls_back_history_insert() {
        let root = std::env::temp_dir().join(format!("eg-history-rollback-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        store
            .write(|db| {
                db.execute("DROP TABLE play_outbox", [])?;
                Ok(())
            })
            .unwrap();
        assert!(store.record_play("a", &track()).is_err());
        assert!(store.history("a").unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn history_keeps_repeated_plays_and_survives_restart_without_crossing_accounts() {
        let root = std::env::temp_dir().join(format!("eg-history-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track: Track = serde_json::from_value(serde_json::json!({"id":"yandex:1", "title":"local", "artists":[], "album":null, "album_title":null, "duration_ms":10000, "cover":null, "availability":"Available"})).unwrap();
        store.record_play("a", &track).unwrap();
        store.record_play("a", &track).unwrap();
        store.record_play("", &track).unwrap();
        assert!(store.history("b").unwrap().is_empty());
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.history("a").unwrap().len(), 2);
        assert_eq!(store.play_events("a").unwrap().len(), 2);
        assert!(store.play_events("b").unwrap().is_empty());
        assert!(store.history("").unwrap().is_empty());
        store.put_track(&track).unwrap();
        let quality = eagle_core::Quality::High;
        store.enqueue_download("a", &track.id, quality).unwrap();
        assert!(store.saved_tracks("a").unwrap().is_empty());
        let job = store.claim_download("a").unwrap().unwrap();
        store.finish_saved_download(&job).unwrap();
        assert_eq!(store.saved_tracks("a").unwrap()[0].id, track.id);
        assert!(store.saved_tracks("b").unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

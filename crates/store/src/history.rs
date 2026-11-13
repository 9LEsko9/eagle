// SPDX-License-Identifier: GPL-3.0-or-later
//! Local playback history, isolated by account and independent of provider availability.
use crate::{Error, Store, now};
use eagle_core::Track;
use rusqlite::params;

impl Store {
    /// History belongs to `profile`; the play report is queued for `account` (the provider
    /// account that owns the track) and skipped when there is none.
    pub fn record_play(&self, profile: &str, account: &str, track: &Track) -> Result<(), Error> {
        self.record_play_with_radio(profile, account, track, None)
    }
    pub fn record_play_with_radio(
        &self,
        profile: &str,
        account: &str,
        track: &Track,
        radio: Option<(u64, &crate::RadioContext)>,
    ) -> Result<(), Error> {
        if profile.is_empty() || (account.is_empty() && radio.is_some()) {
            return Err(Error::Corrupt);
        }
        let radio = radio
            .map(|(occurrence, context)| {
                if context.account != account || context.session_id.is_empty() || occurrence == 0 {
                    return Err(Error::Corrupt);
                }
                Ok((
                    i64::try_from(occurrence).map_err(|_| Error::Corrupt)?,
                    context.clone(),
                ))
            })
            .transpose()?;
        let (profile, account) = (profile.to_owned(), account.to_owned());
        let json = serde_json::to_string(track).map_err(|_| Error::Corrupt)?;
        let track = track.id.to_string();
        self.write(move |c| {
            let transaction = c.transaction()?;
            let played_at = now();
            transaction.execute(
                "INSERT INTO history(account,played_at,json) VALUES(?1,?2,?3)",
                params![profile, played_at, json],
            )?;
            if account.is_empty() {
                return Ok(transaction.commit()?);
            }
            let event_id: String = transaction.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            transaction.execute(
                "INSERT INTO play_outbox(id,account,track,played_at,state) VALUES(?1,?2,?3,?4,'pending')",
                params![event_id, account, track, played_at],
            )?;
            if let Some((occurrence, context)) = radio {
                transaction.execute("INSERT INTO radio_play_intents(event_id,account,session_id,batch_id,occurrence_id) VALUES(?1,?2,?3,?4,?5)", params![event_id, account, context.session_id, context.batch_id, occurrence])?;
            }
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
    fn radio_intent_is_atomic_and_migrates_existing_v7_outbox() {
        let root = std::env::temp_dir().join(format!("eg-radio-history-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        store.record_play("a", "a", &track()).unwrap();
        drop(store);
        let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        db.execute_batch("DROP TABLE radio_play_intents; PRAGMA user_version=7;")
            .unwrap();
        drop(db);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.play_events("a").unwrap().len(), 1);
        let context = crate::RadioContext {
            account: "a".into(),
            session_id: "session".into(),
            batch_id: Some("batch".into()),
        };
        store
            .record_play_with_radio("a", "a", &track(), Some((7, &context)))
            .unwrap();
        let stored: (String, String, i64) = store
            .read()
            .unwrap()
            .query_row(
                "SELECT account,session_id,occurrence_id FROM radio_play_intents",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, ("a".into(), "session".into(), 7));
        assert!(
            store
                .record_play_with_radio("a", "b", &track(), Some((8, &context)))
                .is_err()
        );
        store.write(|db| { db.execute_batch("CREATE TRIGGER fail_radio BEFORE INSERT ON radio_play_intents BEGIN SELECT RAISE(ABORT, 'test failure'); END;")?; Ok(()) }).unwrap();
        assert!(
            store
                .record_play_with_radio("a", "a", &track(), Some((8, &context)))
                .is_err()
        );
        assert_eq!(store.history("a").unwrap().len(), 2);
        assert_eq!(store.play_events("a").unwrap().len(), 2);
        assert!(store.history("b").unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn upgrades_complete_v4_without_losing_data() {
        let root = std::env::temp_dir().join(format!("eg-history-v4-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = track();
        store.put_track(&track).unwrap();
        store.put_setting("fixture", "retained".into()).unwrap();
        store.record_play("a", "a", &track).unwrap();
        store
            .enqueue_download("a", &track.id, eagle_core::Quality::High)
            .unwrap();
        drop(store);
        let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        db.execute_batch("DROP TABLE radio_play_intents; DROP TABLE access; DROP TABLE play_outbox; DROP TABLE offline_owners; PRAGMA user_version=4;")
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
        assert_eq!(version, 14);
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
        assert!(store.record_play("a", "a", &track()).is_err());
        assert!(store.history("a").unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn history_keeps_repeated_plays_and_survives_restart_without_crossing_accounts() {
        let root = std::env::temp_dir().join(format!("eg-history-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track: Track = serde_json::from_value(serde_json::json!({"id":"yandex:1", "title":"local", "artists":[], "album":null, "album_title":null, "duration_ms":10000, "cover":null, "availability":"Available"})).unwrap();
        store.record_play("a", "a", &track).unwrap();
        store.record_play("a", "a", &track).unwrap();
        store.record_play("a", "", &track).unwrap();
        assert!(store.history("b").unwrap().is_empty());
        drop(store);
        let store = Store::open(&root).unwrap();
        // The signed-out play is in the profile history but was never queued for a report.
        assert_eq!(store.history("a").unwrap().len(), 3);
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
    #[test]
    fn v9_moves_history_and_playback_to_the_profile_and_keeps_provider_rows() {
        let root = std::env::temp_dir().join(format!("eg-profile-v9-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = track();
        store.put_track(&track).unwrap();
        store.record_play("42", "42", &track).unwrap();
        store.record_play("other", "other", &track).unwrap();
        store
            .save_playback("42", crate::PlaybackState::default())
            .unwrap();
        store
            .enqueue_download("42", &track.id, eagle_core::Quality::High)
            .unwrap();
        store
            .put_setting("account_uid:yandex", "42".into())
            .unwrap();
        store
            .write(|db| {
                db.execute_batch(
                    "DELETE FROM settings WHERE key IN ('profile_id','device_id'); PRAGMA user_version=8;",
                )?;
                Ok(())
            })
            .unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        let (profile, device) = (store.profile_id().unwrap(), store.device_id().unwrap());
        assert_eq!((profile.len(), device.len()), (32, 32));
        assert_ne!(profile, device);
        assert_eq!(
            store.setting("account:yandex").unwrap().as_deref(),
            Some("42")
        );
        assert!(store.setting("account_uid:yandex").unwrap().is_none());
        assert_eq!(store.history(&profile).unwrap().len(), 1);
        assert!(store.history("42").unwrap().is_empty());
        assert!(store.playback(&profile).unwrap().is_some());
        // Another account's rows are left alone; provider-owned rows keep the provider uid.
        assert_eq!(store.history("other").unwrap().len(), 1);
        assert_eq!(store.downloads("42").unwrap().len(), 1);
        assert_eq!(store.play_events("42").unwrap().len(), 1);
        drop(store);
        // Reopening neither rewrites the ids nor migrates twice.
        let store = Store::open(&root).unwrap();
        assert_eq!(store.profile_id().unwrap(), profile);
        assert_eq!(store.device_id().unwrap(), device);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fresh_database_gets_profile_and_device_ids() {
        let root = std::env::temp_dir().join(format!("eg-profile-fresh-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        assert_eq!(store.profile_id().unwrap().len(), 32);
        assert_ne!(store.profile_id().unwrap(), store.device_id().unwrap());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

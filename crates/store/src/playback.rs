// SPDX-License-Identifier: GPL-3.0-or-later
//! Durable queue occurrences. Audio URLs never cross this boundary.
use crate::{Error, Store};
use eagle_core::Track;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackEntry {
    pub id: u64,
    pub track: Arc<Track>,
    #[serde(default)]
    pub radio: Option<RadioContext>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioContext {
    pub account: String,
    pub session_id: String,
    pub batch_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlaybackState {
    pub entries: Vec<PlaybackEntry>,
    pub order: Vec<u64>,
    pub current: Option<u64>,
    pub position_ms: u64,
    pub shuffle: bool,
    /// 0: off, 1: all, 2: one.
    pub repeat: u8,
}
impl PlaybackState {
    pub fn validate(&self) -> Result<(), Error> {
        let ids: HashSet<_> = self.entries.iter().map(|entry| entry.id).collect();
        let order: HashSet<_> = self.order.iter().copied().collect();
        if self.repeat > 2
            || ids.len() != self.entries.len()
            || ids.contains(&0)
            || ids != order
            || self.order.len() != self.entries.len()
            || self.current.is_none() != self.entries.is_empty()
            || self.current.is_some_and(|id| !ids.contains(&id))
            || self.entries.iter().any(|entry| {
                entry.radio.as_ref().is_some_and(|context| {
                    context.account.is_empty() || context.session_id.is_empty()
                })
            })
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}
impl Store {
    pub fn playback(&self, account: &str) -> Result<Option<PlaybackState>, Error> {
        let connection = self.read()?;
        // This one-shot reader scans one JSON value; a large page cache is never reused.
        connection.pragma_update(None, "cache_size", -64)?;
        let mut statement = connection.prepare("SELECT json FROM playback WHERE account=?1")?;
        let mut rows = statement.query([account])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        // Borrow SQLite's text while decoding instead of copying the entire queue JSON.
        let value = row.get_ref(0)?;
        let json = value.as_str().map_err(|_| Error::Corrupt)?;
        let state: PlaybackState = serde_json::from_str(json).map_err(|_| Error::Corrupt)?;
        state.validate()?;
        Ok(Some(state))
    }
    pub fn save_playback(&self, account: &str, state: PlaybackState) -> Result<(), Error> {
        state.validate()?;
        let account = account.to_owned();
        let json = serde_json::to_string(&state).map_err(|_| Error::Corrupt)?;
        self.write(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO playback VALUES(?1,?2)",
                params![account, json],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_metadata_keeps_the_existing_json_format_and_distinct_occurrences() {
        let track = Arc::new(Track {
            id: "fake:1".parse().unwrap(),
            title: "Track".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
            isrc: None,
        });
        let state = PlaybackState {
            entries: (1..=2)
                .map(|id| PlaybackEntry {
                    id,
                    track: track.clone(),
                    radio: None,
                })
                .collect(),
            order: vec![2, 1],
            current: Some(2),
            ..Default::default()
        };
        let checkpoint = state.clone();
        assert!(Arc::ptr_eq(
            &state.entries[0].track,
            &checkpoint.entries[0].track
        ));
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json["entries"][0]["track"],
            serde_json::to_value(track.as_ref()).unwrap()
        );
        let restored: PlaybackState = serde_json::from_value(json.clone()).unwrap();
        restored.validate().unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), json);
        assert!(!Arc::ptr_eq(
            &restored.entries[0].track,
            &restored.entries[1].track
        ));
    }
    #[test]
    fn database_text_is_decoded_and_corrupt_queues_are_rejected() {
        let root = std::env::temp_dir().join(format!("eg-playback-text-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        assert!(store.playback("missing").unwrap().is_none());
        store
            .save_playback("valid", PlaybackState::default())
            .unwrap();
        assert!(store.playback("valid").unwrap().unwrap().entries.is_empty());
        for json in [
            "{",
            r#"{"entries":[],"order":[1],"current":1,"position_ms":0,"shuffle":false,"repeat":0}"#,
        ] {
            let json = json.to_owned();
            store
                .write(move |c| {
                    c.execute(
                        "INSERT OR REPLACE INTO playback VALUES('invalid',?1)",
                        [json],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(matches!(store.playback("invalid"), Err(Error::Corrupt)));
        }
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_missing_current_and_ambiguous_occurrences() {
        let track = Track {
            id: "yandex:1".parse().unwrap(),
            title: "one".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: eagle_core::Availability::Available,
            source_url: None,
            isrc: None,
        };
        let entry = PlaybackEntry {
            id: 1,
            track: Arc::new(track),
            radio: None,
        };
        let mut state = PlaybackState {
            entries: vec![entry.clone()],
            order: vec![1],
            current: Some(1),
            ..Default::default()
        };
        assert!(state.validate().is_ok());
        state.entries.push(entry);
        assert!(state.validate().is_err());
        state.entries.pop();
        state.current = Some(2);
        assert!(state.validate().is_err());
        state.current = None;
        assert!(state.validate().is_err());
    }
    #[test]
    fn migrates_v1_and_keeps_accounts_separate() {
        let root =
            std::env::temp_dir().join(format!("eg-playback-migration-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("metadata.sqlite");
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE tracks(id TEXT PRIMARY KEY,json TEXT NOT NULL); CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO settings VALUES('existing','kept'); PRAGMA user_version=1;").unwrap();
        drop(c);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.setting("existing").unwrap().as_deref(), Some("kept"));
        let state = PlaybackState {
            shuffle: true,
            ..Default::default()
        };
        store.save_playback("first", state).unwrap();
        assert!(store.playback("first").unwrap().unwrap().shuffle);
        assert!(store.playback("second").unwrap().is_none());
        drop(store);
        let store = Store::open(&root).unwrap();
        assert!(store.playback("first").unwrap().is_some());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

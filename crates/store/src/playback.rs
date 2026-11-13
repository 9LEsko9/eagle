// SPDX-License-Identifier: GPL-3.0-or-later
//! Durable queue occurrences. Audio URLs never cross this boundary.
use crate::{Error, Store};
use eagle_core::Track;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackEntry {
    pub id: u64,
    pub track: Track,
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
        let json: Option<String> = self
            .read()?
            .query_row(
                "SELECT json FROM playback WHERE account=?1",
                [account],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|json| {
            let state: PlaybackState = serde_json::from_str(&json).map_err(|_| Error::Corrupt)?;
            state.validate()?;
            Ok(state)
        })
        .transpose()
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
        };
        let entry = PlaybackEntry {
            id: 1,
            track,
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
        c.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO settings VALUES('existing','kept'); PRAGMA user_version=1;").unwrap();
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

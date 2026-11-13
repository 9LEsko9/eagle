// SPDX-License-Identifier: GPL-3.0-or-later
//! Local play-event journal. Sending requires a verified provider contract.
use crate::{Error, Store};
use rusqlite::params;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxState {
    Pending,
    Uncertain,
    Confirmed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlayEvent {
    pub id: String,
    pub account: String,
    pub track: String,
    pub played_at: i64,
    pub state: OutboxState,
}
impl Store {
    pub fn enqueue_play_event(&self, event: PlayEvent) -> Result<(), Error> {
        if event.id.is_empty()
            || event.account.is_empty()
            || event.track.is_empty()
            || event.played_at < 0
            || event.state != OutboxState::Pending
        {
            return Err(Error::Corrupt);
        }
        self.write(move |c| {
            c.execute("INSERT INTO play_outbox(id,account,track,played_at,state) VALUES(?1,?2,?3,?4,'pending') ON CONFLICT(id) DO NOTHING", params![event.id,event.account,event.track,event.played_at])?;
            Ok(())
        })
    }
    pub fn play_events(&self, account: &str) -> Result<Vec<PlayEvent>, Error> {
        let c = self.read()?;
        let mut q = c.prepare("SELECT id,account,track,played_at,state FROM play_outbox WHERE account=?1 ORDER BY played_at,id")?;
        let rows = q.query_map([account], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|r| {
            let (id, account, track, played_at, state) = r?;
            Ok(PlayEvent {
                id,
                account,
                track,
                played_at,
                state: match state.as_str() {
                    "pending" => OutboxState::Pending,
                    "uncertain" => OutboxState::Uncertain,
                    "confirmed" => OutboxState::Confirmed,
                    _ => return Err(Error::Corrupt),
                },
            })
        })
        .collect()
    }
    /// An ambiguous attempt becomes uncertain and cannot be automatically resent.
    pub fn mark_play_event_uncertain(&self, account: &str, id: &str) -> Result<bool, Error> {
        let (account, id) = (account.to_owned(), id.to_owned());
        self.write(move |c| Ok(c.execute("UPDATE play_outbox SET state='uncertain' WHERE account=?1 AND id=?2 AND state='pending'",params![account,id])? == 1))
    }
    /// Call only with an explicit provider acknowledgment or reconciliation evidence.
    pub fn confirm_play_event(&self, account: &str, id: &str) -> Result<bool, Error> {
        let (account, id) = (account.to_owned(), id.to_owned());
        self.write(move |c| Ok(c.execute("UPDATE play_outbox SET state='confirmed' WHERE account=?1 AND id=?2 AND state IN ('pending','uncertain')",params![account,id])? == 1))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journal_is_idempotent_scoped_and_preserves_uncertainty() {
        let root = std::env::temp_dir().join(format!("eg-outbox-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let event = PlayEvent {
            id: "id".into(),
            account: "a".into(),
            track: "yandex:1".into(),
            played_at: 1,
            state: OutboxState::Pending,
        };
        store.enqueue_play_event(event.clone()).unwrap();
        store.enqueue_play_event(event).unwrap();
        assert!(!store.mark_play_event_uncertain("b", "id").unwrap());
        assert!(store.mark_play_event_uncertain("a", "id").unwrap());
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(
            store.play_events("a").unwrap()[0].state,
            OutboxState::Uncertain
        );
        assert!(store.play_events("b").unwrap().is_empty());
        assert!(store.confirm_play_event("a", "id").unwrap());
        assert_eq!(
            store.play_events("a").unwrap()[0].state,
            OutboxState::Confirmed
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

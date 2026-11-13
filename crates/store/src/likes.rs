// SPDX-License-Identifier: GPL-3.0-or-later
//! Local likes. The source of truth is `liked(grp, at)`, derived from the profile journal; the
//! origin of a like is not kept, so signing out of a provider never removes likes.
use crate::profile_ops::{self, KIND_LIKE};
use crate::{Error, Store};
use eagle_core::{Track, TrackId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashSet;

/// What a provider sync changed locally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LikeSync {
    pub added: usize,
    pub removed: usize,
}

fn group_in(tx: &Transaction, track: &str) -> Result<Option<i64>, Error> {
    Ok(tx
        .query_row(
            "SELECT grp FROM track_groups WHERE track=?1",
            [track],
            |r| r.get(0),
        )
        .optional()?)
}

fn members_in(tx: &Transaction, grp: i64) -> Result<Vec<String>, Error> {
    let mut stmt = tx.prepare("SELECT track FROM track_groups WHERE grp=?1 ORDER BY track")?;
    Ok(stmt
        .query_map([grp], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

fn provider_of(track: &str) -> &str {
    track
        .split_once(':')
        .map_or(track, |(provider, _)| provider)
}

fn like_body(tx: &Transaction, track: &str) -> Result<String, Error> {
    let json: String = tx
        .query_row("SELECT json FROM tracks WHERE id=?1", [track], |r| r.get(0))
        .optional()?
        .ok_or(Error::UnknownTrack)?;
    let snapshot: serde_json::Value = serde_json::from_str(&json).map_err(|_| Error::Corrupt)?;
    Ok(serde_json::json!({"liked": true, "track": snapshot}).to_string())
}

const BACKFILL_SQL: &str = "SELECT MIN(g.track), l.at FROM liked l JOIN track_groups g ON g.grp=l.grp \
     WHERE substr(g.track,1,length(?1)+1)=?1||':' \
     AND NOT EXISTS(SELECT 1 FROM provider_liked_seen s JOIN track_groups g2 ON g2.track=s.track WHERE s.provider=?1 AND g2.grp=l.grp) \
     AND NOT EXISTS(SELECT 1 FROM like_outbox o JOIN track_groups g3 ON g3.track=o.track WHERE o.provider=?1 AND g3.grp=l.grp) \
     GROUP BY l.grp ORDER BY l.at, l.grp";

const UNLIKE_BODY: &str = r#"{"liked":false}"#;

/// Likes the track, or unlikes every member of its group (a tombstone per member, so that no
/// provider's stale copy of the like can bring it back).
fn set_liked_in(tx: &Transaction, track: &str, liked: bool) -> Result<i64, Error> {
    let grp = group_in(tx, track)?.ok_or(Error::UnknownTrack)?;
    if liked {
        let body = like_body(tx, track)?;
        profile_ops::append_local(tx, KIND_LIKE, track, body)?;
    } else {
        for member in members_in(tx, grp)? {
            profile_ops::append_local(tx, KIND_LIKE, &member, UNLIKE_BODY.into())?;
        }
    }
    profile_ops::project_like(tx, track)?;
    Ok(grp)
}

/// Likes stored without their track's metadata: the list is kept until the track arrives.
/// Resolves the ones among `tracks` (just stored, in the same transaction) and returns how many.
pub(crate) fn resolve_likes_in(tx: &Transaction, tracks: &[String]) -> Result<usize, Error> {
    let mut resolved = 0;
    for track in tracks {
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM likes_unresolved WHERE track=?1)",
            [track],
            |r| r.get(0),
        )?;
        if !pending {
            continue;
        }
        set_liked_in(tx, track, true)?;
        tx.execute("DELETE FROM likes_unresolved WHERE track=?1", [track])?;
        resolved += 1;
    }
    Ok(resolved)
}

impl Store {
    /// Likes of `provider` whose track metadata is not stored yet, oldest like first.
    pub fn unresolved_likes(&self, provider: &str, limit: usize) -> Result<Vec<TrackId>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let c = self.read()?;
        let mut stmt = c.prepare(
            "SELECT track FROM likes_unresolved WHERE substr(track,1,length(?1)+1)=?1||':' ORDER BY at LIMIT ?2",
        )?;
        let tracks: Vec<String> = stmt
            .query_map(params![provider, limit], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        tracks
            .iter()
            .map(|t| t.parse().map_err(|_| Error::Corrupt))
            .collect()
    }

    pub fn unresolved_likes_count(&self) -> Result<usize, Error> {
        let count: i64 =
            self.read()?
                .query_row("SELECT COUNT(*) FROM likes_unresolved", [], |r| r.get(0))?;
        usize::try_from(count).map_err(|_| Error::Corrupt)
    }

    /// Likes or unlikes a track in the profile: the journal op, `liked`, and one outbox job per
    /// provider in `targets` that has a track in the group, all in one transaction.
    pub fn like(&self, id: &TrackId, liked: bool, targets: &[String]) -> Result<(), Error> {
        let track = id.to_string();
        let targets = targets.to_vec();
        self.write(move |c| {
            let tx = c.transaction()?;
            let grp = set_liked_in(&tx, &track, liked)?;
            let hlc = profile_ops::next_hlc(&tx)?;
            for member in members_in(&tx, grp)? {
                let provider = provider_of(&member);
                if targets.iter().any(|t| t == provider) {
                    tx.execute(
                        "INSERT OR REPLACE INTO like_outbox VALUES(?1,?2,?3,'pending',?4,0,0)",
                        params![provider, member, liked, hlc],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn is_liked(&self, id: &TrackId) -> Result<bool, Error> {
        Ok(self.read()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM track_groups g JOIN liked l ON l.grp=g.grp WHERE g.track=?1)",
            [id.to_string()],
            |r| r.get(0),
        )?)
    }

    /// Ids of every track whose group is liked.
    pub fn liked_track_ids(&self) -> Result<HashSet<String>, Error> {
        let c = self.read()?;
        let mut stmt =
            c.prepare("SELECT g.track FROM track_groups g JOIN liked l ON l.grp=g.grp")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Liked tracks, newest like first, one per group: the member of the provider that comes
    /// first in `priority`, else the first by id.
    pub fn liked_tracks(&self, priority: &[String]) -> Result<Vec<Track>, Error> {
        let c = self.read()?;
        let mut stmt = c.prepare(
            "SELECT l.grp,t.id,t.json FROM liked l JOIN track_groups g ON g.grp=l.grp JOIN tracks t ON t.id=g.track ORDER BY l.at DESC, l.grp DESC, t.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let rank = |id: &str| {
            priority
                .iter()
                .position(|p| p == provider_of(id))
                .unwrap_or(priority.len())
        };
        let mut out: Vec<(i64, usize, String)> = Vec::new();
        for row in rows {
            let (grp, id, json) = row?;
            let rank = rank(&id);
            match out.last_mut() {
                Some((last, best, best_json)) if *last == grp => {
                    if rank < *best {
                        *best = rank;
                        *best_json = json;
                    }
                }
                _ => out.push((grp, rank, json)),
            }
        }
        out.into_iter()
            .map(|(_, _, json)| serde_json::from_str(&json).map_err(|_| Error::Corrupt))
            .collect()
    }

    /// Merges one provider's liked tracks (`ids`, newest first, already stored) into `liked`.
    ///
    /// Liked groups that have a track of `provider` which the provider does not list and has no
    /// queued job: the likes that could be sent to a newly connected service. Oldest first.
    pub fn likes_backfill_candidates(&self, provider: &str) -> Result<Vec<(String, i64)>, Error> {
        let c = self.read()?;
        let mut stmt = c.prepare(BACKFILL_SQL)?;
        Ok(stmt
            .query_map([provider], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    /// Queues a like job for every backfill candidate of `provider`, keeping the like's own
    /// order (`at`) so the provider sees them oldest first. Returns how many were queued.
    pub fn queue_likes_backfill(&self, provider: &str) -> Result<usize, Error> {
        let provider = provider.to_owned();
        self.write(move |c| {
            let tx = c.transaction()?;
            let candidates: Vec<(String, i64)> = {
                let mut stmt = tx.prepare(BACKFILL_SQL)?;
                stmt.query_map([&provider], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (track, at) in &candidates {
                tx.execute(
                    "INSERT OR IGNORE INTO like_outbox VALUES(?1,?2,1,'pending',?3,0,0)",
                    params![provider, track, at],
                )?;
            }
            tx.commit()?;
            Ok(candidates.len())
        })
    }

    /// New likes are added. A like that the provider listed last time and no longer lists is
    /// removed locally only if it has not been set again since that sync. A like the provider
    /// never listed, or a track whose unlike has not reached the provider yet, is left alone.
    pub fn sync_provider_likes(&self, provider: &str, ids: &[TrackId]) -> Result<LikeSync, Error> {
        let provider = provider.to_owned();
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        self.write(move |c| {
            let tx = c.transaction()?;
            let synced_key = format!("likes_synced:{provider}");
            let previous: Option<i64> = tx
                .query_row("SELECT value FROM settings WHERE key=?1", [&synced_key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .and_then(|v| v.parse().ok());
            let current: HashSet<&str> = ids.iter().map(String::as_str).collect();
            let mut outcome = LikeSync::default();
            if let Some(synced_at) = previous {
                let seen: Vec<String> = {
                    let mut stmt =
                        tx.prepare("SELECT track FROM provider_liked_seen WHERE provider=?1")?;
                    stmt.query_map([&provider], |r| r.get(0))?
                        .collect::<Result<_, _>>()?
                };
                for track in seen.iter().filter(|t| !current.contains(t.as_str())) {
                    let Some(grp) = group_in(&tx, track)? else {
                        continue;
                    };
                    let at: Option<i64> = tx
                        .query_row("SELECT at FROM liked WHERE grp=?1", [grp], |r| r.get(0))
                        .optional()?;
                    if at.is_some_and(|at| at <= synced_at) {
                        set_liked_in(&tx, track, false)?;
                        outcome.removed += 1;
                    }
                }
            }
            for track in ids.iter().rev() {
                if group_in(&tx, track)?.is_none() {
                    log::warn!(provider = provider.as_str(); "liked track has no group");
                    continue;
                }
                let held_back: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM like_outbox WHERE provider=?1 AND track=?2 AND liked=0) OR EXISTS(SELECT 1 FROM profile_state WHERE kind='like' AND key=?2 AND deleted=1 AND hlc>?3)",
                    params![provider, track, previous.unwrap_or(i64::MIN)],
                    |r| r.get(0),
                )?;
                let already: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM track_groups g JOIN liked l ON l.grp=g.grp WHERE g.track=?1)",
                    [track],
                    |r| r.get(0),
                )?;
                if !held_back && !already {
                    set_liked_in(&tx, track, true)?;
                    outcome.added += 1;
                }
            }
            let now = profile_ops::next_hlc(&tx)?;
            tx.execute("DELETE FROM provider_liked_seen WHERE provider=?1", [&provider])?;
            for track in &ids {
                tx.execute(
                    "INSERT OR IGNORE INTO provider_liked_seen VALUES(?1,?2,?3)",
                    params![provider, track, now],
                )?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO settings VALUES(?1,?2)",
                params![synced_key, now.to_string()],
            )?;
            tx.commit()?;
            Ok(outcome)
        })
    }
}

/// v11: journal, `liked` and outbox tables.
pub(crate) fn migrate_tables(c: &Connection) -> Result<(), Error> {
    c.execute_batch(&format!(
        "BEGIN IMMEDIATE; {} PRAGMA user_version=11; COMMIT;",
        profile_ops::SCHEMA
    ))?;
    Ok(())
}

/// v12: `likes` becomes `liked` through groups; every like is recorded as an op of this device,
/// oldest first so that the order of the list survives.
pub(crate) fn migrate_likes(c: &mut Connection) -> Result<(), Error> {
    let tx = c.transaction()?;
    let has_likes: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='likes')",
        [],
        |r| r.get(0),
    )?;
    if !has_likes {
        tx.execute_batch("PRAGMA user_version=12;")?;
        return Ok(tx.commit()?);
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS likes_unresolved(track TEXT PRIMARY KEY, at INTEGER NOT NULL);",
    )?;
    let likes: Vec<(String, i64)> = {
        let mut stmt = tx.prepare("SELECT id, rowid FROM likes ORDER BY rowid")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut unresolved = 0;
    for (id, at) in &likes {
        match set_liked_in(&tx, id, true) {
            Ok(_) => {}
            // Kept, not dropped: the like applies once the track's metadata arrives.
            Err(Error::UnknownTrack) => {
                unresolved += tx.execute(
                    "INSERT OR IGNORE INTO likes_unresolved VALUES(?1,?2)",
                    params![id, at],
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    if unresolved > 0 {
        log::info!(count = unresolved; "v12 migration kept likes without track metadata");
    }
    tx.execute_batch("DROP TABLE likes; PRAGMA user_version=12;")?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Op;
    use eagle_core::{Artist, Availability};
    use std::collections::HashMap;

    fn root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("eg-likes-{name}-{}", std::process::id()))
    }

    fn track(id: &str, title: &str, duration_ms: u64) -> Track {
        Track {
            id: id.parse().unwrap(),
            title: title.into(),
            artists: vec![Artist {
                id: format!("{}:a", id.split(':').next().unwrap())
                    .parse()
                    .unwrap(),
                name: "Same Artist".into(),
                cover: None,
            }],
            album: None,
            album_title: None,
            duration_ms,
            cover: None,
            availability: Availability::Available,
            isrc: None,
        }
    }

    fn id(value: &str) -> TrackId {
        value.parse().unwrap()
    }

    fn open(name: &str) -> (Store, std::path::PathBuf) {
        let dir = root(name);
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    fn liked_ids(store: &Store) -> Vec<String> {
        let mut ids: Vec<_> = store.liked_track_ids().unwrap().into_iter().collect();
        ids.sort();
        ids
    }

    fn all_ops(store: &Store) -> Vec<Op> {
        store.ops_since(&HashMap::new()).unwrap()
    }

    #[test]
    fn like_writes_op_liked_and_outbox_for_providers_with_a_track_in_the_group() {
        let (store, dir) = open("like");
        store
            .put_tracks(&[
                track("fakea:1", "Song", 200_000),
                track("fakeb:9", "Song", 200_500),
                track("fakec:5", "Other", 100_000),
            ])
            .unwrap();
        let targets = vec!["fakea".to_owned(), "fakeb".to_owned(), "fakec".to_owned()];
        store.like(&id("fakea:1"), true, &targets).unwrap();
        assert!(store.is_liked(&id("fakeb:9")).unwrap());
        assert!(!store.is_liked(&id("fakec:5")).unwrap());
        assert_eq!(all_ops(&store).len(), 1);
        let a = store.like_jobs("fakea", 0).unwrap();
        let b = store.like_jobs("fakeb", 0).unwrap();
        assert_eq!((a[0].track.as_str(), a[0].liked), ("fakea:1", true));
        assert_eq!((b[0].track.as_str(), b[0].liked), ("fakeb:9", true));
        assert!(store.like_jobs("fakec", 0).unwrap().is_empty());
        store.like(&id("fakeb:9"), false, &targets).unwrap();
        assert!(!store.is_liked(&id("fakea:1")).unwrap());
        // The newest intent replaces the older one.
        let a = store.like_jobs("fakea", 0).unwrap();
        assert_eq!((a.len(), a[0].liked), (1, false));
        assert!(matches!(
            store.like(&id("fakea:404"), true, &[]),
            Err(Error::UnknownTrack)
        ));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uncertain_jobs_are_not_retried_and_rate_limits_defer() {
        let (store, dir) = open("outbox");
        store
            .put_tracks(&[track("fakea:1", "Song", 200_000)])
            .unwrap();
        store
            .like(&id("fakea:1"), true, &["fakea".to_owned()])
            .unwrap();
        let job = store.like_jobs("fakea", 0).unwrap().remove(0);
        assert_eq!(store.defer_like_job_backoff(&job, 70).unwrap(), Some(100));
        assert!(store.like_jobs("fakea", 99).unwrap().is_empty());
        assert_eq!(store.like_jobs("fakea", 100).unwrap().len(), 1);
        assert!(store.mark_like_job_uncertain(&job).unwrap());
        assert!(store.like_jobs("fakea", i64::MAX).unwrap().is_empty());
        assert_eq!(store.uncertain_like_jobs("fakea").unwrap().len(), 1);
        // The user acting again queues a fresh job.
        store
            .like(&id("fakea:1"), false, &["fakea".to_owned()])
            .unwrap();
        let job = store.like_jobs("fakea", 0).unwrap().remove(0);
        assert!(!job.liked);
        assert!(store.finish_like_job(&job).unwrap());
        assert!(store.like_jobs("fakea", 0).unwrap().is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn device_with(name: &str, tracks: &[Track]) -> (Store, std::path::PathBuf) {
        let (store, dir) = open(name);
        store.put_tracks(tracks).unwrap();
        (store, dir)
    }

    #[test]
    fn ops_apply_in_any_order_twice_and_converge() {
        let songs = [
            track("fakea:1", "One", 100_000),
            track("fakea:2", "Two", 100_000),
            track("fakea:3", "Three", 100_000),
        ];
        let (a, da) = device_with("conv-a", &songs);
        a.like(&id("fakea:1"), true, &[]).unwrap();
        a.like(&id("fakea:2"), true, &[]).unwrap();
        a.like(&id("fakea:1"), false, &[]).unwrap();
        a.like(&id("fakea:3"), true, &[]).unwrap();
        let ops = all_ops(&a);
        let orders: [Vec<usize>; 4] = [
            vec![0, 1, 2, 3],
            vec![3, 2, 1, 0],
            vec![2, 0, 3, 1],
            vec![3, 3, 1, 0, 2, 3, 2],
        ];
        for (n, order) in orders.iter().enumerate() {
            let (b, db) = open(&format!("conv-b{n}"));
            b.apply_ops(order.iter().map(|&i| ops[i].clone()).collect())
                .unwrap();
            b.apply_ops(ops.clone()).unwrap();
            assert_eq!(liked_ids(&b), ["fakea:2", "fakea:3"], "order {order:?}");
            drop(b);
            std::fs::remove_dir_all(db).unwrap();
        }
        drop(a);
        std::fs::remove_dir_all(da).unwrap();
    }

    #[test]
    fn tombstone_beats_older_like_and_ties_go_to_the_greater_device() {
        let (store, dir) = open("tomb");
        let snapshot = track("fakea:1", "One", 100_000);
        let like = |device: &str, seq, hlc, liked: bool| Op {
            device: device.into(),
            seq,
            hlc,
            kind: "like".into(),
            key: "fakea:1".into(),
            body: if liked {
                serde_json::json!({"liked": true, "track": snapshot}).to_string()
            } else {
                r#"{"liked":false}"#.into()
            },
        };
        store.apply_ops(vec![like("d1", 1, 50, false)]).unwrap();
        store.apply_ops(vec![like("d2", 1, 40, true)]).unwrap();
        assert!(
            liked_ids(&store).is_empty(),
            "older like must not resurrect"
        );
        store.apply_ops(vec![like("d2", 2, 50, true)]).unwrap();
        assert_eq!(
            liked_ids(&store),
            ["fakea:1"],
            "tie goes to the greater device"
        );
        store.apply_ops(vec![like("d1", 2, 60, false)]).unwrap();
        assert!(liked_ids(&store).is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_like_from_another_device_lands_in_the_local_group_without_the_network() {
        let (a, da) = device_with("share-a", &[track("fakea:1", "Song", 200_000)]);
        a.like(&id("fakea:1"), true, &[]).unwrap();
        let (b, db) = device_with("share-b", &[track("fakeb:7", "Song", 200_800)]);
        b.apply_ops(all_ops(&a)).unwrap();
        // B knew the same song from another provider: the like joins that group.
        assert!(b.is_liked(&id("fakeb:7")).unwrap());
        assert!(b.is_liked(&id("fakea:1")).unwrap());
        assert_eq!(
            b.liked_tracks(&["fakeb".to_owned()]).unwrap()[0]
                .id
                .to_string(),
            "fakeb:7"
        );
        drop((a, b));
        std::fs::remove_dir_all(da).unwrap();
        std::fs::remove_dir_all(db).unwrap();
    }

    #[test]
    fn hlc_stays_monotonic_when_the_remote_clock_is_ahead() {
        let (store, dir) = device_with("hlc", &[track("fakea:1", "One", 100_000)]);
        let far = (i64::MAX >> 4) as u64;
        store
            .apply_ops(vec![Op {
                device: "remote".into(),
                seq: 1,
                hlc: far,
                kind: "like".into(),
                key: "fakea:9".into(),
                body: r#"{"liked":false}"#.into(),
            }])
            .unwrap();
        store.like(&id("fakea:1"), true, &[]).unwrap();
        let mine = all_ops(&store)
            .into_iter()
            .find(|op| op.device != "remote")
            .unwrap();
        assert!(mine.hlc > far);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_merges_and_removes_only_what_the_provider_once_listed() {
        let songs = [
            track("fakea:1", "One", 100_000),
            track("fakea:2", "Two", 100_000),
            track("fakea:3", "Three", 100_000),
        ];
        let (store, dir) = device_with("sync", &songs);
        let list = |ns: &[&str]| ns.iter().map(|n| id(n)).collect::<Vec<_>>();
        // First sync: provider order is newest first.
        let out = store
            .sync_provider_likes("fakea", &list(&["fakea:2", "fakea:1"]))
            .unwrap();
        assert_eq!((out.added, out.removed), (2, 0));
        let order: Vec<_> = store
            .liked_tracks(&[])
            .unwrap()
            .into_iter()
            .map(|t| t.id.to_string())
            .collect();
        assert_eq!(order, ["fakea:2", "fakea:1"]);
        // A like the provider never listed is not touched.
        store.like(&id("fakea:3"), true, &[]).unwrap();
        // The service removed fakea:1: it goes; fakea:3 stays.
        store
            .sync_provider_likes("fakea", &list(&["fakea:2"]))
            .unwrap();
        assert_eq!(liked_ids(&store), ["fakea:2", "fakea:3"]);
        // Liked locally after the service removed it: stays.
        store
            .sync_provider_likes("fakea", &list(&["fakea:2", "fakea:3"]))
            .unwrap();
        store.like(&id("fakea:3"), false, &[]).unwrap();
        store.like(&id("fakea:3"), true, &[]).unwrap();
        store
            .sync_provider_likes("fakea", &list(&["fakea:2"]))
            .unwrap();
        assert_eq!(liked_ids(&store), ["fakea:2", "fakea:3"]);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_does_not_bring_back_a_like_whose_unlike_is_still_queued() {
        let (store, dir) = device_with("held", &[track("fakea:1", "One", 100_000)]);
        let targets = vec!["fakea".to_owned()];
        store
            .sync_provider_likes("fakea", &[id("fakea:1")])
            .unwrap();
        store.like(&id("fakea:1"), false, &targets).unwrap();
        store
            .sync_provider_likes("fakea", &[id("fakea:1")])
            .unwrap();
        assert!(liked_ids(&store).is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_removal_on_one_provider_is_not_undone_by_the_other_providers_copy() {
        let (store, dir) = device_with(
            "two",
            &[
                track("fakea:1", "Song", 200_000),
                track("fakeb:1", "Song", 200_000),
            ],
        );
        store
            .sync_provider_likes("fakea", &[id("fakea:1")])
            .unwrap();
        store
            .sync_provider_likes("fakeb", &[id("fakeb:1")])
            .unwrap();
        store.sync_provider_likes("fakea", &[]).unwrap();
        assert!(liked_ids(&store).is_empty());
        store
            .sync_provider_likes("fakeb", &[id("fakeb:1")])
            .unwrap();
        assert!(liked_ids(&store).is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compaction_bounds_the_journal_and_a_late_peer_still_converges() {
        let (store, dir) = device_with("compact", &[track("fakea:1", "One", 100_000)]);
        for n in 0..10_000 {
            store.like(&id("fakea:1"), n % 2 == 0, &[]).unwrap();
        }
        assert_eq!(store.profile_op_count().unwrap(), 10_000);
        store.compact_profile().unwrap();
        assert!(store.profile_op_count().unwrap() <= 1);
        // Starting up reads the state tables, never the journal.
        let c = store.read().unwrap();
        let plan: Vec<String> = c
            .prepare(
                "EXPLAIN QUERY PLAN SELECT g.track FROM track_groups g JOIN liked l ON l.grp=g.grp",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(plan.iter().all(|line| !line.contains("profile_ops")));
        let (peer, dp) = open("compact-peer");
        peer.apply_ops(all_ops(&store)).unwrap();
        assert_eq!(liked_ids(&peer), liked_ids(&store));
        drop((c, store, peer));
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(dp).unwrap();
    }

    #[test]
    fn migration_turns_likes_into_groups_and_ops_and_drops_the_old_table() {
        let dir = root("migrate");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&dir).unwrap();
            store
                .put_tracks(&[track("fakea:1", "One", 1000), track("fakea:2", "Two", 1000)])
                .unwrap();
        }
        {
            // Rewind to schema v10 with the old table.
            let c = Connection::open(dir.join("metadata.sqlite")).unwrap();
            c.execute_batch(
                "CREATE TABLE likes(id TEXT PRIMARY KEY);
                 INSERT INTO likes VALUES('fakea:1'),('fakea:2'),('fakea:404');
                 DROP TABLE liked; DROP TABLE like_outbox; DROP TABLE provider_liked_seen;
                 DROP TABLE profile_ops; DROP TABLE profile_state; DROP TABLE profile_devices;
                 PRAGMA user_version=10;",
            )
            .unwrap();
        }
        let store = Store::open(&dir).unwrap();
        let order: Vec<_> = store
            .liked_tracks(&[])
            .unwrap()
            .into_iter()
            .map(|t| t.id.to_string())
            .collect();
        assert_eq!(order, ["fakea:2", "fakea:1"]);
        assert_eq!(store.profile_op_count().unwrap(), 2);
        let c = store.read().unwrap();
        let has_old: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='likes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!has_old);
        drop((c, store));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn table_exists(store: &Store, name: &str) -> bool {
        store
            .read()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [name],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn user_version(store: &Store) -> i64 {
        store
            .read()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn like_ops_for(store: &Store, track: &str) -> usize {
        let count: i64 = store
            .read()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM profile_ops WHERE kind='like' AND key=?1",
                [track],
                |r| r.get(0),
            )
            .unwrap();
        usize::try_from(count).unwrap()
    }

    /// A store with `tracks`, closed and then rewound to an older schema by `raw` SQL.
    fn rewound(name: &str, tracks: &[Track], raw: &str) -> std::path::PathBuf {
        let dir = root(name);
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&dir).unwrap();
            store.put_tracks(tracks).unwrap();
        }
        Connection::open(dir.join("metadata.sqlite"))
            .unwrap()
            .execute_batch(raw)
            .unwrap();
        dir
    }

    /// The v11 schema: `likes` still holds the list and `liked` is empty.
    fn v11_with_likes(likes: &str) -> String {
        format!(
            "DROP TABLE IF EXISTS likes_unresolved;
             CREATE TABLE likes(id TEXT PRIMARY KEY);
             INSERT INTO likes VALUES {likes};
             PRAGMA user_version=11;"
        )
    }

    #[test]
    fn migration_moves_likes_without_metadata_to_unresolved() {
        let dir = rewound(
            "migrate-unresolved",
            &[track("yandex:1", "One", 1000)],
            &v11_with_likes("('yandex:1'),('yandex:2')"),
        );
        let store = Store::open(&dir).unwrap();
        assert_eq!(liked_ids(&store), ["yandex:1"]);
        assert_eq!(
            store.unresolved_likes("yandex", 10).unwrap(),
            [id("yandex:2")]
        );
        assert!(!table_exists(&store, "likes"));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migration_keeps_likes_of_unknown_providers() {
        let dir = rewound("migrate-unknown", &[], &v11_with_likes("('vk:7')"));
        let store = Store::open(&dir).unwrap();
        assert!(liked_ids(&store).is_empty());
        assert_eq!(store.unresolved_likes("vk", 10).unwrap(), [id("vk:7")]);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn storing_the_track_resolves_its_like_with_a_journal_op() {
        let dir = rewound("resolve", &[], &v11_with_likes("('yandex:2')"));
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            store.unresolved_likes("yandex", 10).unwrap(),
            [id("yandex:2")]
        );
        store.put_tracks(&[track("yandex:2", "Two", 1000)]).unwrap();
        assert!(store.is_liked(&id("yandex:2")).unwrap());
        assert!(store.unresolved_likes("yandex", 10).unwrap().is_empty());
        assert_eq!(store.unresolved_likes_count().unwrap(), 0);
        assert_eq!(like_ops_for(&store, "yandex:2"), 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v12_database_gets_the_unresolved_table() {
        let dir = root("v12");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = Store::open(&dir).unwrap();
            store.put_tracks(&[track("yandex:1", "One", 1000)]).unwrap();
            store.like(&id("yandex:1"), true, &[]).unwrap();
        }
        // The schema of v12: no `likes_unresolved` yet.
        Connection::open(dir.join("metadata.sqlite"))
            .unwrap()
            .execute_batch("DROP TABLE IF EXISTS likes_unresolved; PRAGMA user_version=12;")
            .unwrap();
        let store = Store::open(&dir).unwrap();
        assert_eq!(user_version(&store), 14);
        assert!(table_exists(&store, "likes_unresolved"));
        assert_eq!(liked_ids(&store), ["yandex:1"]);
        assert_eq!(store.unresolved_likes_count().unwrap(), 0);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

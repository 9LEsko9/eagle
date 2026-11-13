// SPDX-License-Identifier: GPL-3.0-or-later
//! Track groups: one group per recording across providers, and artist groups derived from them.
//! A track's group is decided once, when it is first stored, and never re-evaluated.
use crate::{Error, Store};
use eagle_core::matching::{MergeMethod, can_merge, isrc_key, match_key, normalize};
use eagle_core::{Track, TrackId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS track_groups(track TEXT PRIMARY KEY, grp INTEGER NOT NULL, method TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS track_groups_grp ON track_groups(grp);
CREATE TABLE IF NOT EXISTS track_keys(key TEXT NOT NULL, track TEXT NOT NULL, PRIMARY KEY(key, track));
CREATE TABLE IF NOT EXISTS artist_groups(artist TEXT PRIMARY KEY, grp INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS artist_groups_grp ON artist_groups(grp);
";

/// v10: backfills groups for tracks stored before groups existed. Each batch commits on its own,
/// so an interrupted run resumes; `PRAGMA user_version` moves to 10 only after the last batch.
pub(crate) fn migrate(c: &mut Connection) -> Result<(), Error> {
    c.execute_batch(SCHEMA)?;
    let mut after = String::new();
    loop {
        let batch: Vec<(String, String)> = {
            let mut stmt = c.prepare(
                "SELECT id,json FROM tracks WHERE id>?1 AND id NOT IN (SELECT track FROM track_groups) ORDER BY id LIMIT 500",
            )?;
            stmt.query_map([&after], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let Some((last, _)) = batch.last() else {
            break;
        };
        after = last.clone();
        let tx = c.transaction()?;
        for (_, json) in &batch {
            match serde_json::from_str::<Track>(json) {
                Ok(track) => assign(&tx, &track)?,
                Err(_) => log::warn!("v10 migration skipped a track with unreadable metadata"),
            }
        }
        tx.commit()?;
    }
    c.execute_batch("PRAGMA user_version=10;")?;
    Ok(())
}

/// Places `track` into a group. Called inside the transaction that stores the track.
pub(crate) fn assign(tx: &Transaction, track: &Track) -> Result<(), Error> {
    let id = track.id.to_string();
    let isrc = isrc_key(track).map(|key| format!("isrc:{key}"));
    let meta = match_key(track).map(|key| format!("meta:{key}"));
    tx.execute("DELETE FROM track_keys WHERE track=?1", [&id])?;
    for key in isrc.iter().chain(meta.iter()) {
        tx.execute(
            "INSERT OR IGNORE INTO track_keys VALUES(?1,?2)",
            params![key, id],
        )?;
    }
    let existing: Option<i64> = tx
        .query_row("SELECT grp FROM track_groups WHERE track=?1", [&id], |r| {
            r.get(0)
        })
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    for key in isrc.iter().chain(meta.iter()) {
        if let Some((candidate, grp, method)) = find_match(tx, track, key)? {
            tx.execute(
                "INSERT INTO track_groups VALUES(?1,?2,?3)",
                params![id, grp, method_name(method)],
            )?;
            union_artists(tx, track, &candidate)?;
            return Ok(());
        }
    }
    let grp = next_id(tx, "track_groups")?;
    tx.execute(
        "INSERT INTO track_groups VALUES(?1,?2,'self')",
        params![id, grp],
    )?;
    Ok(())
}

/// First candidate sharing `key` that can merge with `track` and whose group has no member
/// from `track`'s provider.
fn find_match(
    tx: &Transaction,
    track: &Track,
    key: &str,
) -> Result<Option<(Track, i64, MergeMethod)>, Error> {
    let id = track.id.to_string();
    let jsons: Vec<String> = {
        let mut stmt = tx.prepare_cached(
            "SELECT t.json FROM track_keys k JOIN tracks t ON t.id=k.track WHERE k.key=?1 AND k.track!=?2 ORDER BY t.id",
        )?;
        stmt.query_map(params![key, id], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for json in jsons {
        let Ok(candidate) = serde_json::from_str::<Track>(&json) else {
            continue;
        };
        let Some(method) = can_merge(track, &candidate) else {
            continue;
        };
        let group: Option<i64> = tx
            .query_row(
                "SELECT grp FROM track_groups WHERE track=?1",
                [candidate.id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(grp) = group else {
            continue;
        };
        let has_provider: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM track_groups WHERE grp=?1 AND track LIKE ?2 || ':%')",
            params![grp, track.id.provider()],
            |r| r.get(0),
        )?;
        if !has_provider {
            return Ok(Some((candidate, grp, method)));
        }
    }
    Ok(None)
}

/// Joins artists with equal normalized names across a merged pair of tracks.
fn union_artists(tx: &Transaction, track: &Track, candidate: &Track) -> Result<(), Error> {
    for a in &track.artists {
        let name = normalize(&a.name);
        if name.is_empty() {
            continue;
        }
        for b in &candidate.artists {
            if a.id == b.id || normalize(&b.name) != name {
                continue;
            }
            union_artist(tx, &a.id.to_string(), &b.id.to_string())?;
        }
    }
    Ok(())
}

fn union_artist(tx: &Transaction, a: &str, b: &str) -> Result<(), Error> {
    let ga = artist_group_in(tx, a)?;
    let gb = artist_group_in(tx, b)?;
    match (ga, gb) {
        (None, None) => {
            let grp = next_id(tx, "artist_groups")?;
            tx.execute("INSERT INTO artist_groups VALUES(?1,?2)", params![a, grp])?;
            tx.execute("INSERT INTO artist_groups VALUES(?1,?2)", params![b, grp])?;
        }
        (Some(grp), None) => {
            tx.execute("INSERT INTO artist_groups VALUES(?1,?2)", params![b, grp])?;
        }
        (None, Some(grp)) => {
            tx.execute("INSERT INTO artist_groups VALUES(?1,?2)", params![a, grp])?;
        }
        (Some(keep), Some(other)) if keep != other => {
            tx.execute(
                "UPDATE artist_groups SET grp=?1 WHERE grp=?2",
                params![keep, other],
            )?;
        }
        (Some(_), Some(_)) => {}
    }
    Ok(())
}

fn artist_group_in(tx: &Transaction, artist: &str) -> Result<Option<i64>, Error> {
    Ok(tx
        .query_row(
            "SELECT grp FROM artist_groups WHERE artist=?1",
            [artist],
            |r| r.get(0),
        )
        .optional()?)
}

/// Next unused group number of `table` (`track_groups` or `artist_groups`).
fn next_id(tx: &Transaction, table: &str) -> Result<i64, Error> {
    Ok(tx.query_row(
        &format!("SELECT COALESCE(MAX(grp),0)+1 FROM {table}"),
        [],
        |r| r.get(0),
    )?)
}

fn method_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Isrc => "isrc",
        MergeMethod::Meta => "meta",
    }
}

fn to_u64(value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::Corrupt)
}

fn to_i64(value: u64) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::Corrupt)
}

impl Store {
    /// Group of the track, if it has been placed in one.
    pub fn group_of(&self, id: &TrackId) -> Result<Option<u64>, Error> {
        let grp: Option<i64> = self
            .read()?
            .query_row(
                "SELECT grp FROM track_groups WHERE track=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        grp.map(to_u64).transpose()
    }
    /// How the track joined its group: `isrc`, `meta` or `self`.
    pub fn group_method(&self, id: &TrackId) -> Result<Option<String>, Error> {
        Ok(self
            .read()?
            .query_row(
                "SELECT method FROM track_groups WHERE track=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Members of a group, ordered by track id. Unreadable ids are skipped.
    pub fn group_members(&self, grp: u64) -> Result<Vec<TrackId>, Error> {
        let c = self.read()?;
        let mut stmt = c.prepare("SELECT track FROM track_groups WHERE grp=?1 ORDER BY track")?;
        let ids = stmt
            .query_map([to_i64(grp)?], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids.iter().filter_map(|id| id.parse().ok()).collect())
    }
    /// Artist group of an artist id; artists are grouped only through merged tracks.
    pub fn artist_group(&self, id: &TrackId) -> Result<Option<u64>, Error> {
        let grp: Option<i64> = self
            .read()?
            .query_row(
                "SELECT grp FROM artist_groups WHERE artist=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        grp.map(to_u64).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_core::{Artist, Availability};

    fn root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("eg-groups-{name}-{}", std::process::id()))
    }

    fn track(id: &str, title: &str, artist: &str, duration_ms: u64, isrc: Option<&str>) -> Track {
        let id: TrackId = id.parse().unwrap();
        let artist_id: TrackId = format!("{}:artist-{artist}", id.provider())
            .parse()
            .unwrap();
        Track {
            id,
            title: title.into(),
            artists: vec![Artist {
                id: artist_id,
                name: artist.into(),
                cover: None,
            }],
            album: None,
            album_title: None,
            duration_ms,
            cover: None,
            availability: Availability::Available,
            isrc: isrc.map(str::to_owned),
        }
    }

    fn id(value: &str) -> TrackId {
        value.parse().unwrap()
    }

    #[test]
    fn new_track_gets_its_own_group() {
        let root = root("self");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track("fakea:1", "Song", "Artist", 200_000, None))
            .unwrap();
        assert!(store.group_of(&id("fakea:1")).unwrap().is_some());
        assert_eq!(
            store.group_method(&id("fakea:1")).unwrap().as_deref(),
            Some("self")
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_isrc_across_providers_shares_a_group() {
        let root = root("isrc");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track(
                "fakea:1",
                "Song",
                "Artist",
                200_000,
                Some("USRC17607839"),
            ))
            .unwrap();
        store
            .put_track(&track(
                "fakeb:1",
                "Other",
                "Someone",
                10_000,
                Some("usrc17607839"),
            ))
            .unwrap();
        let grp = store.group_of(&id("fakea:1")).unwrap();
        assert!(grp.is_some());
        assert_eq!(store.group_of(&id("fakeb:1")).unwrap(), grp);
        assert_eq!(
            store.group_method(&id("fakeb:1")).unwrap().as_deref(),
            Some("isrc")
        );
        assert_eq!(
            store.group_members(grp.unwrap()).unwrap(),
            [id("fakea:1"), id("fakeb:1")]
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn meta_match_uses_normalized_title_and_two_second_tolerance() {
        let root = root("meta");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track("fakea:1", "Song (feat. X)", "Artist", 200_000, None))
            .unwrap();
        store
            .put_track(&track("fakeb:1", "song", "ARTIST", 201_000, None))
            .unwrap();
        assert_eq!(
            store.group_of(&id("fakeb:1")).unwrap(),
            store.group_of(&id("fakea:1")).unwrap()
        );
        assert_eq!(
            store.group_method(&id("fakeb:1")).unwrap().as_deref(),
            Some("meta")
        );
        store
            .put_track(&track("fakea:2", "Far", "Band", 200_000, None))
            .unwrap();
        store
            .put_track(&track("fakeb:2", "Far", "Band", 203_000, None))
            .unwrap();
        assert_ne!(
            store.group_of(&id("fakeb:2")).unwrap(),
            store.group_of(&id("fakea:2")).unwrap()
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn same_provider_never_joins_a_group_twice() {
        let root = root("same-provider");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track(
                "fakea:1",
                "Song",
                "Artist",
                200_000,
                Some("USRC17607839"),
            ))
            .unwrap();
        store
            .put_track(&track(
                "fakea:2",
                "Song",
                "Artist",
                200_000,
                Some("USRC17607839"),
            ))
            .unwrap();
        assert_ne!(
            store.group_of(&id("fakea:2")).unwrap(),
            store.group_of(&id("fakea:1")).unwrap()
        );
        assert_eq!(
            store.group_method(&id("fakea:2")).unwrap().as_deref(),
            Some("self")
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artists_join_only_through_merged_tracks() {
        let root = root("artists");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track("fakea:1", "Lonely", "Same", 100_000, None))
            .unwrap();
        store
            .put_track(&track("fakeb:1", "Other", "Same", 300_000, None))
            .unwrap();
        assert_eq!(store.artist_group(&id("fakea:artist-Same")).unwrap(), None);
        assert_eq!(store.artist_group(&id("fakeb:artist-Same")).unwrap(), None);

        store
            .put_track(&track(
                "fakea:2",
                "Joined",
                "Same",
                150_000,
                Some("USRC17607839"),
            ))
            .unwrap();
        store
            .put_track(&track(
                "fakeb:2",
                "Joined",
                "Same",
                150_000,
                Some("USRC17607839"),
            ))
            .unwrap();
        let artist = store.artist_group(&id("fakea:artist-Same")).unwrap();
        assert!(artist.is_some());
        assert_eq!(
            store.artist_group(&id("fakeb:artist-Same")).unwrap(),
            artist
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conflicting_isrcs_block_a_meta_merge() {
        let root = root("conflict");
        let store = Store::open(&root).unwrap();
        store
            .put_track(&track(
                "fakea:1",
                "Song",
                "Artist",
                200_000,
                Some("USAAA1111111"),
            ))
            .unwrap();
        store
            .put_track(&track(
                "fakeb:1",
                "Song",
                "Artist",
                200_000,
                Some("USBBB2222222"),
            ))
            .unwrap();
        assert_ne!(
            store.group_of(&id("fakeb:1")).unwrap(),
            store.group_of(&id("fakea:1")).unwrap()
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rewriting_a_track_keeps_its_group() {
        let root = root("idempotent");
        let store = Store::open(&root).unwrap();
        let a = track("fakea:1", "Song", "Artist", 200_000, Some("USRC17607839"));
        let b = track("fakeb:1", "Song", "Artist", 200_000, Some("USRC17607839"));
        store.put_tracks(&[a.clone(), b]).unwrap();
        let grp = store.group_of(&id("fakea:1")).unwrap();
        store.put_track(&a).unwrap();
        store.put_track(&a).unwrap();
        assert_eq!(store.group_of(&id("fakea:1")).unwrap(), grp);
        let rows: i64 = store
            .read()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM track_groups WHERE track='fakea:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v10_backfills_groups_without_touching_tracks() {
        let root = root("migrate-v10");
        let store = Store::open(&root).unwrap();
        drop(store);
        let shared = track("fakea:1", "Song", "Artist", 200_000, Some("USRC17607839"));
        let twin = track("fakeb:1", "Song", "Artist", 200_000, Some("USRC17607839"));
        let alone = track("fakea:2", "Alone", "Solo", 90_000, None);
        {
            let db = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
            db.execute_batch(
                "DROP TABLE track_groups; DROP TABLE track_keys; DROP TABLE artist_groups; PRAGMA user_version=9;",
            )
            .unwrap();
            for t in [&shared, &twin, &alone] {
                db.execute(
                    "INSERT INTO tracks VALUES(?1,?2)",
                    params![t.id.to_string(), serde_json::to_string(t).unwrap()],
                )
                .unwrap();
            }
        }
        let store = Store::open(&root).unwrap();
        assert_eq!(
            store.group_of(&id("fakea:1")).unwrap(),
            store.group_of(&id("fakeb:1")).unwrap()
        );
        assert_ne!(
            store.group_of(&id("fakea:2")).unwrap(),
            store.group_of(&id("fakea:1")).unwrap()
        );
        assert_eq!(store.track(&id("fakeb:1")).unwrap(), Some(twin));
        assert_eq!(store.track(&id("fakea:2")).unwrap(), Some(alone));
        let c = store.read().unwrap();
        let count: i64 = c
            .query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        let version: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(version, 10);
        drop(c);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

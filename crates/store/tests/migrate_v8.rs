// SPDX-License-Identifier: GPL-3.0-or-later
//! A real v8 database (written by the v8 code at adbfa9e: schema before the profile and groups)
//! opens at the latest version and keeps likes, playlists, saved audio, history, playback
//! position and settings. The fixture was dumped from a database created by that code.
use eagle_core::{Track, TrackId};
use eagle_store::Store;
use rusqlite::Connection;
use std::str::FromStr;

fn third_track() -> Track {
    serde_json::from_value(serde_json::json!({
        "id": "yandex:3",
        "title": "Third",
        "artists": [],
        "album": null,
        "album_title": null,
        "duration_ms": 180000,
        "cover": null,
        "availability": "Available"
    }))
    .unwrap()
}

#[test]
fn v8_database_migrates_to_latest_without_losing_user_data() {
    let dir = std::env::temp_dir().join(format!("eagle-v8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Connection::open(dir.join("metadata.sqlite"))
        .unwrap()
        .execute_batch(include_str!("fixtures/v8.sql"))
        .unwrap();

    let store = Store::open(&dir).unwrap();

    // yandex:3 is liked but has no metadata yet, so it is not listed as a liked track.
    let liked: Vec<_> = store
        .liked_tracks(&[])
        .unwrap()
        .into_iter()
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(liked.len(), 2, "{liked:?}");
    assert!(liked.contains(&"yandex:1".to_owned()) && liked.contains(&"yandex:2".to_owned()));
    let unresolved = store.unresolved_likes("yandex", 10).unwrap();
    assert_eq!(unresolved, vec![TrackId::from_str("yandex:3").unwrap()]);

    let profile = store
        .setting("profile_id")
        .unwrap()
        .expect("profile created");
    assert_eq!(store.history(&profile).unwrap().len(), 2);
    assert_eq!(store.playback(&profile).unwrap().unwrap().position_ms, 4200);
    assert_eq!(store.setting("theme").unwrap().as_deref(), Some("dark"));
    assert_eq!(
        store.setting("account:yandex").unwrap().as_deref(),
        Some("u1")
    );
    // Offline rights stay with the provider account, not the profile.
    assert!(store.owns_audio("u1", "yandex:1").unwrap());
    let saved = store.downloads("u1").unwrap();
    assert_eq!(saved.len(), 1);
    store.put_tracks(&[third_track()]).unwrap();
    let liked = store.liked_tracks(&[]).unwrap();
    assert_eq!(liked.len(), 3, "{liked:?}");
    drop(store);

    let c = Connection::open(dir.join("metadata.sqlite")).unwrap();
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 14);
    let playlists: i64 = c
        .query_row("SELECT count(*) FROM playlists", [], |r| r.get(0))
        .unwrap();
    assert_eq!(playlists, 1);
    std::fs::remove_dir_all(dir).unwrap();
}

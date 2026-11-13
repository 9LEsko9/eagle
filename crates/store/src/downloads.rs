// SPDX-License-Identifier: GPL-3.0-or-later
//! Account-scoped download intent. Resource locations are deliberately never stored.
use crate::{Error, Store};
use eagle_core::{Quality, TrackId};
use rusqlite::{OptionalExtension, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadState {
    Queued,
    Running,
    Paused,
    Cancelled,
    Failed,
    /// Audio is pinned; artwork and metadata still need verification.
    AudioReady,
    Saved,
}
impl DownloadState {
    fn key(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::AudioReady => "audio_ready",
            Self::Saved => "saved",
        }
    }
    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            "audio_ready" => Ok(Self::AudioReady),
            "saved" => Ok(Self::Saved),
            _ => Err(Error::Corrupt),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadJob {
    pub id: i64,
    pub account: String,
    pub track: TrackId,
    pub title: Option<String>,
    pub quality: Quality,
    pub state: DownloadState,
    /// Invalidates an in-flight result after a user action or process restart.
    pub revision: i64,
    /// A fixed application message, never an HTTP body or a signed URL.
    pub error: Option<String>,
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DownloadJob> {
    let track: String = row.get(2)?;
    let quality: String = row.get(3)?;
    let state: String = row.get(4)?;
    let invalid = || rusqlite::Error::InvalidQuery;
    Ok(DownloadJob {
        id: row.get(0)?,
        account: row.get(1)?,
        track: track.parse().map_err(|_| invalid())?,
        title: row
            .get::<_, Option<String>>(7)?
            .and_then(|json| serde_json::from_str::<eagle_core::Track>(&json).ok())
            .map(|track| track.title),
        quality: match quality.as_str() {
            "standard" => Quality::Standard,
            "high" => Quality::High,
            _ => return Err(invalid()),
        },
        state: DownloadState::parse(&state).map_err(|_| invalid())?,
        revision: row.get(5)?,
        error: row.get(6)?,
    })
}
impl Store {
    pub fn downloads(&self, account: &str) -> Result<Vec<DownloadJob>, Error> {
        let c = self.read()?;
        let mut q = c.prepare("SELECT id,account,track,quality,state,revision,error,(SELECT json FROM tracks WHERE tracks.id=downloads.track) FROM downloads WHERE account=?1 ORDER BY id")?;
        Ok(q.query_map([account], row)?.collect::<Result<_, _>>()?)
    }
    /// Idempotent for active and completed jobs. Explicit retry resumes failed jobs.
    pub fn enqueue_download(
        &self,
        account: &str,
        track: &TrackId,
        quality: Quality,
    ) -> Result<(), Error> {
        let account = account.to_owned();
        if account.is_empty() {
            return Err(Error::Corrupt);
        }
        let track = track.to_string();
        self.write(move |c| {
            c.execute(
                "INSERT INTO downloads(account,track,quality,state) VALUES(?1,?2,?3,'queued') ON CONFLICT(account,track,quality) DO UPDATE SET state='queued',revision=downloads.revision+1,error=NULL WHERE downloads.state='cancelled'",
                params![account, track, quality.key()],
            )?;
            Ok(())
        })
    }
    /// Must run once before starting workers, after the previous process has stopped.
    pub fn recover_downloads(&self, account: &str) -> Result<(), Error> {
        let account = account.to_owned();
        self.write(move |c| {
            c.execute("UPDATE downloads SET state='queued',revision=revision+1,error=NULL WHERE account=?1 AND state='running'", [account])?;
            Ok(())
        })
    }
    /// Enqueue a displayed collection atomically; repeated tracks share one audio file.
    pub fn enqueue_downloads(
        &self,
        account: &str,
        tracks: &[TrackId],
        quality: Quality,
    ) -> Result<(), Error> {
        if account.is_empty() {
            return Err(Error::Corrupt);
        }
        let account = account.to_owned();
        let tracks = tracks.iter().map(ToString::to_string).collect::<Vec<_>>();
        self.write(move |c| {
            let tx = c.transaction()?;
            for track in tracks {
                tx.execute("INSERT INTO downloads(account,track,quality,state) VALUES(?1,?2,?3,'queued') ON CONFLICT(account,track,quality) DO UPDATE SET state='queued',revision=downloads.revision+1,error=NULL WHERE downloads.state='cancelled'", params![account, track, quality.key()])?;
            }
            tx.commit()?;
            Ok(())
        })
    }
    /// SQLite serializes claims; never claims more than two jobs for this account.
    pub fn claim_download(&self, account: &str) -> Result<Option<DownloadJob>, Error> {
        let account = account.to_owned();
        self.write(move |c| {
            let tx = c.transaction()?;
            let running: i64 = tx.query_row("SELECT COUNT(*) FROM downloads WHERE account=?1 AND state='running'", [&account], |r| r.get(0))?;
            if running >= 2 {
                return Ok(None);
            }
            let claimed: Option<(i64, String)> = tx.query_row("SELECT id,state FROM downloads WHERE account=?1 AND state IN ('queued','audio_ready') ORDER BY CASE state WHEN 'audio_ready' THEN 0 ELSE 1 END,id LIMIT 1", [&account], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((id, previous_state)) = claimed else { return Ok(None) };
            tx.execute("UPDATE downloads SET state='running',revision=revision+1,error=NULL WHERE id=?1", [id])?;
            let mut job = tx.query_row("SELECT id,account,track,quality,state,revision,error,(SELECT json FROM tracks WHERE tracks.id=downloads.track) FROM downloads WHERE id=?1", [id], row)?;
            if previous_state == "audio_ready" { job.state = DownloadState::AudioReady; }
            tx.commit()?;
            Ok(Some(job))
        })
    }
    /// A stale worker cannot undo pause, cancellation, retry, or recovery.
    pub fn finish_download(&self, job: &DownloadJob, success: bool) -> Result<bool, Error> {
        let (id, account, revision) = (job.id, job.account.clone(), job.revision);
        self.write(move |c| {
            Ok(c.execute("UPDATE downloads SET state=?1,error=?2,revision=revision+1 WHERE id=?3 AND account=?4 AND revision=?5 AND state='running'",
                params![if success { "audio_ready" } else { "failed" }, if success { None } else { Some("Не удалось завершить сохранение; можно повторить") }, id, account, revision])? == 1)
        })
    }
    /// Keep a specific, safe explanation instead of persisting a provider response.
    pub fn reject_download_quality(&self, job: &DownloadJob) -> Result<bool, Error> {
        let (id, account, revision) = (job.id, job.account.clone(), job.revision);
        self.write(move |c| {
            Ok(c.execute("UPDATE downloads SET state='failed',error='Выбранное качество недоступно для этого трека',revision=revision+1 WHERE id=?1 AND account=?2 AND revision=?3 AND state='running'",
                params![id, account, revision])? == 1)
        })
    }
    /// Publish only after audio, metadata, artwork and colors are durable.
    pub fn finish_saved_download(&self, job: &DownloadJob) -> Result<bool, Error> {
        let (id, account, revision) = (job.id, job.account.clone(), job.revision);
        self.write(move |c| {
            Ok(c.execute("UPDATE downloads SET state='saved',error=NULL,revision=revision+1 WHERE id=?1 AND account=?2 AND revision=?3 AND state='running'",
                params![id, account, revision])? == 1)
        })
    }
    /// Forget this account's completed save. Other accounts retain their references.
    pub fn forget_saved(
        &self,
        account: &str,
        id: i64,
    ) -> Result<Option<(DownloadJob, bool)>, Error> {
        let account = account.to_owned();
        self.write(move |c| {
            let tx = c.transaction()?;
            let job = tx.query_row("SELECT id,account,track,quality,state,revision,error,(SELECT json FROM tracks WHERE tracks.id=downloads.track) FROM downloads WHERE id=?1 AND account=?2 AND state='saved'", params![id, account], row).optional()?;
            let Some(job) = job else { return Ok(None) };
            tx.execute("UPDATE downloads SET state='cancelled',revision=revision+1,error=NULL WHERE id=?1 AND account=?2", params![id, account])?;
            let retained: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM downloads WHERE track=?1 AND quality=?2 AND state IN ('saved','audio_ready','running','queued','paused'))", params![job.track.to_string(), job.quality.key()], |row| row.get(0))?;
            tx.commit()?;
            Ok(Some((job, retained)))
        })
    }
    /// User actions are scoped to an account and invalidate the current attempt.
    pub fn control_download(
        &self,
        account: &str,
        id: i64,
        state: DownloadState,
    ) -> Result<bool, Error> {
        if !matches!(
            state,
            DownloadState::Queued | DownloadState::Paused | DownloadState::Cancelled
        ) {
            return Err(Error::Corrupt);
        }
        let account = account.to_owned();
        self.write(move |c| {
            // Completed audio is never undone by a late pause/cancel click.
            Ok(c.execute("UPDATE downloads SET state=?1,revision=revision+1,error=NULL WHERE id=?2 AND account=?3 AND state IN ('queued','running','audio_ready','paused','cancelled','failed')", params![state.key(), id, account])? == 1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn audio_ready_survives_restart_and_is_claimed_before_queued_audio() {
        let root = std::env::temp_dir().join(format!("eg-ready-priority-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let first = "yandex:1".parse().unwrap();
        let second = "yandex:2".parse().unwrap();
        store.enqueue_download("a", &first, Quality::High).unwrap();
        let job = store.claim_download("a").unwrap().unwrap();
        assert!(store.finish_download(&job, true).unwrap());
        store.enqueue_download("a", &second, Quality::High).unwrap();
        drop(store);
        let store = Store::open(&root).unwrap();
        store.recover_downloads("a").unwrap();
        let claimed = store.claim_download("a").unwrap().unwrap();
        assert_eq!(claimed.track, first);
        assert_eq!(claimed.state, DownloadState::AudioReady);
        assert!(store.finish_saved_download(&claimed).unwrap());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collection_enqueue_is_atomic_idempotent_and_account_scoped() {
        let root = std::env::temp_dir().join(format!("eg-batch-downloads-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let tracks = (0..100)
            .map(|id| format!("yandex:{id}").parse().unwrap())
            .collect::<Vec<_>>();
        store
            .enqueue_downloads("a", &tracks, Quality::High)
            .unwrap();
        store
            .enqueue_downloads("a", &tracks, Quality::High)
            .unwrap();
        assert_eq!(store.downloads("a").unwrap().len(), 100);
        assert!(store.downloads("b").unwrap().is_empty());
        assert!(store.enqueue_downloads("", &tracks, Quality::High).is_err());
        let job = store.claim_download("a").unwrap().unwrap();
        store
            .control_download("a", job.id, DownloadState::Paused)
            .unwrap();
        store
            .enqueue_downloads("a", &tracks, Quality::High)
            .unwrap();
        assert_eq!(
            store.downloads("a").unwrap()[0].state,
            DownloadState::Paused
        );
        drop(store);
        let store = Store::open(&root).unwrap();
        assert_eq!(store.downloads("a").unwrap().len(), 100);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn saving_again_requeues_only_cancelled_jobs() {
        for batch in [false, true] {
            let root =
                std::env::temp_dir().join(format!("eg-resave-{}-{batch}", std::process::id()));
            let store = Store::open(&root).unwrap();
            let track = "yandex:1".parse().unwrap();
            let enqueue = || {
                if batch {
                    store
                        .enqueue_downloads("a", std::slice::from_ref(&track), Quality::High)
                        .unwrap();
                } else {
                    store.enqueue_download("a", &track, Quality::High).unwrap();
                }
            };
            enqueue();
            let stale = store.claim_download("a").unwrap().unwrap();
            enqueue();
            assert_eq!(store.downloads("a").unwrap()[0].revision, stale.revision);
            store
                .control_download("a", stale.id, DownloadState::Cancelled)
                .unwrap();
            let cancelled_revision = store.downloads("a").unwrap()[0].revision;
            enqueue();
            let queued = &store.downloads("a").unwrap()[0];
            assert_eq!(queued.state, DownloadState::Queued);
            assert!(queued.revision > cancelled_revision);
            let job = store.claim_download("a").unwrap().unwrap();
            assert!(!store.finish_download(&stale, true).unwrap());
            assert!(store.finish_download(&job, true).unwrap());
            enqueue();
            assert_eq!(
                store.downloads("a").unwrap()[0].state,
                DownloadState::AudioReady
            );
            let ready = store.claim_download("a").unwrap().unwrap();
            assert!(store.finish_saved_download(&ready).unwrap());
            enqueue();
            assert_eq!(store.downloads("a").unwrap()[0].state, DownloadState::Saved);
            store.forget_saved("a", ready.id).unwrap().unwrap();
            enqueue();
            assert_eq!(
                store.downloads("a").unwrap()[0].state,
                DownloadState::Queued
            );
            assert!(store.claim_download("a").unwrap().is_some());
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn quality_failure_is_durable_and_cannot_override_cancellation() {
        let root = std::env::temp_dir().join(format!("eg-quality-{}", std::process::id()));
        let store = Store::open(&root).unwrap();
        let track = "yandex:1".parse().unwrap();
        store.enqueue_download("a", &track, Quality::High).unwrap();
        let job = store.claim_download("a").unwrap().unwrap();
        assert!(store.reject_download_quality(&job).unwrap());
        drop(store);
        let store = Store::open(&root).unwrap();
        let failed = &store.downloads("a").unwrap()[0];
        assert_eq!(failed.state, DownloadState::Failed);
        assert_eq!(failed.quality, Quality::High);
        assert_eq!(
            failed.error.as_deref(),
            Some("Выбранное качество недоступно для этого трека")
        );
        store
            .control_download("a", job.id, DownloadState::Queued)
            .unwrap();
        let retry = store.claim_download("a").unwrap().unwrap();
        store
            .control_download("a", job.id, DownloadState::Cancelled)
            .unwrap();
        assert!(!store.reject_download_quality(&retry).unwrap());
        assert_eq!(
            store.downloads("a").unwrap()[0].state,
            DownloadState::Cancelled
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn migrates_v2_without_losing_playback_or_settings() {
        let root = std::env::temp_dir().join(format!("eg-downloads-v2-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let c = rusqlite::Connection::open(root.join("metadata.sqlite")).unwrap();
        c.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE tracks(id TEXT PRIMARY KEY,json TEXT NOT NULL); CREATE TABLE playback(account TEXT PRIMARY KEY,json TEXT NOT NULL); INSERT INTO settings VALUES('existing','kept'); PRAGMA user_version=2;").unwrap();
        let state = crate::PlaybackState {
            shuffle: true,
            ..Default::default()
        };
        c.execute(
            "INSERT INTO playback VALUES('a',?1)",
            [serde_json::to_string(&state).unwrap()],
        )
        .unwrap();
        drop(c);
        let store = Store::open(&root).unwrap();
        assert!(store.playback("a").unwrap().unwrap().shuffle);
        assert_eq!(store.setting("existing").unwrap().as_deref(), Some("kept"));
        assert!(store.downloads("a").unwrap().is_empty());
        let version: i64 = store
            .read()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 10);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn claims_are_bounded_account_scoped_and_stale_results_cannot_resume_jobs() {
        let root = std::env::temp_dir().join(format!("eg-downloads-{}", std::process::id()));
        let store = Arc::new(Store::open(&root).unwrap());
        for n in 1..=4 {
            let id = format!("yandex:{n}").parse().unwrap();
            store.enqueue_download("a", &id, Quality::High).unwrap();
            store.enqueue_download("a", &id, Quality::High).unwrap();
        }
        let id = "yandex:1".parse().unwrap();
        store.enqueue_download("b", &id, Quality::Standard).unwrap();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || store.claim_download("a").unwrap())
            })
            .collect();
        let claimed: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(claimed.len(), 2);
        assert_ne!(claimed[0].id, claimed[1].id);
        assert!(
            !store
                .control_download("b", claimed[0].id, DownloadState::Cancelled)
                .unwrap()
        );
        assert!(
            store
                .control_download("a", claimed[0].id, DownloadState::Paused)
                .unwrap()
        );
        assert!(!store.finish_download(&claimed[0], true).unwrap());
        assert!(!store.finish_saved_download(&claimed[0]).unwrap());
        store.recover_downloads("a").unwrap();
        assert!(!store.finish_download(&claimed[1], true).unwrap());
        drop(store);
        let store = Store::open(&root).unwrap();
        let a = store.downloads("a").unwrap();
        assert_eq!(a.len(), 4);
        assert_eq!(
            a.iter()
                .filter(|j| j.state == DownloadState::Paused)
                .count(),
            1
        );
        assert_eq!(
            a.iter()
                .filter(|j| j.state == DownloadState::Queued)
                .count(),
            3
        );
        let b = store.claim_download("b").unwrap().unwrap();
        assert_eq!(b.quality, Quality::Standard);
        assert!(store.finish_download(&b, false).unwrap());
        assert_eq!(
            store.downloads("b").unwrap()[0].state,
            DownloadState::Failed
        );
        store
            .control_download("b", b.id, DownloadState::Queued)
            .unwrap();
        let retry = store.claim_download("b").unwrap().unwrap();
        assert!(retry.revision > b.revision);
        assert!(!store.finish_download(&b, true).unwrap());
        assert!(store.finish_download(&retry, true).unwrap());
        assert_eq!(
            store.downloads("b").unwrap()[0].state,
            DownloadState::AudioReady
        );
        store.recover_downloads("b").unwrap();
        let finalize = store.claim_download("b").unwrap().unwrap();
        assert!(store.finish_saved_download(&finalize).unwrap());
        store.recover_downloads("b").unwrap();
        assert_eq!(store.downloads("b").unwrap()[0].state, DownloadState::Saved);
        assert!(!store.finish_saved_download(&finalize).unwrap());
        store.enqueue_download("a", &id, Quality::Standard).unwrap();
        let qualities = store.downloads("a").unwrap();
        assert_eq!(qualities.len(), 5);
        assert_eq!(qualities[0].quality, Quality::High);
        assert_eq!(qualities[4].quality, Quality::Standard);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}

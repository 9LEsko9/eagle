// SPDX-License-Identifier: GPL-3.0-or-later
//! Likes waiting to be sent to a provider. The newest intent per (provider, track) wins; an
//! attempt with an unknown outcome becomes uncertain and is never resent automatically.
use crate::{Error, Store};
use rusqlite::{OptionalExtension, params};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LikeJob {
    pub provider: String,
    pub track: String,
    pub liked: bool,
}

/// Seconds to wait after the `attempts`-th failure: 30, 60, 120 ... up to 1800.
fn backoff_secs(attempts: i64) -> i64 {
    let exponent = (attempts - 1).clamp(0, 16);
    (30_i64 << exponent).min(1800)
}

impl Store {
    /// Pending jobs of `provider` that are due at `now` (seconds), oldest first.
    pub fn like_jobs(&self, provider: &str, now: i64) -> Result<Vec<LikeJob>, Error> {
        let c = self.read()?;
        let mut stmt = c.prepare(
            "SELECT provider,track,liked FROM like_outbox WHERE provider=?1 AND state='pending' AND retry_at<=?2 ORDER BY hlc",
        )?;
        Ok(stmt
            .query_map(params![provider, now], |r| {
                Ok(LikeJob {
                    provider: r.get(0)?,
                    track: r.get(1)?,
                    liked: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    /// The provider accepted the job. A newer intent for the same track stays queued.
    pub fn finish_like_job(&self, job: &LikeJob) -> Result<bool, Error> {
        let job = job.clone();
        self.write(move |c| {
            Ok(c.execute(
                "DELETE FROM like_outbox WHERE provider=?1 AND track=?2 AND liked=?3",
                params![job.provider, job.track, job.liked],
            )? == 1)
        })
    }

    /// The outcome of an attempt is unknown: keep the job but never resend it by itself.
    pub fn mark_like_job_uncertain(&self, job: &LikeJob) -> Result<bool, Error> {
        let job = job.clone();
        self.write(move |c| {
            Ok(c.execute(
                "UPDATE like_outbox SET state='uncertain' WHERE provider=?1 AND track=?2 AND liked=?3 AND state='pending'",
                params![job.provider, job.track, job.liked],
            )? == 1)
        })
    }

    /// The provider failed or asked to slow down: retry after a delay that doubles with every
    /// failed attempt (30 s, 60 s, ... capped at 30 min). Returns the new `retry_at` (seconds),
    /// or `None` when the job is no longer pending.
    pub fn defer_like_job_backoff(&self, job: &LikeJob, now: i64) -> Result<Option<i64>, Error> {
        let job = job.clone();
        self.write(move |c| {
            let attempts: Option<i64> = c
                .query_row(
                    "SELECT attempts FROM like_outbox WHERE provider=?1 AND track=?2 AND liked=?3 AND state='pending'",
                    params![job.provider, job.track, job.liked],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(attempts) = attempts else {
                return Ok(None);
            };
            let attempts = attempts + 1;
            let retry_at = now + backoff_secs(attempts);
            c.execute(
                "UPDATE like_outbox SET attempts=?4, retry_at=?5 WHERE provider=?1 AND track=?2 AND liked=?3 AND state='pending'",
                params![job.provider, job.track, job.liked, attempts, retry_at],
            )?;
            Ok(Some(retry_at))
        })
    }

    /// The earliest `retry_at` after `now` among pending jobs of every provider. Jobs that are due
    /// but were not sent (for example, after a sign-in failure) are not a deadline.
    pub fn next_like_retry_at(&self, now: i64) -> Result<Option<i64>, Error> {
        let c = self.read()?;
        Ok(c.query_row(
            "SELECT MIN(retry_at) FROM like_outbox WHERE state='pending' AND retry_at>?1",
            [now],
            |r| r.get(0),
        )?)
    }

    /// The network came back: every pending job becomes due now. Returns how many were woken.
    pub fn wake_like_jobs(&self) -> Result<usize, Error> {
        self.write(|c| {
            Ok(c.execute(
                "UPDATE like_outbox SET retry_at=0 WHERE state='pending' AND retry_at>0",
                [],
            )?)
        })
    }

    /// Jobs whose outcome is unknown, for diagnostics.
    pub fn uncertain_like_jobs(&self, provider: &str) -> Result<Vec<LikeJob>, Error> {
        let c = self.read()?;
        let mut stmt = c.prepare(
            "SELECT provider,track,liked FROM like_outbox WHERE provider=?1 AND state='uncertain' ORDER BY hlc",
        )?;
        Ok(stmt
            .query_map([provider], |r| {
                Ok(LikeJob {
                    provider: r.get(0)?,
                    track: r.get(1)?,
                    liked: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    fn open(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("eagle-outbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    /// A pending like of `track` for `fake`, due immediately.
    fn enqueue(store: &Store, track: &str) {
        let track = track.to_owned();
        store
            .write(move |c| {
                Ok(c.execute(
                    "INSERT INTO like_outbox(provider,track,liked,state,hlc) VALUES('fake',?1,1,'pending',1)",
                    [track],
                )?)
            })
            .unwrap();
    }

    #[test]
    fn network_failures_back_off_and_never_spin() {
        let (store, dir) = open("backoff");
        enqueue(&store, "fake:1");
        let job = store.like_jobs("fake", 0).unwrap().remove(0);
        assert_eq!(
            store.defer_like_job_backoff(&job, 1000).unwrap(),
            Some(1030)
        );
        assert_eq!(
            store.defer_like_job_backoff(&job, 1030).unwrap(),
            Some(1090)
        );
        // Delays double: 120, 240, 480, 960, then capped at 1800 s.
        let mut last = 1090;
        for _ in 0..5 {
            last = store.defer_like_job_backoff(&job, last).unwrap().unwrap();
        }
        assert_eq!(last, 1090 + 120 + 240 + 480 + 960 + 1800);
        assert_eq!(store.next_like_retry_at(1000).unwrap(), Some(last));
        assert!(store.like_jobs("fake", last - 1).unwrap().is_empty());
        assert_eq!(store.like_jobs("fake", last).unwrap().len(), 1);
        // Once due, the job is not a timer: an unsent due job must not arm a deadline.
        assert_eq!(store.next_like_retry_at(last).unwrap(), None);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_outbox_has_no_retry_deadline() {
        let (store, dir) = open("empty");
        assert_eq!(store.next_like_retry_at(0).unwrap(), None);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn woken_jobs_are_due_at_once() {
        let (store, dir) = open("woken");
        enqueue(&store, "fake:1");
        let job = store.like_jobs("fake", 0).unwrap().remove(0);
        store.defer_like_job_backoff(&job, 1000).unwrap();
        assert!(store.like_jobs("fake", 1000).unwrap().is_empty());
        assert_eq!(store.wake_like_jobs().unwrap(), 1);
        assert_eq!(store.like_jobs("fake", 1000).unwrap().len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

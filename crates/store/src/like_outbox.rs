// SPDX-License-Identifier: GPL-3.0-or-later
//! Likes waiting to be sent to a provider. The newest intent per (provider, track) wins; an
//! attempt with an unknown outcome becomes uncertain and is never resent automatically.
use crate::{Error, Store};
use rusqlite::params;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LikeJob {
    pub provider: String,
    pub track: String,
    pub liked: bool,
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

    /// The provider asked to slow down: try again at `retry_at` (seconds).
    pub fn defer_like_job(&self, job: &LikeJob, retry_at: i64) -> Result<bool, Error> {
        let job = job.clone();
        self.write(move |c| {
            Ok(c.execute(
                "UPDATE like_outbox SET retry_at=?4 WHERE provider=?1 AND track=?2 AND liked=?3 AND state='pending'",
                params![job.provider, job.track, job.liked, retry_at],
            )? == 1)
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

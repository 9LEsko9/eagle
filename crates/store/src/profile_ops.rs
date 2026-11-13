// SPDX-License-Identifier: GPL-3.0-or-later
//! Profile op journal. Every profile change is an op `(device, seq, hlc, kind, key, body)`; the
//! state of a key is the op with the greatest `(hlc, device)`, so applying ops is idempotent and
//! independent of order. `profile_state` is also the snapshot that lets the log be compacted.
use crate::{Error, Store, groups};
use eagle_core::Track;
use rusqlite::{OptionalExtension, Transaction, params};
use std::collections::HashMap;

pub(crate) const KIND_LIKE: &str = "like";

/// Superseded ops are dropped at once; the winner of a key leaves the log after this long.
const RETENTION_MS: i64 = 90 * 24 * 3600 * 1000;
/// Compaction runs at most this often (seconds).
const COMPACT_EVERY: i64 = 24 * 3600;
const COUNTER_BITS: u32 = 16;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS profile_ops(device TEXT NOT NULL, seq INTEGER NOT NULL, hlc INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(device, seq));
CREATE INDEX IF NOT EXISTS profile_ops_key ON profile_ops(kind, key);
CREATE TABLE IF NOT EXISTS profile_state(kind TEXT NOT NULL, key TEXT NOT NULL, hlc INTEGER NOT NULL, device TEXT NOT NULL, body TEXT NOT NULL, deleted INTEGER NOT NULL, PRIMARY KEY(kind, key));
CREATE TABLE IF NOT EXISTS profile_devices(device TEXT PRIMARY KEY, seen_seq INTEGER NOT NULL, compacted_seq INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS liked(grp INTEGER PRIMARY KEY, at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS liked_at ON liked(at);
CREATE TABLE IF NOT EXISTS like_outbox(provider TEXT NOT NULL, track TEXT NOT NULL, liked INTEGER NOT NULL, state TEXT NOT NULL, hlc INTEGER NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(provider, track));
CREATE TABLE IF NOT EXISTS provider_liked_seen(provider TEXT NOT NULL, track TEXT NOT NULL, synced_at INTEGER NOT NULL, PRIMARY KEY(provider, track));
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub device: String,
    /// Per-device counter. `0` marks a state-only op synthesized from the snapshot.
    pub seq: u64,
    /// Hybrid logical clock: wall milliseconds shifted by a 16-bit counter.
    pub hlc: u64,
    pub kind: String,
    pub key: String,
    pub body: String,
}

fn wall_hlc() -> i64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    ms << COUNTER_BITS
}

/// Next local HLC: monotonic even when the wall clock steps back.
pub(crate) fn next_hlc(tx: &Transaction) -> Result<i64, Error> {
    let last: i64 = tx
        .query_row("SELECT value FROM settings WHERE key='hlc'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let next = wall_hlc().max(last + 1);
    tx.execute(
        "INSERT OR REPLACE INTO settings VALUES('hlc',?1)",
        [next.to_string()],
    )?;
    Ok(next)
}

/// A remote clock may be ahead of ours; later local ops must still order after it.
fn observe_hlc(tx: &Transaction, remote: i64) -> Result<(), Error> {
    let last: i64 = tx
        .query_row("SELECT value FROM settings WHERE key='hlc'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if remote > last {
        tx.execute(
            "INSERT OR REPLACE INTO settings VALUES('hlc',?1)",
            [remote.to_string()],
        )?;
    }
    Ok(())
}

pub(crate) fn device_id(tx: &Transaction) -> Result<String, Error> {
    tx.query_row(
        "SELECT value FROM settings WHERE key='device_id'",
        [],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(Error::Corrupt)
}

fn is_tombstone(kind: &str, body: &str) -> bool {
    kind == KIND_LIKE
        && serde_json::from_str::<serde_json::Value>(body)
            .map(|v| v.get("liked").and_then(|l| l.as_bool()) != Some(true))
            .unwrap_or(true)
}

/// Records a local change: log entry plus state. Returns the op's HLC.
pub(crate) fn append_local(
    tx: &Transaction,
    kind: &str,
    key: &str,
    body: String,
) -> Result<i64, Error> {
    let device = device_id(tx)?;
    let hlc = next_hlc(tx)?;
    // `seen_seq` never goes down, so a compacted-away seq is not reused.
    let seq: i64 = tx
        .query_row(
            "SELECT seen_seq+1 FROM profile_devices WHERE device=?1",
            [&device],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(1);
    tx.execute(
        "INSERT INTO profile_ops VALUES(?1,?2,?3,?4,?5,?6)",
        params![device, seq, hlc, kind, key, body],
    )?;
    set_state(tx, &device, hlc, kind, key, &body)?;
    tx.execute(
        "INSERT INTO profile_devices(device,seen_seq) VALUES(?1,?2) ON CONFLICT(device) DO UPDATE SET seen_seq=MAX(seen_seq,?2)",
        params![device, seq],
    )?;
    Ok(hlc)
}

/// Keeps the op with the greatest `(hlc, device)`. Returns whether `state` changed.
fn set_state(
    tx: &Transaction,
    device: &str,
    hlc: i64,
    kind: &str,
    key: &str,
    body: &str,
) -> Result<bool, Error> {
    let current: Option<(i64, String)> = tx
        .query_row(
            "SELECT hlc,device FROM profile_state WHERE kind=?1 AND key=?2",
            [kind, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((cur_hlc, cur_device)) = current
        && (cur_hlc, cur_device.as_str()) >= (hlc, device)
    {
        return Ok(false);
    }
    tx.execute(
        "INSERT OR REPLACE INTO profile_state VALUES(?1,?2,?3,?4,?5,?6)",
        params![kind, key, hlc, device, body, is_tombstone(kind, body)],
    )?;
    Ok(true)
}

pub(crate) const LIKE_AT_SQL: &str = "SELECT MAX(s.hlc) FROM track_groups g CROSS JOIN profile_state s ON s.kind='like' AND s.key=g.track WHERE g.grp=?1 AND s.deleted=0";

/// Brings `liked` in line with `profile_state` for the group of `track`.
pub(crate) fn project_like(tx: &Transaction, track: &str) -> Result<(), Error> {
    let grp: Option<i64> = tx
        .query_row(
            "SELECT grp FROM track_groups WHERE track=?1",
            [track],
            |r| r.get(0),
        )
        .optional()?;
    let Some(grp) = grp else {
        return Ok(());
    };
    // CROSS JOIN fixes the order: from the group's few members into `profile_state` by key. The
    // planner's own choice walks every like for each call, which is quadratic in a full sync.
    let at: Option<i64> = tx.query_row(LIKE_AT_SQL, [grp], |r| r.get(0))?;
    match at {
        Some(at) => tx.execute(
            "INSERT OR REPLACE INTO liked VALUES(?1,?2)",
            params![grp, at],
        )?,
        None => tx.execute("DELETE FROM liked WHERE grp=?1", [grp])?,
    };
    Ok(())
}

/// A liked track that this device does not know yet is stored from the op's snapshot.
fn store_snapshot(tx: &Transaction, body: &str) -> Result<(), Error> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| Error::Corrupt)?;
    let Some(snapshot) = value.get("track") else {
        return Ok(());
    };
    let track: Track = serde_json::from_value(snapshot.clone()).map_err(|_| Error::Corrupt)?;
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
        [track.id.to_string()],
        |r| r.get(0),
    )?;
    if !known {
        tx.execute(
            "INSERT INTO tracks VALUES(?1,?2)",
            params![
                track.id.to_string(),
                serde_json::to_string(&track).map_err(|_| Error::Corrupt)?
            ],
        )?;
        groups::assign(tx, &track)?;
    }
    Ok(())
}

pub(crate) fn apply(tx: &Transaction, ops: &[Op]) -> Result<(), Error> {
    let mut touched = Vec::new();
    for op in ops {
        let hlc = i64::try_from(op.hlc).map_err(|_| Error::Corrupt)?;
        let seq = i64::try_from(op.seq).map_err(|_| Error::Corrupt)?;
        if op.seq > 0 {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO profile_ops VALUES(?1,?2,?3,?4,?5,?6)",
                params![op.device, seq, hlc, op.kind, op.key, op.body],
            )?;
            tx.execute(
                "INSERT INTO profile_devices(device,seen_seq) VALUES(?1,?2) ON CONFLICT(device) DO UPDATE SET seen_seq=MAX(seen_seq,?2)",
                params![op.device, seq],
            )?;
            if inserted == 0 {
                continue;
            }
        }
        observe_hlc(tx, hlc)?;
        if op.kind == KIND_LIKE && !is_tombstone(&op.kind, &op.body) {
            store_snapshot(tx, &op.body)?;
        }
        if set_state(tx, &op.device, hlc, &op.kind, &op.key, &op.body)? && op.kind == KIND_LIKE {
            touched.push(op.key.clone());
        }
    }
    for key in touched {
        project_like(tx, &key)?;
    }
    Ok(())
}

/// Drops superseded ops, winners past retention and old tombstones. The state table is the
/// snapshot, so peers that fall behind the log are served from it.
pub(crate) fn compact(tx: &Transaction) -> Result<(), Error> {
    tx.execute(
        "DELETE FROM profile_ops WHERE NOT EXISTS (SELECT 1 FROM profile_state s WHERE s.kind=profile_ops.kind AND s.key=profile_ops.key AND s.hlc=profile_ops.hlc AND s.device=profile_ops.device)",
        [],
    )?;
    let cutoff = wall_hlc() - (RETENTION_MS << COUNTER_BITS);
    tx.execute(
        "UPDATE profile_devices SET compacted_seq=MAX(compacted_seq, COALESCE((SELECT MAX(seq) FROM profile_ops p WHERE p.device=profile_devices.device AND p.hlc<?1),0))",
        [cutoff],
    )?;
    tx.execute("DELETE FROM profile_ops WHERE hlc<?1", [cutoff])?;
    tx.execute(
        "DELETE FROM profile_state WHERE deleted=1 AND hlc<?1",
        [cutoff],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO settings VALUES('profile_compacted_at',?1)",
        [crate::now().to_string()],
    )?;
    Ok(())
}

impl Store {
    /// Ops a peer that has seen `seen[device]` of each device is missing. A peer behind the
    /// compacted part of the log also gets the snapshot as state-only ops (`seq == 0`).
    pub fn ops_since(&self, seen: &HashMap<String, u64>) -> Result<Vec<Op>, Error> {
        let c = self.read()?;
        let devices: Vec<(String, i64)> = {
            let mut stmt = c.prepare("SELECT device,compacted_seq FROM profile_devices")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let row = |r: &rusqlite::Row| -> rusqlite::Result<Op> {
            Ok(Op {
                device: r.get(0)?,
                seq: r.get::<_, i64>(1)? as u64,
                hlc: r.get::<_, i64>(2)? as u64,
                kind: r.get(3)?,
                key: r.get(4)?,
                body: r.get(5)?,
            })
        };
        let mut ops = Vec::new();
        let mut behind = false;
        for (device, compacted) in &devices {
            let from = seen.get(device).copied().unwrap_or(0) as i64;
            behind |= from < *compacted;
            let mut stmt = c.prepare(
                "SELECT device,seq,hlc,kind,key,body FROM profile_ops WHERE device=?1 AND seq>?2 ORDER BY seq",
            )?;
            for op in stmt.query_map(params![device, from], row)? {
                ops.push(op?);
            }
        }
        if behind {
            let mut stmt = c.prepare("SELECT device,0,hlc,kind,key,body FROM profile_state")?;
            for op in stmt.query_map([], row)? {
                ops.push(op?);
            }
        }
        Ok(ops)
    }

    /// Idempotent and order-independent: applying the same ops again, or in another order,
    /// leaves the same state.
    pub fn apply_ops(&self, ops: Vec<Op>) -> Result<(), Error> {
        self.write(move |c| {
            let tx = c.transaction()?;
            apply(&tx, &ops)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Compacts the journal now.
    pub fn compact_profile(&self) -> Result<(), Error> {
        self.write(|c| {
            let tx = c.transaction()?;
            compact(&tx)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Compacts at most once a day; called when the store opens.
    pub(crate) fn compact_profile_if_due(&self) {
        let due = match self.setting("profile_compacted_at") {
            Ok(Some(at)) => at
                .parse::<i64>()
                .map_or(true, |at| crate::now() - at >= COMPACT_EVERY),
            Ok(None) => true,
            Err(_) => false,
        };
        if due && let Err(error) = self.compact_profile() {
            log::warn!(error = error.to_string().as_str(); "profile journal compaction failed");
        }
    }

    /// Number of ops kept in the log (diagnostics and tests).
    pub fn profile_op_count(&self) -> Result<u64, Error> {
        let count: i64 = self
            .read()?
            .query_row("SELECT COUNT(*) FROM profile_ops", [], |r| r.get(0))?;
        Ok(count as u64)
    }
}

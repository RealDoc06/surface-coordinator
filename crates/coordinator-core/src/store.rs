//! SQLite storage.
//!
//! One connection behind a mutex, used from async code through
//! [`Store::call`], which runs the closure on the blocking pool. The query
//! functions themselves are synchronous and take the connection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::Error;

const DB_FILE: &str = "coordinator.db";

/// Appended to stored output when it was cut at the cap.
pub const TRUNCATION_MARKER: &str = "\n[output truncated]\n";

/// Schema migrations in order. `PRAGMA user_version` records how many have
/// been applied; later milestones append to this list.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE enrollment_keys (
        id      TEXT PRIMARY KEY,
        label   TEXT NOT NULL,
        secret  TEXT NOT NULL,
        created INTEGER NOT NULL,
        revoked INTEGER
    );
    CREATE TABLE devices (
        id                TEXT PRIMARY KEY,
        enrollment_key_id TEXT NOT NULL REFERENCES enrollment_keys(id),
        hostname          TEXT NOT NULL,
        os                TEXT NOT NULL,
        client_version    TEXT NOT NULL,
        first_seen        INTEGER NOT NULL,
        last_seen         INTEGER NOT NULL,
        online            INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE jobs (
        id         TEXT PRIMARY KEY,
        kind       TEXT NOT NULL,
        payload    TEXT NOT NULL,
        created_by TEXT,
        created    INTEGER NOT NULL
    );
    CREATE TABLE job_targets (
        id               TEXT PRIMARY KEY,
        job_id           TEXT NOT NULL REFERENCES jobs(id),
        device_id        TEXT NOT NULL REFERENCES devices(id),
        state            TEXT NOT NULL,
        exit_code        INTEGER,
        reason           TEXT,
        message          TEXT,
        output           TEXT NOT NULL DEFAULT '',
        output_truncated INTEGER NOT NULL DEFAULT 0,
        last_seq         INTEGER NOT NULL DEFAULT 0,
        created          INTEGER NOT NULL,
        started          INTEGER,
        finished         INTEGER
    );
    CREATE INDEX job_targets_by_device ON job_targets(device_id, state);
    CREATE INDEX job_targets_by_job ON job_targets(job_id);
"];

/// Current time as Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (creating if needed) the database in `data_dir` and migrates it.
    pub fn open(data_dir: &Path) -> Result<Store, Error> {
        std::fs::create_dir_all(data_dir)?;
        let path: PathBuf = data_dir.join(DB_FILE);
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Store { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Runs `f` with the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut conn)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
        .map_err(Error::from)
    }

    /// Runs `f` with the connection on the calling thread.
    pub fn call_blocking<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> Result<T, Error> {
        let mut conn = self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(f(&mut conn)?)
    }
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let applied: usize = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index + 1)?;
        tx.commit()?;
    }
    Ok(())
}

// ---------------------------------------------------------------- keys

/// An enrollment key. The secret is shared with clients at install time and
/// never travels over the wire.
#[derive(Debug, Clone)]
pub struct EnrollmentKey {
    pub id: String,
    pub label: String,
    pub secret: String,
    pub created: i64,
    pub revoked: Option<i64>,
}

fn key_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnrollmentKey> {
    Ok(EnrollmentKey {
        id: row.get(0)?,
        label: row.get(1)?,
        secret: row.get(2)?,
        created: row.get(3)?,
        revoked: row.get(4)?,
    })
}

pub fn create_enrollment_key(conn: &Connection, label: &str) -> rusqlite::Result<EnrollmentKey> {
    let key = EnrollmentKey {
        id: uuid::Uuid::new_v4().simple().to_string(),
        label: label.to_owned(),
        secret: hex::encode(rand::random::<[u8; 32]>()),
        created: now_ms(),
        revoked: None,
    };
    conn.execute(
        "INSERT INTO enrollment_keys (id, label, secret, created) VALUES (?1, ?2, ?3, ?4)",
        params![key.id, key.label, key.secret, key.created],
    )?;
    Ok(key)
}

pub fn get_enrollment_key(conn: &Connection, id: &str) -> rusqlite::Result<Option<EnrollmentKey>> {
    conn.query_row(
        "SELECT id, label, secret, created, revoked FROM enrollment_keys WHERE id = ?1",
        [id],
        key_from_row,
    )
    .optional()
}

pub fn list_enrollment_keys(conn: &Connection) -> rusqlite::Result<Vec<EnrollmentKey>> {
    conn.prepare(
        "SELECT id, label, secret, created, revoked FROM enrollment_keys ORDER BY created, rowid",
    )?
    .query_map([], key_from_row)?
    .collect()
}

/// Returns false if the key does not exist or is already revoked.
pub fn revoke_enrollment_key(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE enrollment_keys SET revoked = ?2 WHERE id = ?1 AND revoked IS NULL",
        params![id, now_ms()],
    )?;
    Ok(changed > 0)
}

// ---------------------------------------------------------------- devices

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub enrollment_key_id: String,
    pub hostname: String,
    pub os: String,
    pub client_version: String,
    pub online: bool,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

pub struct DeviceInfo<'a> {
    pub id: &'a str,
    pub enrollment_key_id: &'a str,
    pub hostname: &'a str,
    pub os: &'a str,
    pub client_version: &'a str,
}

/// Records a device that completed the handshake and marks it online.
pub fn upsert_device_online(conn: &Connection, device: &DeviceInfo<'_>) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO devices (id, enrollment_key_id, hostname, os, client_version, first_seen, last_seen, online)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
         ON CONFLICT(id) DO UPDATE SET
             enrollment_key_id = excluded.enrollment_key_id,
             hostname = excluded.hostname,
             os = excluded.os,
             client_version = excluded.client_version,
             last_seen = excluded.last_seen,
             online = 1",
        params![device.id, device.enrollment_key_id, device.hostname, device.os, device.client_version, now],
    )?;
    Ok(())
}

pub fn set_device_offline(conn: &Connection, id: &str, last_seen: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE devices SET online = 0, last_seen = ?2 WHERE id = ?1",
        params![id, last_seen],
    )?;
    Ok(())
}

/// Clears online flags left behind by a server that did not stop cleanly.
pub fn mark_all_devices_offline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("UPDATE devices SET online = 0 WHERE online = 1", [])?;
    Ok(())
}

pub fn list_devices(conn: &Connection) -> rusqlite::Result<Vec<Device>> {
    conn.prepare(
        "SELECT id, enrollment_key_id, hostname, os, client_version, online, first_seen, last_seen
         FROM devices ORDER BY hostname, id",
    )?
    .query_map([], |row| {
        Ok(Device {
            id: row.get(0)?,
            enrollment_key_id: row.get(1)?,
            hostname: row.get(2)?,
            os: row.get(3)?,
            client_version: row.get(4)?,
            online: row.get(5)?,
            first_seen_at: row.get(6)?,
            last_seen_at: row.get(7)?,
        })
    })?
    .collect()
}

// ---------------------------------------------------------------- jobs

pub mod target_state {
    pub const QUEUED: &str = "queued";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub targets: Vec<JobTarget>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobTarget {
    pub id: String,
    pub device_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub message: Option<String>,
    /// Omitted in listings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub output_truncated: bool,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// Which devices a new job goes to.
pub enum Targets {
    All,
    Devices(Vec<String>),
}

pub enum CreateJobError {
    UnknownDevice(String),
    NoTargets,
}

/// Creates a job and one queued target per device, atomically.
pub fn create_job(
    conn: &mut Connection,
    kind: &str,
    payload: &serde_json::Value,
    created_by: Option<&str>,
    targets: &Targets,
) -> rusqlite::Result<Result<Job, CreateJobError>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let device_ids: Vec<String> = match targets {
        Targets::All => tx
            .prepare("SELECT id FROM devices ORDER BY hostname, id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?,
        Targets::Devices(ids) => {
            let mut unique: Vec<String> = Vec::new();
            for id in ids {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1)",
                    [id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Ok(Err(CreateJobError::UnknownDevice(id.clone())));
                }
                if !unique.contains(id) {
                    unique.push(id.clone());
                }
            }
            unique
        }
    };
    if device_ids.is_empty() {
        return Ok(Err(CreateJobError::NoTargets));
    }

    let now = now_ms();
    let job_id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO jobs (id, kind, payload, created_by, created) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![job_id, kind, payload.to_string(), created_by, now],
    )?;
    for device_id in &device_ids {
        tx.execute(
            "INSERT INTO job_targets (id, job_id, device_id, state, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uuid::Uuid::new_v4().to_string(), job_id, device_id, target_state::QUEUED, now],
        )?;
    }
    let job = get_job(&tx, &job_id, true)?.expect("job was just inserted");
    tx.commit()?;
    Ok(Ok(job))
}

fn job_targets(conn: &Connection, job_id: &str, with_output: bool) -> rusqlite::Result<Vec<JobTarget>> {
    conn.prepare(
        "SELECT id, device_id, state, exit_code, reason, message, output, output_truncated,
                created, started, finished
         FROM job_targets WHERE job_id = ?1 ORDER BY rowid",
    )?
    .query_map([job_id], |row| {
        Ok(JobTarget {
            id: row.get(0)?,
            device_id: row.get(1)?,
            state: row.get(2)?,
            exit_code: row.get(3)?,
            reason: row.get(4)?,
            message: row.get(5)?,
            output: if with_output { Some(row.get(6)?) } else { None },
            output_truncated: row.get(7)?,
            created_at: row.get(8)?,
            started_at: row.get(9)?,
            finished_at: row.get(10)?,
        })
    })?
    .collect()
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let payload: String = row.get(2)?;
    Ok(Job {
        id: row.get(0)?,
        kind: row.get(1)?,
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        created_by: row.get(3)?,
        created_at: row.get(4)?,
        targets: Vec::new(),
    })
}

pub fn get_job(conn: &Connection, id: &str, with_output: bool) -> rusqlite::Result<Option<Job>> {
    let job = conn
        .query_row(
            "SELECT id, kind, payload, created_by, created FROM jobs WHERE id = ?1",
            [id],
            job_from_row,
        )
        .optional()?;
    match job {
        Some(mut job) => {
            job.targets = job_targets(conn, &job.id, with_output)?;
            Ok(Some(job))
        }
        None => Ok(None),
    }
}

/// Jobs newest first, targets without output.
pub fn list_jobs(conn: &Connection, limit: u32) -> rusqlite::Result<Vec<Job>> {
    let mut jobs: Vec<Job> = conn
        .prepare(
            "SELECT id, kind, payload, created_by, created FROM jobs
             ORDER BY created DESC, rowid DESC LIMIT ?1",
        )?
        .query_map([limit], job_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    for job in &mut jobs {
        job.targets = job_targets(conn, &job.id, false)?;
    }
    Ok(jobs)
}

/// A job target ready to be sent to its device.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub job_target_id: String,
    pub job_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Returns the targets to send to a device, oldest first, and marks the
/// queued ones as sent. With `include_unresolved`, targets already sent or
/// running are returned too (for re-delivery after a reconnect; the client
/// ignores the ones it already has).
pub fn take_dispatchable(
    conn: &mut Connection,
    device_id: &str,
    include_unresolved: bool,
) -> rusqlite::Result<Vec<Dispatch>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let states = if include_unresolved { "('queued', 'sent', 'running')" } else { "('queued')" };
    let dispatches: Vec<Dispatch> = tx
        .prepare(&format!(
            "SELECT t.id, j.id, j.kind, j.payload
             FROM job_targets t JOIN jobs j ON j.id = t.job_id
             WHERE t.device_id = ?1 AND t.state IN {states}
             ORDER BY t.created, t.rowid"
        ))?
        .query_map([device_id], |row| {
            let payload: String = row.get(3)?;
            Ok(Dispatch {
                job_target_id: row.get(0)?,
                job_id: row.get(1)?,
                kind: row.get(2)?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute(
        "UPDATE job_targets SET state = 'sent' WHERE device_id = ?1 AND state = 'queued'",
        [device_id],
    )?;
    tx.commit()?;
    Ok(dispatches)
}

/// `sent` → `running`. Repeats are harmless.
pub fn mark_target_started(conn: &Connection, target_id: &str, device_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE job_targets SET state = 'running', started = ?3
         WHERE id = ?1 AND device_id = ?2 AND state = 'sent'",
        params![target_id, device_id, now_ms()],
    )?;
    Ok(())
}

/// Appends an output chunk unless it is a repeat or the target is finished.
/// Output beyond `max_bytes` is dropped and replaced by a marker.
pub fn append_target_output(
    conn: &mut Connection,
    target_id: &str,
    device_id: &str,
    seq: u64,
    data: &str,
    max_bytes: usize,
) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<(i64, i64, bool)> = tx
        .query_row(
            "SELECT last_seq, length(CAST(output AS BLOB)), output_truncated FROM job_targets
             WHERE id = ?1 AND device_id = ?2 AND state IN ('sent', 'running')",
            params![target_id, device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((last_seq, stored_bytes, truncated)) = current else {
        return Ok(());
    };
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    if seq <= last_seq {
        return Ok(());
    }
    if truncated {
        tx.execute("UPDATE job_targets SET last_seq = ?2 WHERE id = ?1", params![target_id, seq])?;
    } else {
        let remaining = max_bytes.saturating_sub(stored_bytes as usize);
        if data.len() <= remaining {
            tx.execute(
                "UPDATE job_targets SET output = output || ?2, last_seq = ?3 WHERE id = ?1",
                params![target_id, data, seq],
            )?;
        } else {
            let mut cut = remaining;
            while !data.is_char_boundary(cut) {
                cut -= 1;
            }
            let kept = format!("{}{}", &data[..cut], TRUNCATION_MARKER);
            tx.execute(
                "UPDATE job_targets SET output = output || ?2, last_seq = ?3, output_truncated = 1
                 WHERE id = ?1",
                params![target_id, kept, seq],
            )?;
        }
    }
    tx.commit()
}

pub struct TargetResult<'a> {
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub reason: Option<&'a str>,
    pub message: Option<&'a str>,
}

/// Records the final result of a target that is still unresolved and belongs
/// to the device. Anything else (a repeat, a target of another device, an
/// unknown id) changes nothing.
pub fn finish_target(
    conn: &Connection,
    target_id: &str,
    device_id: &str,
    result: &TargetResult<'_>,
) -> rusqlite::Result<()> {
    let state = if result.succeeded { target_state::SUCCEEDED } else { target_state::FAILED };
    conn.execute(
        "UPDATE job_targets
         SET state = ?3, exit_code = ?4, reason = ?5, message = ?6, finished = ?7
         WHERE id = ?1 AND device_id = ?2 AND state IN ('sent', 'running')",
        params![target_id, device_id, state, result.exit_code, result.reason, result.message, now_ms()],
    )?;
    Ok(())
}

/// Expires targets that have been queued since before `cutoff`.
pub fn expire_queued_targets(conn: &Connection, cutoff: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE job_targets SET state = 'expired', finished = ?2
         WHERE state = 'queued' AND created < ?1",
        params![cutoff, now_ms()],
    )
}

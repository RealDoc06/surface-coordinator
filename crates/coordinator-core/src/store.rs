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
", "
    CREATE TABLE accounts (
        id            TEXT PRIMARY KEY,
        username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
        password_hash TEXT NOT NULL,
        created       INTEGER NOT NULL
    );
    CREATE TABLE sessions (
        token_hash TEXT PRIMARY KEY,
        account_id TEXT REFERENCES accounts(id),
        created    INTEGER NOT NULL,
        expires    INTEGER NOT NULL
    );
    CREATE TABLE rejected_connections (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        at        INTEGER NOT NULL,
        peer      TEXT NOT NULL,
        device_id TEXT,
        hostname  TEXT,
        key_id    TEXT,
        reason    TEXT NOT NULL
    );
", "
    ALTER TABLE devices ADD COLUMN network_ssid TEXT;
    ALTER TABLE devices ADD COLUMN network_ip TEXT;
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

/// Stores a key created elsewhere (exported from another coordinator).
/// Returns `None` if a key with this id already exists.
pub fn import_enrollment_key(
    conn: &Connection,
    id: &str,
    secret: &str,
    label: &str,
) -> rusqlite::Result<Option<EnrollmentKey>> {
    let key = EnrollmentKey {
        id: id.to_owned(),
        label: label.to_owned(),
        secret: secret.to_owned(),
        created: now_ms(),
        revoked: None,
    };
    let inserted = conn.execute(
        "INSERT INTO enrollment_keys (id, label, secret, created) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO NOTHING",
        params![key.id, key.label, key.secret, key.created],
    )?;
    Ok((inserted > 0).then_some(key))
}

/// A key as listed in the UI: no secret.
#[derive(Debug, Clone, Serialize)]
pub struct KeySummary {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub device_count: i64,
}

pub fn list_key_summaries(conn: &Connection) -> rusqlite::Result<Vec<KeySummary>> {
    conn.prepare(
        "SELECT k.id, k.label, k.created, k.revoked,
                (SELECT COUNT(*) FROM devices d WHERE d.enrollment_key_id = k.id)
         FROM enrollment_keys k ORDER BY k.created, k.rowid",
    )?
    .query_map([], |row| {
        Ok(KeySummary {
            id: row.get(0)?,
            label: row.get(1)?,
            created_at: row.get(2)?,
            revoked_at: row.get(3)?,
            device_count: row.get(4)?,
        })
    })?
    .collect()
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
    pub network: DeviceNetwork,
}

/// What the device reported about its network in its last handshake.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceNetwork {
    pub ssid: Option<String>,
    pub ip: Option<String>,
}

pub struct DeviceInfo<'a> {
    pub id: &'a str,
    pub enrollment_key_id: &'a str,
    pub hostname: &'a str,
    pub os: &'a str,
    pub client_version: &'a str,
    /// Replace what was stored before, so a device that stops reporting its
    /// network does not keep showing an old one.
    pub network_ssid: Option<&'a str>,
    pub network_ip: Option<&'a str>,
}

/// Records a device that completed the handshake and marks it online.
pub fn upsert_device_online(conn: &Connection, device: &DeviceInfo<'_>) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO devices (id, enrollment_key_id, hostname, os, client_version, first_seen, last_seen, online,
                              network_ssid, network_ip)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             enrollment_key_id = excluded.enrollment_key_id,
             hostname = excluded.hostname,
             os = excluded.os,
             client_version = excluded.client_version,
             last_seen = excluded.last_seen,
             online = 1,
             network_ssid = excluded.network_ssid,
             network_ip = excluded.network_ip",
        params![
            device.id,
            device.enrollment_key_id,
            device.hostname,
            device.os,
            device.client_version,
            now,
            device.network_ssid,
            device.network_ip,
        ],
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

fn device_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: row.get(0)?,
        enrollment_key_id: row.get(1)?,
        hostname: row.get(2)?,
        os: row.get(3)?,
        client_version: row.get(4)?,
        online: row.get(5)?,
        first_seen_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        network: DeviceNetwork { ssid: row.get(8)?, ip: row.get(9)? },
    })
}

pub fn list_devices(conn: &Connection) -> rusqlite::Result<Vec<Device>> {
    conn.prepare(
        "SELECT id, enrollment_key_id, hostname, os, client_version, online, first_seen, last_seen,
                network_ssid, network_ip
         FROM devices ORDER BY hostname, id",
    )?
    .query_map([], device_from_row)?
    .collect()
}

pub fn get_device(conn: &Connection, id: &str) -> rusqlite::Result<Option<Device>> {
    conn.query_row(
        "SELECT id, enrollment_key_id, hostname, os, client_version, online, first_seen, last_seen,
                network_ssid, network_ip
         FROM devices WHERE id = ?1",
        [id],
        device_from_row,
    )
    .optional()
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
    let job = get_job(&tx, &job_id, false)?.expect("job was just inserted");
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

/// Shown in place of a Wi-Fi password.
pub const REDACTED_PASSWORD: &str = "********";

/// The payload as the API and the event stream show it, with secrets masked.
/// Every [`Job`] is built from a row here, so no view of a job can carry a
/// secret. Dispatch reads the stored payload through [`Dispatch`] instead,
/// because the device needs the real value.
fn redacted_payload(kind: &str, mut payload: serde_json::Value) -> serde_json::Value {
    if kind == protocol::job_kind::WIFI_JOIN {
        if let Some(password) = payload.get_mut("password").filter(|p| !p.is_null()) {
            *password = serde_json::Value::String(REDACTED_PASSWORD.to_owned());
        }
    }
    payload
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let kind: String = row.get(1)?;
    let payload: String = row.get(2)?;
    let payload = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
    Ok(Job {
        id: row.get(0)?,
        payload: redacted_payload(&kind, payload),
        kind,
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
pub fn list_jobs(conn: &Connection, limit: u32, before: Option<i64>) -> rusqlite::Result<Vec<Job>> {
    let mut jobs: Vec<Job> = conn
        .prepare(
            "SELECT id, kind, payload, created_by, created FROM jobs
             WHERE created < ?2
             ORDER BY created DESC, rowid DESC LIMIT ?1",
        )?
        .query_map(params![limit, before.unwrap_or(i64::MAX)], job_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    for job in &mut jobs {
        job.targets = job_targets(conn, &job.id, false)?;
    }
    Ok(jobs)
}

/// A change to one job target, as published to the UI (no output).
#[derive(Debug, Clone, Serialize)]
pub struct TargetChange {
    pub job_id: String,
    pub target: JobTarget,
}

pub fn get_target_change(conn: &Connection, target_id: &str) -> rusqlite::Result<Option<TargetChange>> {
    conn.query_row(
        "SELECT job_id, id, device_id, state, exit_code, reason, message, output_truncated,
                created, started, finished
         FROM job_targets WHERE id = ?1",
        [target_id],
        |row| {
            Ok(TargetChange {
                job_id: row.get(0)?,
                target: JobTarget {
                    id: row.get(1)?,
                    device_id: row.get(2)?,
                    state: row.get(3)?,
                    exit_code: row.get(4)?,
                    reason: row.get(5)?,
                    message: row.get(6)?,
                    output: None,
                    output_truncated: row.get(7)?,
                    created_at: row.get(8)?,
                    started_at: row.get(9)?,
                    finished_at: row.get(10)?,
                },
            })
        },
    )
    .optional()
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
/// queued ones as sent (those are also returned as changes). With
/// `include_unresolved`, targets already sent or running are returned too
/// (for re-delivery after a reconnect; the client ignores the ones it
/// already has).
pub fn take_dispatchable(
    conn: &mut Connection,
    device_id: &str,
    include_unresolved: bool,
) -> rusqlite::Result<(Vec<Dispatch>, Vec<TargetChange>)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let states = if include_unresolved { "('queued', 'sent', 'running')" } else { "('queued')" };
    let rows: Vec<(Dispatch, bool)> = tx
        .prepare(&format!(
            "SELECT t.id, j.id, j.kind, j.payload, t.state = 'queued'
             FROM job_targets t JOIN jobs j ON j.id = t.job_id
             WHERE t.device_id = ?1 AND t.state IN {states}
             ORDER BY t.created, t.rowid"
        ))?
        .query_map([device_id], |row| {
            let payload: String = row.get(3)?;
            Ok((
                Dispatch {
                    job_target_id: row.get(0)?,
                    job_id: row.get(1)?,
                    kind: row.get(2)?,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                },
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    tx.execute(
        "UPDATE job_targets SET state = 'sent' WHERE device_id = ?1 AND state = 'queued'",
        [device_id],
    )?;
    let mut changes = Vec::new();
    for (dispatch, was_queued) in &rows {
        if *was_queued {
            changes.extend(get_target_change(&tx, &dispatch.job_target_id)?);
        }
    }
    tx.commit()?;
    Ok((rows.into_iter().map(|(dispatch, _)| dispatch).collect(), changes))
}

/// `sent` → `running`. Repeats are harmless and return `None`.
pub fn mark_target_started(
    conn: &Connection,
    target_id: &str,
    device_id: &str,
) -> rusqlite::Result<Option<TargetChange>> {
    let changed = conn.execute(
        "UPDATE job_targets SET state = 'running', started = ?3
         WHERE id = ?1 AND device_id = ?2 AND state = 'sent'",
        params![target_id, device_id, now_ms()],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_target_change(conn, target_id)
}

/// What an output chunk added to the stored output.
#[derive(Debug, Clone)]
pub struct OutputAppended {
    pub job_id: String,
    /// Exactly the text appended, including the marker if it was cut.
    pub data: String,
    pub truncated: bool,
    /// Length in code points of the stored output before this append.
    pub offset: u64,
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
) -> rusqlite::Result<Option<OutputAppended>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // `length` of TEXT counts characters, of a BLOB bytes.
    let current: Option<(i64, i64, i64, bool, String)> = tx
        .query_row(
            "SELECT last_seq, length(CAST(output AS BLOB)), length(output), output_truncated, job_id
             FROM job_targets
             WHERE id = ?1 AND device_id = ?2 AND state IN ('sent', 'running')",
            params![target_id, device_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()?;
    let Some((last_seq, stored_bytes, stored_chars, truncated, job_id)) = current else {
        return Ok(None);
    };
    let offset = u64::try_from(stored_chars).unwrap_or(0);
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    if seq <= last_seq {
        return Ok(None);
    }
    let appended = if truncated {
        tx.execute("UPDATE job_targets SET last_seq = ?2 WHERE id = ?1", params![target_id, seq])?;
        None
    } else {
        let remaining = max_bytes.saturating_sub(stored_bytes as usize);
        if data.len() <= remaining {
            tx.execute(
                "UPDATE job_targets SET output = output || ?2, last_seq = ?3 WHERE id = ?1",
                params![target_id, data, seq],
            )?;
            Some(OutputAppended { job_id, data: data.to_owned(), truncated: false, offset })
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
            Some(OutputAppended { job_id, data: kept, truncated: true, offset })
        }
    };
    tx.commit()?;
    Ok(appended)
}

pub struct TargetResult<'a> {
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub reason: Option<&'a str>,
    pub message: Option<&'a str>,
}

/// Records the final result of a target that is still unresolved and belongs
/// to the device. Anything else (a repeat, a target of another device, an
/// unknown id) changes nothing and returns `None`.
pub fn finish_target(
    conn: &Connection,
    target_id: &str,
    device_id: &str,
    result: &TargetResult<'_>,
) -> rusqlite::Result<Option<TargetChange>> {
    let state = if result.succeeded { target_state::SUCCEEDED } else { target_state::FAILED };
    let changed = conn.execute(
        "UPDATE job_targets
         SET state = ?3, exit_code = ?4, reason = ?5, message = ?6, finished = ?7
         WHERE id = ?1 AND device_id = ?2 AND state IN ('sent', 'running')",
        params![target_id, device_id, state, result.exit_code, result.reason, result.message, now_ms()],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_target_change(conn, target_id)
}

/// Expires targets that have been queued since before `cutoff`.
pub fn expire_queued_targets(conn: &mut Connection, cutoff: i64) -> rusqlite::Result<Vec<TargetChange>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids: Vec<String> = tx
        .prepare("SELECT id FROM job_targets WHERE state = 'queued' AND created < ?1")?
        .query_map([cutoff], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut changes = Vec::new();
    for id in &ids {
        tx.execute(
            "UPDATE job_targets SET state = 'expired', finished = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        changes.extend(get_target_change(&tx, id)?);
    }
    tx.commit()?;
    Ok(changes)
}

/// Recorded on a `wifi.join` target presumed to have succeeded.
pub const PRESUMED_JOINED_MESSAGE: &str = "The device left this coordinator's network and did not \
return. It is presumed to have joined the new network and may be reporting to another coordinator.";

/// Marks `wifi.join` targets that are still running as succeeded when their
/// device has been gone since before `cutoff`. A device that joined the new
/// network reports to whichever coordinator it reached, so this one never
/// hears the result.
///
/// `is_online` must come from the live connection table; the stored flag can
/// lag behind it. The caller holds the presence lock so that the stored
/// `last_seen` and the table agree.
pub fn presume_wifi_joins_succeeded(
    conn: &mut Connection,
    cutoff: i64,
    is_online: impl Fn(&str) -> bool,
) -> rusqlite::Result<Vec<TargetChange>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let candidates: Vec<(String, String)> = tx
        .prepare(
            "SELECT t.id, t.device_id
             FROM job_targets t
             JOIN jobs j ON j.id = t.job_id
             JOIN devices d ON d.id = t.device_id
             WHERE j.kind = ?1 AND t.state = 'running' AND d.last_seen < ?2
             ORDER BY t.rowid",
        )?
        .query_map(params![protocol::job_kind::WIFI_JOIN, cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut changes = Vec::new();
    for (target_id, device_id) in candidates {
        if is_online(&device_id) {
            continue;
        }
        tx.execute(
            "UPDATE job_targets
             SET state = 'succeeded', exit_code = NULL, reason = NULL, message = ?2, finished = ?3
             WHERE id = ?1 AND state = 'running'",
            params![target_id, PRESUMED_JOINED_MESSAGE, now_ms()],
        )?;
        changes.extend(get_target_change(&tx, &target_id)?);
    }
    tx.commit()?;
    Ok(changes)
}

/// A device's most recent targets with the job each belongs to.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceTarget {
    #[serde(flatten)]
    pub target: JobTarget,
    pub job: JobSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobSummary {
    pub id: String,
    pub kind: String,
    pub created_at: i64,
}

pub fn list_device_targets(conn: &Connection, device_id: &str, limit: u32) -> rusqlite::Result<Vec<DeviceTarget>> {
    conn.prepare(
        "SELECT t.id, t.device_id, t.state, t.exit_code, t.reason, t.message, t.output_truncated,
                t.created, t.started, t.finished, j.id, j.kind, j.created
         FROM job_targets t JOIN jobs j ON j.id = t.job_id
         WHERE t.device_id = ?1 ORDER BY t.created DESC, t.rowid DESC LIMIT ?2",
    )?
    .query_map(params![device_id, limit], |row| {
        Ok(DeviceTarget {
            target: JobTarget {
                id: row.get(0)?,
                device_id: row.get(1)?,
                state: row.get(2)?,
                exit_code: row.get(3)?,
                reason: row.get(4)?,
                message: row.get(5)?,
                output: None,
                output_truncated: row.get(6)?,
                created_at: row.get(7)?,
                started_at: row.get(8)?,
                finished_at: row.get(9)?,
            },
            job: JobSummary { id: row.get(10)?, kind: row.get(11)?, created_at: row.get(12)? },
        })
    })?
    .collect()
}

/// Forgets a device and its job history. Jobs left without any target are
/// removed too. Returns false if there was no such device.
pub fn delete_device(conn: &mut Connection, device_id: &str) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM job_targets WHERE device_id = ?1", [device_id])?;
    tx.execute(
        "DELETE FROM jobs WHERE NOT EXISTS (SELECT 1 FROM job_targets t WHERE t.job_id = jobs.id)",
        [],
    )?;
    let deleted = tx.execute("DELETE FROM devices WHERE id = ?1", [device_id])?;
    tx.commit()?;
    Ok(deleted > 0)
}

// ---------------------------------------------------------------- accounts

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: String,
    pub username: String,
    pub created_at: i64,
}

fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account { id: row.get(0)?, username: row.get(1)?, created_at: row.get(2)? })
}

/// Returns `None` if the username is taken.
pub fn create_account(
    conn: &Connection,
    username: &str,
    password_hash: &str,
) -> rusqlite::Result<Option<Account>> {
    let account = Account {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_owned(),
        created_at: now_ms(),
    };
    let inserted = conn.execute(
        "INSERT INTO accounts (id, username, password_hash, created) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(username) DO NOTHING",
        params![account.id, account.username, password_hash, account.created_at],
    )?;
    Ok((inserted > 0).then_some(account))
}

pub fn list_accounts(conn: &Connection) -> rusqlite::Result<Vec<Account>> {
    conn.prepare("SELECT id, username, created FROM accounts ORDER BY username")?
        .query_map([], account_from_row)?
        .collect()
}

/// The account and its password hash, by username (case-insensitive).
pub fn get_account_for_login(conn: &Connection, username: &str) -> rusqlite::Result<Option<(Account, String)>> {
    conn.query_row(
        "SELECT id, username, created, password_hash FROM accounts WHERE username = ?1",
        [username],
        |row| Ok((account_from_row(row)?, row.get(3)?)),
    )
    .optional()
}

/// Sets a new password hash and ends the account's sessions.
pub fn set_account_password(conn: &Connection, id: &str, password_hash: &str) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE accounts SET password_hash = ?2 WHERE id = ?1",
        params![id, password_hash],
    )?;
    if changed > 0 {
        conn.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
    }
    Ok(changed > 0)
}

pub fn delete_account(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
    Ok(conn.execute("DELETE FROM accounts WHERE id = ?1", [id])? > 0)
}

pub fn find_account_by_username(conn: &Connection, username: &str) -> rusqlite::Result<Option<Account>> {
    Ok(get_account_for_login(conn, username)?.map(|(account, _)| account))
}

// ---------------------------------------------------------------- sessions

/// `account_id` is `None` for a session opened with the embedded-mode token.
pub fn create_session(
    conn: &Connection,
    token_hash: &str,
    account_id: Option<&str>,
    expires: i64,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sessions WHERE expires < ?1", [now_ms()])?;
    conn.execute(
        "INSERT INTO sessions (token_hash, account_id, created, expires) VALUES (?1, ?2, ?3, ?4)",
        params![token_hash, account_id, now_ms(), expires],
    )?;
    Ok(())
}

/// A live session: `Some(None)` is a token-mode session without an account.
pub fn get_session(conn: &Connection, token_hash: &str) -> rusqlite::Result<Option<Option<Account>>> {
    conn.query_row(
        "SELECT a.id, a.username, a.created
         FROM sessions s LEFT JOIN accounts a ON a.id = s.account_id
         WHERE s.token_hash = ?1 AND s.expires > ?2",
        params![token_hash, now_ms()],
        |row| {
            let id: Option<String> = row.get(0)?;
            Ok(match id {
                Some(id) => Some(Account { id, username: row.get(1)?, created_at: row.get(2)? }),
                None => None,
            })
        },
    )
    .optional()
}

pub fn delete_session(conn: &Connection, token_hash: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
    Ok(())
}

/// Ends sessions that are not tied to an account (token-mode sessions).
pub fn delete_token_sessions(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sessions WHERE account_id IS NULL", [])?;
    Ok(())
}

// ---------------------------------------------------------------- rejected connections

const MAX_REJECTED_ROWS: i64 = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Rejected {
    pub id: i64,
    pub at: i64,
    pub peer: String,
    pub device_id: Option<String>,
    pub hostname: Option<String>,
    pub key_id: Option<String>,
    pub reason: String,
}

pub fn record_rejected(
    conn: &Connection,
    peer: &str,
    device_id: Option<&str>,
    hostname: Option<&str>,
    key_id: Option<&str>,
    reason: &str,
) -> rusqlite::Result<Rejected> {
    let at = now_ms();
    conn.execute(
        "INSERT INTO rejected_connections (at, peer, device_id, hostname, key_id, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![at, peer, device_id, hostname, key_id, reason],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute("DELETE FROM rejected_connections WHERE id <= ?1", [id - MAX_REJECTED_ROWS])?;
    Ok(Rejected {
        id,
        at,
        peer: peer.to_owned(),
        device_id: device_id.map(str::to_owned),
        hostname: hostname.map(str::to_owned),
        key_id: key_id.map(str::to_owned),
        reason: reason.to_owned(),
    })
}

pub fn list_rejected(conn: &Connection) -> rusqlite::Result<Vec<Rejected>> {
    conn.prepare(
        "SELECT id, at, peer, device_id, hostname, key_id, reason FROM rejected_connections
         ORDER BY id DESC LIMIT ?1",
    )?
    .query_map([MAX_REJECTED_ROWS], |row| {
        Ok(Rejected {
            id: row.get(0)?,
            at: row.get(1)?,
            peer: row.get(2)?,
            device_id: row.get(3)?,
            hostname: row.get(4)?,
            key_id: row.get(5)?,
            reason: row.get(6)?,
        })
    })?
    .collect()
}

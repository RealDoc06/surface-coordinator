//! SQLite storage.
//!
//! Async requests share bounded batches on one blocking worker. Each request
//! is isolated by a savepoint; replies follow the batch's durable WAL commit.
//! The query functions themselves are synchronous and take the connection.

use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::update::Version;
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
", "
    CREATE TABLE groups (
        id      TEXT PRIMARY KEY,
        name    TEXT NOT NULL UNIQUE COLLATE NOCASE,
        created INTEGER NOT NULL
    );
    CREATE TABLE group_members (
        group_id  TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
        device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        PRIMARY KEY (group_id, device_id)
    );
    CREATE TABLE scripts (
        id       TEXT PRIMARY KEY,
        name     TEXT NOT NULL UNIQUE COLLATE NOCASE,
        shell    TEXT NOT NULL,
        body     TEXT NOT NULL,
        built_in INTEGER NOT NULL DEFAULT 0,
        created  INTEGER NOT NULL,
        updated  INTEGER NOT NULL
    );
", "
    ALTER TABLE devices ADD COLUMN protocol_version INTEGER NOT NULL DEFAULT 1;
    CREATE TABLE client_releases (
        version TEXT NOT NULL,
        target  TEXT NOT NULL,
        size    INTEGER NOT NULL,
        sha256  TEXT NOT NULL,
        notes   TEXT,
        cached  INTEGER NOT NULL,
        PRIMARY KEY (version, target)
    );
", "
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
", "
    ALTER TABLE job_targets ADD COLUMN cancel_requested INTEGER;
    CREATE INDEX jobs_by_created ON jobs(created);
    CREATE INDEX job_targets_by_device_created ON job_targets(device_id, created);
    CREATE INDEX job_targets_queued_expiry ON job_targets(created) WHERE state = 'queued';
", "
    UPDATE job_targets SET cancel_requested = finished
        WHERE state = 'failed' AND reason = 'cancelled' AND cancel_requested IS NULL;
    CREATE INDEX job_targets_cancellations ON job_targets(device_id, cancel_requested)
        WHERE cancel_requested IS NOT NULL;
", "
    CREATE INDEX job_targets_replay ON job_targets(device_id)
        WHERE state IN ('sent', 'running') AND cancel_requested IS NULL;
", "
    ALTER TABLE job_targets ADD COLUMN output_bytes INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE job_targets ADD COLUMN output_chars INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE job_target_output_chunks (
        target_id TEXT NOT NULL REFERENCES job_targets(id) ON DELETE CASCADE,
        seq       INTEGER NOT NULL,
        data      TEXT NOT NULL,
        PRIMARY KEY (target_id, seq)
    ) WITHOUT ROWID;
    CREATE TRIGGER compact_finished_target_output AFTER UPDATE OF state ON job_targets
    WHEN NEW.state NOT IN ('sent', 'running')
      AND EXISTS (SELECT 1 FROM job_target_output_chunks WHERE target_id = NEW.id)
    BEGIN
        UPDATE job_targets SET output = output || COALESCE((
            SELECT group_concat(data, '') FROM (
                SELECT data FROM job_target_output_chunks WHERE target_id = NEW.id ORDER BY seq
            )
        ), '') WHERE id = NEW.id;
        DELETE FROM job_target_output_chunks WHERE target_id = NEW.id;
    END;
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
    work: Arc<Mutex<WorkQueue>>,
    /// Admitted work, including the current batch. Further callers wait asynchronously.
    capacity: Arc<tokio::sync::Semaphore>,
}

const MAX_PENDING_REQUESTS: usize = 128;
const MAX_COMMIT_BATCH: usize = 64;
/// Bound newly delivered work on clients that execute their queue serially.
const MAX_IN_FLIGHT_TARGETS: i64 = 16;
type Completion = Box<dyn FnOnce(&Result<(), String>) + Send>;
type Work = Box<dyn FnOnce(Option<&mut Connection>) -> (Completion, bool) + Send>;

#[derive(Default)]
struct WorkQueue {
    pending: VecDeque<Work>,
    running: bool,
}

/// Existing multi-statement operations remain atomic both inside an async
/// batch and when called directly by the administration CLI.
enum WriteTransaction<'a> {
    Transaction(rusqlite::Transaction<'a>),
    Savepoint(rusqlite::Savepoint<'a>),
}

impl<'a> WriteTransaction<'a> {
    fn new(conn: &'a mut Connection) -> rusqlite::Result<Self> {
        if conn.is_autocommit() {
            conn.transaction_with_behavior(TransactionBehavior::Immediate).map(Self::Transaction)
        } else {
            conn.savepoint().map(Self::Savepoint)
        }
    }

    fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Transaction(transaction) => transaction.commit(),
            Self::Savepoint(savepoint) => savepoint.commit(),
        }
    }
}

impl Deref for WriteTransaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Transaction(transaction) => transaction,
            Self::Savepoint(savepoint) => savepoint,
        }
    }
}

fn isolated_request<T>(
    conn: &mut Connection,
    work: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
) -> (Result<T, Error>, bool) {
    if let Err(error) = conn.execute_batch("SAVEPOINT sc_request") {
        return (Err(error.into()), true);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(conn)))
        .map_err(|_| Error::Internal("database operation panicked".into()))
        .and_then(|result| result.map_err(Error::from));
    let closed = if result.is_ok() {
        conn.execute_batch("RELEASE sc_request")
    } else {
        conn.execute_batch("ROLLBACK TO sc_request; RELEASE sc_request")
    };
    match closed {
        Ok(()) => (result, false),
        Err(error) => (Err(error.into()), true),
    }
}

/// Drains ready work without delaying the first request to fill a batch. A
/// single FULL-synchronous commit amortizes disk flushes across busy devices.
fn drain_work(conn: Arc<Mutex<Connection>>, work: Arc<Mutex<WorkQueue>>) {
    loop {
        let batch: Vec<_> = {
            let mut queue = work.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if queue.pending.is_empty() {
                // Changing this while holding the queue lock prevents a
                // caller's wakeup from being lost as this worker exits.
                queue.running = false;
                return;
            }
            let count = queue.pending.len().min(MAX_COMMIT_BATCH);
            queue.pending.drain(..count).collect()
        };
        let mut conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut failure = conn.execute_batch("BEGIN IMMEDIATE").err().map(|error| error.to_string());
        let mut completions = Vec::with_capacity(batch.len());
        for request in batch {
            let (complete, aborted) = request(if failure.is_none() { Some(&mut conn) } else { None });
            completions.push(complete);
            // Disk-full/I/O errors can roll back SQLite's outer transaction.
            // Never let a later savepoint become its own independent commit.
            if failure.is_none() && (aborted || conn.is_autocommit()) {
                failure = Some("database batch transaction was interrupted".into());
            }
        }
        let committed = match failure {
            Some(error) => Err(error),
            None => conn.execute_batch("COMMIT").map_err(|error| error.to_string()),
        };
        if committed.is_err() && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        drop(conn);
        for complete in completions {
            complete(&committed);
        }
    }
}

impl Store {
    /// Opens (creating if needed) the database in `data_dir` and migrates it.
    pub fn open(data_dir: &Path) -> Result<Store, Error> {
        std::fs::create_dir_all(data_dir)?;
        let path: PathBuf = data_dir.join(DB_FILE);
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Grouping commits must not weaken acknowledgement durability.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            work: Arc::new(Mutex::new(WorkQueue::default())),
            capacity: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_REQUESTS)),
        })
    }

    /// Runs `f` in a savepoint on the blocking pool. A successful reply means
    /// the encompassing FULL-synchronous transaction has committed.
    pub async fn call<T, F>(&self, f: F) -> Result<T, Error>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self.capacity.clone().acquire_owned().await
            .map_err(|error| Error::Internal(error.to_string()))?;
        let (reply, received) = tokio::sync::oneshot::channel();
        let request: Work = Box::new(move |conn| {
            let (result, aborted) = match conn {
                Some(conn) => isolated_request(conn, f),
                None => (Err(Error::Internal("database batch could not start".into())), false),
            };
            let complete: Completion = Box::new(move |committed| {
                let _permit = permit;
                let result = match committed {
                    Ok(()) => result,
                    Err(error) => Err(Error::Internal(format!("database commit failed: {error}"))),
                };
                let _ = reply.send(result);
            });
            (complete, aborted)
        });
        let start_worker = {
            let mut work = self.work.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            work.pending.push_back(request);
            let start = !work.running;
            work.running = true;
            start
        };
        if start_worker {
            let conn = self.conn.clone();
            let work = self.work.clone();
            tokio::task::spawn_blocking(move || drain_work(conn, work));
        }
        received.await.map_err(|error| Error::Internal(error.to_string()))?
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
        if index == 9 {
            migrate_output_counts(&tx)?;
            // Resume old running jobs on the same append path. Completed
            // rows retain their original representation for offline readers.
            tx.execute_batch("INSERT INTO job_target_output_chunks (target_id, seq, data)
                SELECT id, 0, output FROM job_targets
                WHERE state IN ('sent', 'running') AND length(CAST(output AS BLOB)) > 0;
                UPDATE job_targets SET output = '' WHERE state IN ('sent', 'running');")?;
        }
        tx.pragma_update(None, "user_version", index + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Count existing output in bounded pages. SQLite length(TEXT) stops at NUL,
/// whereas event offsets and the browser count every Unicode code point.
fn migrate_output_counts(conn: &Connection) -> rusqlite::Result<()> {
    let mut after = i64::MIN;
    loop {
        let rows = conn.prepare(
            "SELECT rowid, id, output FROM job_targets WHERE rowid > ?1 ORDER BY rowid LIMIT 256",
        )?.query_map([after], |row| {
            let output: String = row.get(2)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, output.len(), output.chars().count()))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        for (rowid, id, bytes, chars) in rows {
            conn.execute("UPDATE job_targets SET output_bytes = ?2, output_chars = ?3 WHERE id = ?1",
                params![id, bytes as i64, chars as i64])?;
            after = rowid;
        }
    }
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
    /// The protocol version of its last handshake.
    pub protocol_version: u32,
    pub online: bool,
    /// Connected with a protocol version this coordinator does not support,
    /// so it is only sent `client.update`. Set from the live connection.
    pub restricted: bool,
    /// Its client version is older than the newest cached release. Set from
    /// the release cache.
    pub update_available: bool,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub network: DeviceNetwork,
    /// Ids of the groups the device is a member of, sorted.
    pub group_ids: Vec<String>,
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
    pub protocol_version: u32,
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
                              network_ssid, network_ip, protocol_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
             enrollment_key_id = excluded.enrollment_key_id,
             hostname = excluded.hostname,
             os = excluded.os,
             client_version = excluded.client_version,
             last_seen = excluded.last_seen,
             online = 1,
             network_ssid = excluded.network_ssid,
             network_ip = excluded.network_ip,
             protocol_version = excluded.protocol_version",
        params![
            device.id,
            device.enrollment_key_id,
            device.hostname,
            device.os,
            device.client_version,
            now,
            device.network_ssid,
            device.network_ip,
            device.protocol_version,
        ],
    )?;
    Ok(())
}

pub fn set_device_offline(conn: &mut Connection, id: &str, last_seen: i64) -> rusqlite::Result<Vec<TargetChange>> {
    let tx = WriteTransaction::new(conn)?;
    tx.execute(
        "UPDATE devices SET online = 0, last_seen = ?2 WHERE id = ?1",
        params![id, last_seen],
    )?;
    let changes = settle_pending_cancels(&tx, Some(id))?;
    tx.commit()?;
    Ok(changes)
}

/// Clears online flags left behind by a server that did not stop cleanly.
pub fn mark_all_devices_offline(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = WriteTransaction::new(conn)?;
    tx.execute("UPDATE devices SET online = 0 WHERE online = 1", [])?;
    settle_pending_cancels(&tx, None)?;
    tx.commit()?;
    Ok(())
}

fn device_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: row.get(0)?,
        enrollment_key_id: row.get(1)?,
        hostname: row.get(2)?,
        os: row.get(3)?,
        client_version: row.get(4)?,
        protocol_version: row.get(10)?,
        online: row.get(5)?,
        // Live state, filled in by `AppState::apply_live_state`.
        restricted: false,
        update_available: false,
        first_seen_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        network: DeviceNetwork { ssid: row.get(8)?, ip: row.get(9)? },
        // Filled in by the callers.
        group_ids: Vec::new(),
    })
}

pub fn list_devices(conn: &Connection) -> rusqlite::Result<Vec<Device>> {
    let mut devices: Vec<Device> = conn
        .prepare(
            "SELECT id, enrollment_key_id, hostname, os, client_version, online, first_seen, last_seen,
                    network_ssid, network_ip, protocol_version
             FROM devices ORDER BY hostname, id",
        )?
        .query_map([], device_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut memberships: HashMap<String, Vec<String>> = HashMap::new();
    let mut statement = conn.prepare("SELECT device_id, group_id FROM group_members ORDER BY group_id")?;
    for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
        let (device_id, group_id) = row?;
        memberships.entry(device_id).or_default().push(group_id);
    }
    for device in &mut devices {
        device.group_ids = memberships.remove(&device.id).unwrap_or_default();
    }
    Ok(devices)
}

pub fn get_device(conn: &Connection, id: &str) -> rusqlite::Result<Option<Device>> {
    let device = conn
        .query_row(
            "SELECT id, enrollment_key_id, hostname, os, client_version, online, first_seen, last_seen,
                    network_ssid, network_ip, protocol_version
             FROM devices WHERE id = ?1",
            [id],
            device_from_row,
        )
        .optional()?;
    let Some(mut device) = device else { return Ok(None) };
    device.group_ids = conn
        .prepare("SELECT group_id FROM group_members WHERE device_id = ?1 ORDER BY group_id")?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(device))
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
    /// The members of a group when the job is created.
    Group(String),
}

pub enum CreateJobError {
    UnknownDevice(String),
    UnknownGroup(String),
    NoTargets,
}

/// Starts the message of a `client.update` target that succeeded without
/// installing anything, whether the coordinator or the client decided so.
pub const ALREADY_ON_VERSION: &str = "already on version";

/// Creates a job and one queued target per device, atomically.
///
/// With `done_if_on_version` (a `client.update` job), the target of a device
/// that already reports that client version is created as succeeded instead,
/// so it is never sent.
pub fn create_job(
    conn: &mut Connection,
    kind: &str,
    payload: &serde_json::Value,
    created_by: Option<&str>,
    targets: &Targets,
    done_if_on_version: Option<&Version>,
) -> rusqlite::Result<Result<Job, CreateJobError>> {
    let tx = WriteTransaction::new(conn)?;
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
        Targets::Group(group_id) => {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM groups WHERE id = ?1)",
                [group_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(Err(CreateJobError::UnknownGroup(group_id.clone())));
            }
            tx.prepare(
                "SELECT d.id FROM group_members m JOIN devices d ON d.id = m.device_id
                 WHERE m.group_id = ?1 ORDER BY d.hostname, d.id",
            )?
            .query_map([group_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
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
        let target_id = uuid::Uuid::new_v4().to_string();
        let already_on = match done_if_on_version {
            Some(version) => {
                let reported: String =
                    tx.query_row("SELECT client_version FROM devices WHERE id = ?1", [device_id], |row| row.get(0))?;
                reported.parse::<Version>().is_ok_and(|reported| reported == *version)
            }
            None => false,
        };
        if already_on {
            let version = done_if_on_version.expect("checked above");
            tx.execute(
                "INSERT INTO job_targets (id, job_id, device_id, state, message, created, finished)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![target_id, job_id, device_id, target_state::SUCCEEDED, format!("{ALREADY_ON_VERSION} {version}"), now],
            )?;
        } else {
            tx.execute(
                "INSERT INTO job_targets (id, job_id, device_id, state, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![target_id, job_id, device_id, target_state::QUEUED, now],
            )?;
        }
    }
    let job = get_job(&tx, &job_id, false)?.expect("job was just inserted");
    tx.commit()?;
    Ok(Ok(job))
}

// Completed output remains in the original column. Unresolved jobs append
// small durable rows instead of rewriting an ever-growing output string.
const TARGET_OUTPUT_SQL: &str = "t.output || COALESCE((
    SELECT group_concat(data, '') FROM (
        SELECT data FROM job_target_output_chunks WHERE target_id = t.id ORDER BY seq
    )
), '')";

#[derive(Debug, Clone, Serialize)]
pub struct TargetOutput {
    pub output: String,
    pub output_truncated: bool,
}

/// Reads only the selected target, including its unmaterialized live output.
pub fn get_target_output(conn: &Connection, job_id: &str, target_id: &str) -> rusqlite::Result<Option<TargetOutput>> {
    conn.query_row(
        &format!("SELECT {TARGET_OUTPUT_SQL}, t.output_truncated FROM job_targets t WHERE t.job_id = ?1 AND t.id = ?2"),
        params![job_id, target_id],
        |row| Ok(TargetOutput { output: row.get(0)?, output_truncated: row.get(1)? }),
    ).optional()
}

fn job_targets(conn: &Connection, job_id: &str, with_output: bool) -> rusqlite::Result<Vec<JobTarget>> {
    let output = if with_output { TARGET_OUTPUT_SQL } else { "NULL" };
    conn.prepare(
        &format!("SELECT t.id, device_id, state, exit_code, reason, message, {output}, output_truncated,
                created, started, finished
         FROM job_targets t WHERE job_id = ?1 ORDER BY t.rowid"),
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
    pub rowid: i64,
    pub job_target_id: String,
    pub job_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Loads one target in insertion order, marking queued work sent. Previously
/// sent work after this connection's cursor is replayed once on reconnect.
/// New queued work is eligible even if SQLite reused a deleted rowid.
pub fn take_next_dispatchable(
    conn: &mut Connection,
    device_id: &str,
    after_rowid: i64,
    only_kind: Option<&str>,
) -> rusqlite::Result<Option<(Dispatch, Option<TargetChange>)>> {
    let tx = WriteTransaction::new(conn)?;
    // Each ordered candidate uses an index range and loads at most one row.
    // Combining queued work and replay in one OR query would repeatedly sort
    // the entire unresolved backlog for every message sent.
    let mut queued: Option<i64> = tx.query_row(
        "SELECT t.rowid FROM job_targets t
         WHERE device_id = ?1 AND state = 'queued' AND cancel_requested IS NULL
           AND (?2 IS NULL OR EXISTS (SELECT 1 FROM jobs j WHERE j.id = t.job_id AND j.kind = ?2))
         ORDER BY t.rowid LIMIT 1",
        params![device_id, only_kind], |row| row.get(0),
    ).optional()?;
    if queued.is_some() {
        let in_flight: i64 = tx.query_row(
            "SELECT count(*) FROM job_targets t
             WHERE device_id = ?1 AND state IN ('sent', 'running')
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM jobs j WHERE j.id = t.job_id AND j.kind = ?2))",
            params![device_id, only_kind], |row| row.get(0),
        )?;
        if in_flight >= MAX_IN_FLIGHT_TARGETS {
            queued = None;
        }
    }
    let replay: Option<i64> = tx.query_row(
        "SELECT t.rowid FROM job_targets t INDEXED BY job_targets_replay
         WHERE device_id = ?1 AND state IN ('sent', 'running') AND cancel_requested IS NULL
           AND t.rowid > ?2
           AND (?3 IS NULL OR EXISTS (SELECT 1 FROM jobs j WHERE j.id = t.job_id AND j.kind = ?3))
         ORDER BY t.rowid LIMIT 1",
        params![device_id, after_rowid, only_kind], |row| row.get(0),
    ).optional()?;
    let Some(rowid) = [queued, replay].into_iter().flatten().min() else { return Ok(None) };
    let row = tx.query_row(
            "SELECT t.id, j.id, j.kind, j.payload, t.state = 'queued', t.rowid
             FROM job_targets t JOIN jobs j ON j.id = t.job_id
             WHERE t.rowid = ?1",
        [rowid], |row| {
            let payload: String = row.get(3)?;
            Ok((
                Dispatch {
                    rowid: row.get(5)?,
                    job_target_id: row.get(0)?,
                    job_id: row.get(1)?,
                    kind: row.get(2)?,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                },
                row.get::<_, bool>(4)?,
            ))
        }).optional()?;
    let Some((dispatch, was_queued)) = row else { return Ok(None) };
    let change = if was_queued {
        tx.execute(
            "UPDATE job_targets SET state = 'sent' WHERE id = ?1 AND state = 'queued'",
            [&dispatch.job_target_id],
        )?;
        get_target_change(&tx, &dispatch.job_target_id)?
    } else {
        None
    };
    tx.commit()?;
    Ok(Some((dispatch, change)))
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
    let tx = WriteTransaction::new(conn)?;
    let current: Option<(i64, i64, i64, bool, String)> = tx
        .query_row(
            "SELECT last_seq, output_bytes, output_chars, output_truncated, job_id
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
        let (kept, truncated) = if data.len() <= remaining {
            (data.to_owned(), false)
        } else {
            let mut cut = remaining;
            while !data.is_char_boundary(cut) {
                cut -= 1;
            }
            (format!("{}{}", &data[..cut], TRUNCATION_MARKER), true)
        };
        // Empty chunks advance de-duplication without consuming durable rows.
        if !kept.is_empty() {
            tx.execute("INSERT INTO job_target_output_chunks (target_id, seq, data) VALUES (?1, ?2, ?3)",
                params![target_id, seq, kept])?;
        }
        tx.execute(
            "UPDATE job_targets SET last_seq = ?2, output_bytes = output_bytes + ?3,
             output_chars = output_chars + ?4, output_truncated = ?5 WHERE id = ?1",
            params![target_id, seq, kept.len() as i64, kept.chars().count() as i64, truncated],
        )?;
        Some(OutputAppended { job_id, data: kept, truncated, offset })
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

/// Records an unresolved target's result, including an unconfirmed Wi-Fi
/// handoff. Confirmed results, other devices' targets and unknown ids are
/// unchanged and return `None`.
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
         WHERE id = ?1 AND device_id = ?2
           AND (state IN ('sent', 'running') OR (state = 'failed' AND reason = ?8))",
        params![target_id, device_id, state, result.exit_code, result.reason, result.message, now_ms(), HANDOFF_UNCONFIRMED],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_target_change(conn, target_id)
}

/// What cancelling a job did.
pub struct Cancelled {
    /// The job as it stands after the immediate changes, without output.
    pub job: Job,
    /// The targets that were changed here.
    pub changes: Vec<TargetChange>,
}

/// Cancels the unfinished targets of a job, in one transaction. Returns
/// `None` if there is no such job.
///
/// Queued targets fail at once. For a target that was sent to a device,
/// `signal(device_id, job_target_id)` tries to tell the device to stop and
/// returns whether it could; if so the target is left for the device's
/// `job.result` to settle, otherwise (the device is gone) it fails at once.
/// In-flight power targets always await the device's result: its native
/// operation may already be committed and cannot reliably be cancelled.
/// The caller holds the presence lock around this, so a device cannot go
/// offline between `signal` and the commit without that being seen.
pub fn cancel_job(
    conn: &mut Connection,
    job_id: &str,
    mut signal: impl FnMut(&str, &str) -> bool,
) -> rusqlite::Result<Option<Cancelled>> {
    let tx = WriteTransaction::new(conn)?;
    let kind: Option<String> =
        tx.query_row("SELECT kind FROM jobs WHERE id = ?1", [job_id], |row| row.get(0)).optional()?;
    let Some(kind) = kind else {
        return Ok(None);
    };
    let unfinished: Vec<(String, String, String)> = tx
        .prepare(
            "SELECT id, device_id, state FROM job_targets
             WHERE job_id = ?1 AND state IN ('queued', 'sent', 'running') ORDER BY rowid",
        )?
        .query_map([job_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut changes = Vec::new();
    for (target_id, device_id, state) in unfinished {
        // Persist the intent before queueing the signal. A lost socket or a
        // coordinator restart must never cause this target to run again.
        // Keep per-device ordering when cancellations happen in one clock
        // tick or the wall clock moves backwards during a live connection.
        tx.execute(
            "UPDATE job_targets SET cancel_requested = COALESCE(cancel_requested,
                MAX(?2, COALESCE((SELECT MAX(cancel_requested) FROM job_targets WHERE device_id = ?3), 0) + 1))
             WHERE id = ?1",
            params![target_id, now_ms(), device_id],
        )?;
        if state != target_state::QUEUED
            && (signal(&device_id, &target_id) || kind == protocol::job_kind::POWER)
        {
            continue;
        }
        tx.execute(
            "UPDATE job_targets
             SET state = 'failed', exit_code = NULL, reason = ?2, message = NULL, finished = ?3
             WHERE id = ?1",
            params![target_id, protocol::reason::CANCELLED, now_ms()],
        )?;
        changes.extend(get_target_change(&tx, &target_id)?);
    }
    let job = get_job(&tx, job_id, false)?.expect("job exists");
    tx.commit()?;
    Ok(Some(Cancelled { job, changes }))
}

/// Once a cancelled device disconnects, settle its targets as we do for an
/// offline cancellation. The stored cancellation is replayed when it returns.
/// Power operations may be committed already, so only their device can settle them.
fn settle_pending_cancels(conn: &Connection, device_id: Option<&str>) -> rusqlite::Result<Vec<TargetChange>> {
    let ids = conn.prepare(
        "SELECT id FROM job_targets
         WHERE cancel_requested IS NOT NULL AND state IN ('sent', 'running')
           AND (?1 IS NULL OR device_id = ?1)
           AND EXISTS (SELECT 1 FROM jobs WHERE id = job_targets.job_id AND kind != ?2)",
    )?.query_map(params![device_id, protocol::job_kind::POWER], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changes = Vec::new();
    for id in ids {
        conn.execute(
            "UPDATE job_targets SET state = 'failed', exit_code = NULL, reason = ?2,
             message = NULL, finished = ?3 WHERE id = ?1",
            params![id, protocol::reason::CANCELLED, now_ms()],
        )?;
        changes.extend(get_target_change(conn, &id)?);
    }
    Ok(changes)
}

pub struct Cancellation {
    pub target_id: String,
    pub requested_at: i64,
    pub rowid: i64,
}

/// Replays cancellations once per connection, with constant memory even
/// after a large offline backlog. Unacknowledged requests do not expire.
pub fn next_cancellation(
    conn: &Connection,
    device_id: &str,
    since: i64,
    after: (i64, i64),
) -> rusqlite::Result<Option<Cancellation>> {
    conn.query_row(
        "SELECT id, cancel_requested, rowid FROM job_targets
         WHERE device_id = ?1
           AND cancel_requested IS NOT NULL
           AND (cancel_requested > ?4 OR (cancel_requested = ?4 AND rowid > ?5))
           AND ((state = 'failed' AND reason = ?2 AND finished >= ?3)
             OR state IN ('sent', 'running'))
         ORDER BY cancel_requested, rowid LIMIT 1",
        params![device_id, protocol::reason::CANCELLED, since, after.0, after.1],
        |row| Ok(Cancellation { target_id: row.get(0)?, requested_at: row.get(1)?, rowid: row.get(2)? }),
    ).optional()
}

/// Expires targets that have been queued since before `cutoff`.
pub fn expire_queued_targets(conn: &mut Connection, cutoff: i64) -> rusqlite::Result<Vec<TargetChange>> {
    let tx = WriteTransaction::new(conn)?;
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

pub const HANDOFF_UNCONFIRMED: &str = "handoff_unconfirmed";
/// Silence does not establish whether Wi-Fi association succeeded.
pub const UNCONFIRMED_HANDOFF_MESSAGE: &str = "The device disconnected before confirming the new \
network. Check the device or its destination coordinator before retrying.";

/// Marks a running `wifi.join` unconfirmed once its device has been gone
/// since before `cutoff`. An actual result can still resolve this later.
///
/// `is_online` must come from the live connection table; the stored flag can
/// lag behind it. The caller holds the presence lock so that the stored
/// `last_seen` and the table agree.
pub fn mark_wifi_handoffs_unconfirmed(
    conn: &mut Connection,
    cutoff: i64,
    is_online: impl Fn(&str) -> bool,
) -> rusqlite::Result<Vec<TargetChange>> {
    let tx = WriteTransaction::new(conn)?;
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
             SET state = 'failed', exit_code = NULL, reason = ?4, message = ?2, finished = ?3
             WHERE id = ?1 AND state = 'running'",
            params![target_id, UNCONFIRMED_HANDOFF_MESSAGE, now_ms(), HANDOFF_UNCONFIRMED],
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

/// What forgetting a device removed.
pub struct DeviceDeleted {
    /// The device was a member of at least one group.
    pub was_in_group: bool,
}

/// Forgets a device, its job history and its group memberships. Jobs left
/// without any target are removed too. Returns `None` if there was no such
/// device.
pub fn delete_device(conn: &mut Connection, device_id: &str) -> rusqlite::Result<Option<DeviceDeleted>> {
    let tx = WriteTransaction::new(conn)?;
    tx.execute("DELETE FROM job_targets WHERE device_id = ?1", [device_id])?;
    tx.execute(
        "DELETE FROM jobs WHERE NOT EXISTS (SELECT 1 FROM job_targets t WHERE t.job_id = jobs.id)",
        [],
    )?;
    // Explicitly, rather than through the cascade, so this does not depend
    // on foreign keys being enforced.
    let was_in_group = tx.execute("DELETE FROM group_members WHERE device_id = ?1", [device_id])? > 0;
    let deleted = tx.execute("DELETE FROM devices WHERE id = ?1", [device_id])?;
    tx.commit()?;
    Ok((deleted > 0).then_some(DeviceDeleted { was_in_group }))
}

// ---------------------------------------------------------------- client releases

/// A verified client release in the update cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientRelease {
    pub version: String,
    pub target: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
    pub cached_at: i64,
    pub notes: Option<String>,
}

fn release_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClientRelease> {
    Ok(ClientRelease {
        version: row.get(0)?,
        target: row.get(1)?,
        size: row.get(2)?,
        sha256: row.get(3)?,
        cached_at: row.get(4)?,
        notes: row.get(5)?,
    })
}

const RELEASE_COLUMNS: &str = "version, target, size, sha256, cached, notes";

/// Cached releases, newest version first. Rows whose version does not parse
/// (none are written that way) sort last.
pub fn list_client_releases(conn: &Connection) -> rusqlite::Result<Vec<ClientRelease>> {
    let mut releases: Vec<ClientRelease> = conn
        .prepare(&format!("SELECT {RELEASE_COLUMNS} FROM client_releases"))?
        .query_map([], release_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    releases.sort_by(|a, b| {
        let version = |r: &ClientRelease| r.version.parse::<Version>().ok();
        version(b).cmp(&version(a)).then_with(|| a.target.cmp(&b.target))
    });
    Ok(releases)
}

pub fn get_client_release(conn: &Connection, version: &str, target: &str) -> rusqlite::Result<Option<ClientRelease>> {
    conn.query_row(
        &format!("SELECT {RELEASE_COLUMNS} FROM client_releases WHERE version = ?1 AND target = ?2"),
        params![version, target],
        release_from_row,
    )
    .optional()
}

/// Records a release whose files are in place; replaces an earlier record.
pub fn insert_client_release(conn: &Connection, release: &ClientRelease) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO client_releases (version, target, size, sha256, cached, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![release.version, release.target, release.size, release.sha256, release.cached_at, release.notes],
    )?;
    Ok(())
}

pub fn delete_client_release(conn: &Connection, version: &str, target: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM client_releases WHERE version = ?1 AND target = ?2", params![version, target])?;
    Ok(())
}

/// Every known device's id and reported client version, ordered by id.
pub fn device_versions(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    conn.prepare("SELECT id, client_version FROM devices ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// One `client.update` target for a version: a device's attempt to get it.
#[derive(Debug, Clone)]
pub struct UpdateAttempt {
    pub device_id: String,
    pub state: String,
    pub message: Option<String>,
    /// Created by the automatic rollout rather than an operator.
    pub automatic: bool,
}

/// The `client.update` targets for `version`, oldest first. `automatic_creator`
/// is the `created_by` of jobs the rollout created.
pub fn update_attempts(conn: &Connection, version: &str, automatic_creator: &str) -> rusqlite::Result<Vec<UpdateAttempt>> {
    conn.prepare(
        "SELECT t.device_id, t.state, t.message, j.created_by IS ?3
         FROM job_targets t JOIN jobs j ON j.id = t.job_id
         WHERE j.kind = ?1 AND json_extract(j.payload, '$.version') = ?2
         ORDER BY t.created, t.rowid",
    )?
    .query_map(params![protocol::job_kind::CLIENT_UPDATE, version, automatic_creator], |row| {
        Ok(UpdateAttempt { device_id: row.get(0)?, state: row.get(1)?, message: row.get(2)?, automatic: row.get(3)? })
    })?
    .collect()
}

// ---------------------------------------------------------------- settings

/// Keys of the `settings` table.
pub mod setting {
    /// `"true"` or `"false"`: the automatic update rollout is on.
    pub const AUTO_UPDATE: &str = "auto_update";
}

/// The stored value of a setting, storing `default` first if there is none.
pub fn setting_or_init(conn: &Connection, key: &str, default: &str) -> rusqlite::Result<String> {
    conn.execute("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING", params![key, default])?;
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- groups

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    /// Sorted.
    pub device_ids: Vec<String>,
}

/// Why a group could not be changed.
#[derive(Debug, PartialEq, Eq)]
pub enum GroupError {
    NotFound,
    NameTaken,
    UnknownDevice(String),
}

fn get_group(conn: &Connection, id: &str) -> rusqlite::Result<Option<Group>> {
    let group = conn
        .query_row("SELECT id, name, created FROM groups WHERE id = ?1", [id], |row| {
            Ok(Group { id: row.get(0)?, name: row.get(1)?, created_at: row.get(2)?, device_ids: Vec::new() })
        })
        .optional()?;
    let Some(mut group) = group else { return Ok(None) };
    group.device_ids = conn
        .prepare("SELECT device_id FROM group_members WHERE group_id = ?1 ORDER BY device_id")?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(group))
}

/// Groups ordered by name.
pub fn list_groups(conn: &Connection) -> rusqlite::Result<Vec<Group>> {
    let mut groups: Vec<Group> = conn
        .prepare("SELECT id, name, created FROM groups ORDER BY name, id")?
        .query_map([], |row| {
            Ok(Group { id: row.get(0)?, name: row.get(1)?, created_at: row.get(2)?, device_ids: Vec::new() })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    let mut statement = conn.prepare("SELECT group_id, device_id FROM group_members ORDER BY device_id")?;
    for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
        let (group_id, device_id) = row?;
        members.entry(group_id).or_default().push(device_id);
    }
    for group in &mut groups {
        group.device_ids = members.remove(&group.id).unwrap_or_default();
    }
    Ok(groups)
}

/// Creates an empty group. Returns `None` if the name is taken (ignoring case).
pub fn create_group(conn: &Connection, name: &str) -> rusqlite::Result<Option<Group>> {
    let group = Group {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
        created_at: now_ms(),
        device_ids: Vec::new(),
    };
    let inserted = conn.execute(
        "INSERT INTO groups (id, name, created) VALUES (?1, ?2, ?3) ON CONFLICT(name) DO NOTHING",
        params![group.id, group.name, group.created_at],
    )?;
    Ok((inserted > 0).then_some(group))
}

pub fn rename_group(conn: &mut Connection, id: &str, name: &str) -> rusqlite::Result<Result<Group, GroupError>> {
    let tx = WriteTransaction::new(conn)?;
    if get_group(&tx, id)?.is_none() {
        return Ok(Err(GroupError::NotFound));
    }
    // The column compares ignoring case, so this also finds "LAB" for "lab".
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM groups WHERE name = ?1 AND id != ?2)",
        params![name, id],
        |row| row.get(0),
    )?;
    if taken {
        return Ok(Err(GroupError::NameTaken));
    }
    tx.execute("UPDATE groups SET name = ?2 WHERE id = ?1", params![id, name])?;
    let group = get_group(&tx, id)?.expect("group exists");
    tx.commit()?;
    Ok(Ok(group))
}

/// Deletes a group; its members stay. Returns false if there was none.
pub fn delete_group(conn: &mut Connection, id: &str) -> rusqlite::Result<bool> {
    let tx = WriteTransaction::new(conn)?;
    tx.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
    let deleted = tx.execute("DELETE FROM groups WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(deleted > 0)
}

/// Replaces a group's membership. Nothing changes if a device is unknown.
pub fn set_group_members(
    conn: &mut Connection,
    id: &str,
    device_ids: &[String],
) -> rusqlite::Result<Result<Group, GroupError>> {
    let tx = WriteTransaction::new(conn)?;
    if get_group(&tx, id)?.is_none() {
        return Ok(Err(GroupError::NotFound));
    }
    for device_id in device_ids {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1)",
            [device_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Err(GroupError::UnknownDevice(device_id.clone())));
        }
    }
    tx.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
    for device_id in device_ids {
        tx.execute(
            "INSERT INTO group_members (group_id, device_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![id, device_id],
        )?;
    }
    let group = get_group(&tx, id)?.expect("group exists");
    tx.commit()?;
    Ok(Ok(group))
}

// ---------------------------------------------------------------- scripts

#[derive(Debug, Clone, Serialize)]
pub struct Script {
    pub id: String,
    pub name: String,
    /// `powershell` or `cmd`.
    pub shell: String,
    pub body: String,
    pub built_in: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Why a script could not be changed.
#[derive(Debug, PartialEq, Eq)]
pub enum ScriptError {
    NotFound,
    NameTaken,
    BuiltIn,
}

const SCRIPT_COLUMNS: &str = "id, name, shell, body, built_in, created, updated";

fn script_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Script> {
    Ok(Script {
        id: row.get(0)?,
        name: row.get(1)?,
        shell: row.get(2)?,
        body: row.get(3)?,
        built_in: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Scripts ordered by name.
pub fn list_scripts(conn: &Connection) -> rusqlite::Result<Vec<Script>> {
    conn.prepare(&format!("SELECT {SCRIPT_COLUMNS} FROM scripts ORDER BY name, id"))?
        .query_map([], script_from_row)?
        .collect()
}

fn get_script(conn: &Connection, id: &str) -> rusqlite::Result<Option<Script>> {
    conn.query_row(&format!("SELECT {SCRIPT_COLUMNS} FROM scripts WHERE id = ?1"), [id], script_from_row)
        .optional()
}

/// Creates a script. Returns `None` if the name is taken (ignoring case).
pub fn create_script(conn: &Connection, name: &str, shell: &str, body: &str) -> rusqlite::Result<Option<Script>> {
    let now = now_ms();
    let script = Script {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_owned(),
        shell: shell.to_owned(),
        body: body.to_owned(),
        built_in: false,
        created_at: now,
        updated_at: now,
    };
    let inserted = conn.execute(
        "INSERT INTO scripts (id, name, shell, body, built_in, created, updated)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5) ON CONFLICT(name) DO NOTHING",
        params![script.id, script.name, script.shell, script.body, now],
    )?;
    Ok((inserted > 0).then_some(script))
}

pub fn update_script(
    conn: &mut Connection,
    id: &str,
    name: &str,
    shell: &str,
    body: &str,
) -> rusqlite::Result<Result<Script, ScriptError>> {
    let tx = WriteTransaction::new(conn)?;
    match get_script(&tx, id)? {
        None => return Ok(Err(ScriptError::NotFound)),
        Some(script) if script.built_in => return Ok(Err(ScriptError::BuiltIn)),
        Some(_) => {}
    }
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM scripts WHERE name = ?1 AND id != ?2)",
        params![name, id],
        |row| row.get(0),
    )?;
    if taken {
        return Ok(Err(ScriptError::NameTaken));
    }
    tx.execute(
        "UPDATE scripts SET name = ?2, shell = ?3, body = ?4, updated = ?5 WHERE id = ?1",
        params![id, name, shell, body, now_ms()],
    )?;
    let script = get_script(&tx, id)?.expect("script exists");
    tx.commit()?;
    Ok(Ok(script))
}

pub fn delete_script(conn: &mut Connection, id: &str) -> rusqlite::Result<Result<(), ScriptError>> {
    let tx = WriteTransaction::new(conn)?;
    match get_script(&tx, id)? {
        None => return Ok(Err(ScriptError::NotFound)),
        Some(script) if script.built_in => return Ok(Err(ScriptError::BuiltIn)),
        Some(_) => {}
    }
    tx.execute("DELETE FROM scripts WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Makes sure the built-in scripts are stored as this version defines them:
/// inserted if missing, name and body refreshed if they differ (so a newer
/// coordinator updates them). A built-in whose name a user's script already
/// uses is left alone rather than failing the start.
pub fn seed_builtin_scripts(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = WriteTransaction::new(conn)?;
    let now = now_ms();
    for script in &crate::builtin_scripts::BUILT_IN_SCRIPTS {
        let name_in_use: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM scripts WHERE name = ?1 AND id != ?2)",
            params![script.name, script.id],
            |row| row.get(0),
        )?;
        if name_in_use {
            tracing::warn!(id = script.id, name = script.name, "a script already has this built-in's name; skipping it");
            continue;
        }
        // `updated` only moves when something changed, so a restart is not an edit.
        tx.execute(
            "INSERT INTO scripts (id, name, shell, body, built_in, created, updated)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name, shell = excluded.shell, body = excluded.body, updated = excluded.updated
             WHERE scripts.name COLLATE BINARY IS NOT excluded.name
                OR scripts.shell IS NOT excluded.shell
                OR scripts.body IS NOT excluded.body",
            params![script.id, script.name, script.shell, script.body, now],
        )?;
    }
    tx.commit()
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
pub fn set_account_password(conn: &mut Connection, id: &str, password_hash: &str) -> rusqlite::Result<bool> {
    let tx = WriteTransaction::new(conn)?;
    let changed = tx.execute(
        "UPDATE accounts SET password_hash = ?2 WHERE id = ?1",
        params![id, password_hash],
    )?;
    if changed > 0 {
        tx.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

pub fn delete_account(conn: &mut Connection, id: &str) -> rusqlite::Result<bool> {
    let tx = WriteTransaction::new(conn)?;
    tx.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
    let deleted = tx.execute("DELETE FROM accounts WHERE id = ?1", [id])? > 0;
    tx.commit()?;
    Ok(deleted)
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

/// Opens an account session only if the password verified outside the database
/// is still current. A racing password reset or account deletion wins cleanly.
pub fn create_account_session(
    conn: &Connection,
    token_hash: &str,
    account_id: &str,
    verified_password_hash: &str,
    expires: i64,
) -> rusqlite::Result<bool> {
    conn.execute("DELETE FROM sessions WHERE expires < ?1", [now_ms()])?;
    let created = conn.execute(
        "INSERT INTO sessions (token_hash, account_id, created, expires)
         SELECT ?1, id, ?4, ?5 FROM accounts WHERE id = ?2 AND password_hash = ?3",
        params![token_hash, account_id, verified_password_hash, now_ms(), expires],
    )?;
    Ok(created > 0)
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

pub fn delete_session(conn: &Connection, token_hash: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])? > 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn seed_running_output(conn: &Connection, output: &str, last_seq: i64) {
        conn.execute_batch("INSERT INTO enrollment_keys (id, label, secret, created) VALUES ('key', 'test', 'secret', 0);
            INSERT INTO devices (id, enrollment_key_id, hostname, os, client_version, first_seen, last_seen)
                VALUES ('device', 'key', 'test', 'windows', '1.0.0', 0, 0);
            INSERT INTO jobs (id, kind, payload, created) VALUES ('job', 'script.run', '{}', 0);")
            .unwrap();
        conn.execute("INSERT INTO job_targets (id, job_id, device_id, state, output, last_seq, created)
            VALUES ('target', 'job', 'device', 'running', ?1, ?2, 0)", params![output, last_seq]).unwrap();
    }

    #[test]
    fn legacy_live_output_migrates_and_compacts_with_unicode_nul_deduplication_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
        for sql in &MIGRATIONS[..9] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 9).unwrap();
        let prefix = "A\0é😀";
        seed_running_output(&conn, prefix, 7);
        drop(conn);

        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| {
            assert_eq!(conn.query_row("SELECT output_bytes, output_chars FROM job_targets", [],
                |row| Ok((row.get::<_, usize>(0)?, row.get::<_, usize>(1)?)))?, (prefix.len(), 4));
            let chunk = "B\0🚀";
            let cap = prefix.len() + chunk.len() + 1;
            let appended = append_target_output(conn, "target", "device", 8, chunk, cap)?.unwrap();
            assert_eq!((appended.offset, appended.data.as_str()), (4, chunk));
            assert!(append_target_output(conn, "target", "device", 8, "repeat", cap)?.is_none());
            let capped = append_target_output(conn, "target", "device", 9, "é", cap)?.unwrap();
            assert_eq!((capped.offset, capped.truncated), (7, true));
            assert_eq!(capped.data, TRUNCATION_MARKER);
            assert!(append_target_output(conn, "target", "device", 10, "discard", cap)?.is_none());
            let expected = format!("{prefix}{chunk}{TRUNCATION_MARKER}");
            assert_eq!(get_target_output(conn, "job", "target")?.unwrap().output, expected);
            assert_eq!(get_job(conn, "job", true)?.unwrap().targets[0].output.as_deref(), Some(expected.as_str()));
            assert_eq!(conn.query_row("SELECT output FROM job_targets", [], |row| row.get::<_, String>(0))?, "");
            assert_eq!(conn.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, usize>(0))?, 3);
            finish_target(conn, "target", "device", &TargetResult { succeeded: true, exit_code: Some(0), reason: None, message: None })?;
            assert_eq!(conn.query_row("SELECT output FROM job_targets", [], |row| row.get::<_, String>(0))?, expected);
            assert_eq!(conn.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, usize>(0))?, 0);
            assert!(append_target_output(conn, "target", "device", 11, "too late", cap)?.is_none());
            assert!(finish_target(conn, "target", "device", &TargetResult { succeeded: true, exit_code: Some(0), reason: None, message: None })?.is_none());
            assert_eq!(get_target_output(conn, "job", "target")?.unwrap().output, expected);
            Ok(())
        }).unwrap();
    }

    #[tokio::test]
    async fn live_chunks_counters_and_terminal_compaction_roll_back_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| { seed_running_output(conn, "", 0); Ok(()) }).unwrap();
        assert!(store.call(|conn| -> rusqlite::Result<()> {
            append_target_output(conn, "target", "device", 1, "rolled back", 1024)?;
            Err(rusqlite::Error::InvalidQuery)
        }).await.is_err());
        store.call_blocking(|conn| {
            assert_eq!(conn.query_row("SELECT last_seq, output_bytes, output_chars FROM job_targets", [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))?, (0, 0, 0));
            assert_eq!(conn.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, i64>(0))?, 0);
            Ok(())
        }).unwrap();
        store.call(|conn| append_target_output(conn, "target", "device", 1, "kept\0😀", 1024)).await.unwrap();
        store.call_blocking(|conn| conn.execute_batch("CREATE TRIGGER fail_compaction BEFORE DELETE ON job_target_output_chunks
            BEGIN SELECT RAISE(ABORT, 'injected compaction failure'); END;")).unwrap();
        assert!(store.call(|conn| finish_target(conn, "target", "device", &TargetResult {
            succeeded: true, exit_code: Some(0), reason: None, message: None,
        })).await.is_err());
        store.call_blocking(|conn| {
            assert_eq!(conn.query_row("SELECT state, output FROM job_targets", [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?, ("running".into(), "".into()));
            assert_eq!(get_target_output(conn, "job", "target")?.unwrap().output, "kept\0😀");
            conn.execute_batch("DROP TRIGGER fail_compaction")?;
            Ok(())
        }).unwrap();
        store.call(|conn| finish_target(conn, "target", "device", &TargetResult {
            succeeded: true, exit_code: Some(0), reason: None, message: None,
        })).await.unwrap();
        let reader = Connection::open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(reader.query_row("SELECT output FROM job_targets", [], |row| row.get::<_, String>(0)).unwrap(), "kept\0😀");
        assert_eq!(reader.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn cancelling_compacts_output_and_forgetting_a_live_device_removes_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| {
            seed_running_output(conn, "", 0);
            append_target_output(conn, "target", "device", 1, "cancelled output", 1024)?;
            cancel_job(conn, "job", |_, _| false)?.unwrap();
            assert_eq!(get_target_output(conn, "job", "target")?.unwrap().output, "cancelled output");
            assert_eq!(conn.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, i64>(0))?, 0);
            conn.execute("INSERT INTO job_targets (id, job_id, device_id, state, created) VALUES ('live', 'job', 'device', 'running', 0)", [])?;
            append_target_output(conn, "live", "device", 1, "live output", 1024)?;
            delete_device(conn, "device")?.unwrap();
            assert_eq!(conn.query_row("SELECT count(*) FROM job_target_output_chunks", [], |row| row.get::<_, i64>(0))?, 0);
            Ok(())
        }).unwrap();
    }

    /// Holds the current batch while the next requests queue together.
    async fn hold_batch(store: &Store) -> (tokio::task::JoinHandle<Result<(), Error>>, std::sync::mpsc::Sender<()>) {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let store = store.clone();
        let task = tokio::spawn(async move {
            store.call(move |_| {
                let _ = entered.send(());
                wait.recv_timeout(Duration::from_secs(5)).map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(())
            }).await
        });
        ready.await.unwrap();
        (task, release)
    }

    async fn wait_queued(store: &Store, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store.work.lock().unwrap().pending.len() == count { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("requests did not queue");
    }

    #[tokio::test]
    async fn a_failed_request_rolls_back_without_losing_other_work_in_its_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| conn.execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY)")).unwrap();
        let (blocker, release) = hold_batch(&store).await;
        let failed_store = store.clone();
        let failed = tokio::spawn(async move {
            failed_store.call(|conn| -> rusqlite::Result<()> {
                conn.execute("INSERT INTO probe VALUES (1)", [])?;
                Err(rusqlite::Error::InvalidQuery)
            }).await
        });
        wait_queued(&store, 1).await;
        let succeeded_store = store.clone();
        let succeeded = tokio::spawn(async move {
            succeeded_store.call(|conn| conn.execute("INSERT INTO probe VALUES (2)", [])).await
        });
        wait_queued(&store, 2).await;
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        assert!(failed.await.unwrap().is_err());
        assert_eq!(succeeded.await.unwrap().unwrap(), 1);
        let reader = Connection::open(dir.path().join(DB_FILE)).unwrap();
        let ids: Vec<i64> = reader.prepare("SELECT id FROM probe").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(ids, [2]);
    }

    #[tokio::test]
    async fn replies_wait_for_durable_commit_and_commit_failure_rolls_back_every_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| conn.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE probe (id INTEGER PRIMARY KEY);
             CREATE TABLE child (parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);"
        )).unwrap();
        assert_eq!(store.call_blocking(|conn| conn.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))).unwrap(), 2);
        let (blocker, release) = hold_batch(&store).await;
        let first_store = store.clone();
        let first = tokio::spawn(async move {
            first_store.call(|conn| conn.execute("INSERT INTO probe VALUES (1)", [])).await
        });
        wait_queued(&store, 1).await;
        let (written, ready) = tokio::sync::oneshot::channel();
        let (commit, wait) = std::sync::mpsc::channel();
        let second_store = store.clone();
        let second = tokio::spawn(async move {
            second_store.call(move |conn| {
                conn.execute("INSERT INTO child VALUES (999)", [])?;
                let _ = written.send(());
                wait.recv_timeout(Duration::from_secs(5)).map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(())
            }).await
        });
        wait_queued(&store, 2).await;
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        ready.await.unwrap();
        assert!(!first.is_finished(), "success was returned before the batch committed");
        let reader = Connection::open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(reader.query_row("SELECT count(*) FROM probe", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        commit.send(()).unwrap();
        assert!(first.await.unwrap().is_err());
        assert!(second.await.unwrap().is_err());
        for table in ["probe", "child"] {
            assert_eq!(reader.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        }
        // The failed commit did not leave the worker or connection stuck.
        store.call(|conn| conn.execute("INSERT INTO probe VALUES (2)", [])).await.unwrap();
        assert_eq!(reader.query_row("SELECT id FROM probe", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    }

    #[tokio::test]
    async fn an_aborted_batch_never_commits_later_requests_independently() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| conn.execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY)")).unwrap();
        let (blocker, release) = hold_batch(&store).await;
        let aborted_store = store.clone();
        let aborted = tokio::spawn(async move {
            // Simulate SQLite's automatic transaction rollback on an I/O error.
            aborted_store.call(|conn| conn.execute_batch("ROLLBACK")).await
        });
        wait_queued(&store, 1).await;
        let later_store = store.clone();
        let later = tokio::spawn(async move {
            later_store.call(|conn| conn.execute("INSERT INTO probe VALUES (1)", [])).await
        });
        wait_queued(&store, 2).await;
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        assert!(aborted.await.unwrap().is_err());
        assert!(later.await.unwrap().is_err());
        let reader = Connection::open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(reader.query_row("SELECT count(*) FROM probe", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }
}

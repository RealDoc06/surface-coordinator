//! Record of the job targets this client has started or finished.
//!
//! This is what prevents a job from running twice. An id is written to disk
//! before its job starts; if the client restarts while the job is running,
//! the entry is found unfinished and is turned into an `interrupted`
//! failure rather than being run again. Finished entries keep their result
//! until the coordinator acknowledges it.

use std::io;
use std::path::{Path, PathBuf};

use protocol::{reason, JobResult, JobStatus};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "jobs.json";
/// Acknowledged entries kept for duplicate detection.
const MAX_ACKED_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// `None` while the job is running.
    pub result: Option<JobResult>,
    /// The coordinator has confirmed it stored the result.
    #[serde(default)]
    pub acked: bool,
    /// The job was cancelled before it started, so `job.started` is not
    /// part of what is reported about it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub never_started: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    entries: Vec<Entry>,
}

#[derive(Debug)]
pub struct Ledger {
    path: Option<PathBuf>,
    entries: Vec<Entry>,
}

impl Ledger {
    /// A ledger that lives only as long as the process.
    pub fn in_memory() -> Ledger {
        Ledger { path: None, entries: Vec::new() }
    }

    /// Loads the ledger from `state_dir`. Jobs recorded as started but not
    /// finished were cut short by a restart and become `interrupted`.
    pub fn load(state_dir: &Path) -> io::Result<Ledger> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        let mut entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                Ok(file) => file.entries,
                Err(error) => {
                    // Keep the unreadable file for inspection and start
                    // clean rather than refusing to run at all.
                    tracing::error!(%error, path = %path.display(), "job ledger is corrupt; starting a new one");
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    Vec::new()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut interrupted = false;
        for entry in &mut entries {
            if entry.result.is_none() {
                entry.result = Some(JobResult {
                    job_target_id: entry.id.clone(),
                    status: JobStatus::Failed,
                    exit_code: None,
                    reason: Some(reason::INTERRUPTED.into()),
                    message: Some("the client restarted while the job was running".into()),
                });
                entry.acked = false;
                interrupted = true;
            }
        }
        let ledger = Ledger { path: Some(path), entries };
        if interrupted {
            ledger.save();
        }
        Ok(ledger)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    /// Entries whose result the coordinator has not acknowledged, oldest first.
    pub fn unacked(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|entry| !entry.acked)
    }

    /// Records that a job is about to run.
    pub fn start(&mut self, id: &str) {
        self.entries.push(Entry { id: id.to_owned(), result: None, acked: false, never_started: false });
        self.save();
    }

    /// Records the result of a job that never started, such as one
    /// cancelled while waiting. Like any result it is kept until
    /// acknowledged, and the id is from now on a known duplicate.
    pub fn record_not_started(&mut self, result: JobResult) {
        let id = result.job_target_id.clone();
        self.entries.push(Entry { id, result: Some(result), acked: false, never_started: true });
        self.save();
    }

    /// Whether the coordinator has acknowledged the result for `id`. An id
    /// that is no longer here was acknowledged long ago and pruned.
    pub fn is_acked(&self, id: &str) -> bool {
        self.entries.iter().find(|entry| entry.id == id).is_none_or(|entry| entry.acked)
    }

    pub fn finish(&mut self, result: JobResult) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == result.job_target_id) {
            entry.result = Some(result);
            self.save();
        }
    }

    /// Marks a finished entry as acknowledged. Returns whether anything changed.
    pub fn ack(&mut self, id: &str) -> bool {
        let Some(entry) =
            self.entries.iter_mut().find(|e| e.id == id && e.result.is_some() && !e.acked)
        else {
            return false;
        };
        entry.acked = true;
        self.prune();
        self.save();
        true
    }

    /// Drops the oldest acknowledged entries beyond the limit.
    fn prune(&mut self) {
        let acked = self.entries.iter().filter(|e| e.acked).count();
        let mut excess = acked.saturating_sub(MAX_ACKED_ENTRIES);
        self.entries.retain(|entry| {
            if excess > 0 && entry.acked {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }

    /// Writes the ledger atomically. A failure is logged, not fatal: the
    /// in-memory ledger still protects against duplicates until a restart.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let write = || -> io::Result<()> {
            let bytes = serde_json::to_vec(&File { entries: self.entries.clone() })?;
            let temporary = path.with_extension("json.tmp");
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, path)
        };
        if let Err(error) = write() {
            tracing::error!(%error, path = %path.display(), "failed to save the job ledger");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn succeeded(id: &str) -> JobResult {
        JobResult {
            job_target_id: id.into(),
            status: JobStatus::Succeeded,
            exit_code: Some(0),
            reason: None,
            message: None,
        }
    }

    #[test]
    fn tracks_started_finished_and_acknowledged() {
        let mut ledger = Ledger::in_memory();
        assert!(!ledger.contains("a"));
        ledger.start("a");
        assert!(ledger.contains("a"));
        assert!(!ledger.ack("a"), "a running job cannot be acknowledged");

        ledger.finish(succeeded("a"));
        assert_eq!(ledger.unacked().count(), 1);
        assert!(ledger.ack("a"));
        assert!(!ledger.ack("a"));
        assert_eq!(ledger.unacked().count(), 0);
        assert!(ledger.contains("a"), "acknowledged jobs are still known duplicates");
    }

    #[test]
    fn survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        ledger.start("acked");
        ledger.finish(succeeded("acked"));
        ledger.ack("acked");
        ledger.start("unacked");
        ledger.finish(succeeded("unacked"));

        let reloaded = Ledger::load(dir.path()).unwrap();
        assert!(reloaded.contains("acked"));
        let unacked: Vec<&Entry> = reloaded.unacked().collect();
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].result, Some(succeeded("unacked")));
    }

    #[test]
    fn a_job_running_at_restart_becomes_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        ledger.start("running");
        drop(ledger);

        for _ in 0..2 {
            // The conversion is itself persisted, so it holds on every load.
            let reloaded = Ledger::load(dir.path()).unwrap();
            assert!(reloaded.contains("running"));
            let result = reloaded.unacked().next().unwrap().result.clone().unwrap();
            assert_eq!(result.status, JobStatus::Failed);
            assert_eq!(result.reason.as_deref(), Some(reason::INTERRUPTED));
            assert_eq!(result.job_target_id, "running");
        }
    }

    #[test]
    fn a_job_that_never_started_is_recorded_with_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        let mut cancelled = succeeded("cancelled");
        cancelled.status = JobStatus::Failed;
        cancelled.reason = Some(reason::CANCELLED.into());
        ledger.record_not_started(cancelled.clone());
        assert!(ledger.contains("cancelled"));
        assert!(!ledger.is_acked("cancelled"));

        let mut reloaded = Ledger::load(dir.path()).unwrap();
        let entry = reloaded.unacked().next().unwrap().clone();
        assert!(entry.never_started);
        assert_eq!(entry.result, Some(cancelled));
        assert!(reloaded.ack("cancelled"));
        assert!(reloaded.is_acked("cancelled"));
    }

    #[test]
    fn an_entry_written_before_never_started_existed_counts_as_started() {
        let entry: Entry = serde_json::from_str(r#"{"id":"old","result":null,"acked":false}"#).unwrap();
        assert!(!entry.never_started);
    }

    #[test]
    fn a_corrupt_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"{ not json").unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        assert_eq!(ledger.unacked().count(), 0);
        assert!(dir.path().join("jobs.json.corrupt").exists());
        ledger.start("a");
        assert!(Ledger::load(dir.path()).unwrap().contains("a"));
    }

    #[test]
    fn old_acknowledged_entries_are_pruned_but_unacknowledged_ones_are_kept() {
        let mut ledger = Ledger::in_memory();
        ledger.start("pending-result");
        ledger.finish(succeeded("pending-result"));
        for index in 0..MAX_ACKED_ENTRIES + 5 {
            let id = format!("job-{index}");
            ledger.start(&id);
            ledger.finish(succeeded(&id));
            ledger.ack(&id);
        }
        assert!(!ledger.contains("job-0"));
        assert!(!ledger.contains("job-4"));
        assert!(ledger.contains("job-5"));
        assert!(ledger.contains(&format!("job-{}", MAX_ACKED_ENTRIES + 4)));
        assert!(ledger.contains("pending-result"));
    }
}

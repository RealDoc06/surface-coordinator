//! Record of the job targets this client has started or finished.
//!
//! This is what prevents a job from running twice. An id is written to disk
//! before its job starts; if the client restarts while the job is running,
//! the entry is found unfinished and is turned into an `interrupted`
//! failure rather than being run again. Finished entries keep their result
//! until the coordinator acknowledges it.
//!
//! The one exception is a `client.update` that has replaced the executable:
//! it is unfinished on purpose, marked as awaiting the restart, and is
//! finished by the next start (see `crate::update`).

use std::io::{self, Write};
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
    /// The job restarts the client to complete (`client.update`): still
    /// unfinished after a restart, but not interrupted by it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub awaiting_restart: bool,
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
    /// finished were cut short by a restart and become `interrupted`,
    /// except those awaiting the restart.
    pub fn load(state_dir: &Path) -> io::Result<Ledger> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join(FILE_NAME);
        let mut entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<File>(&bytes).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidData, format!(
                    "job ledger {} is corrupt: {error}; restore a known-good backup before restarting to preserve duplicate protection",
                    path.display(),
                ))
            })?.entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut interrupted = false;
        for entry in &mut entries {
            if entry.result.is_none() && !entry.awaiting_restart {
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
            ledger.save()?;
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

    /// Records that a job is about to run. A persistent ledger must commit
    /// successfully before the caller may perform the job's side effects.
    pub fn start(&mut self, id: &str) -> io::Result<()> {
        self.entries.push(Entry {
            id: id.to_owned(),
            result: None,
            acked: false,
            never_started: false,
            awaiting_restart: false,
        });
        if let Err(error) = self.save() {
            self.entries.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Marks a running job as one that completes after a restart of the
    /// client, so that the restart does not make it `interrupted`. The caller
    /// must persist this transition before writing its update marker or
    /// replacing the executable.
    pub fn await_restart(&mut self, id: &str) -> io::Result<()> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id == id && entry.result.is_none())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("job {id} has no unfinished durable start record"),
                )
            })?;
        let previous = self.entries[index].awaiting_restart;
        self.entries[index].awaiting_restart = true;
        if let Err(error) = self.save() {
            self.entries[index].awaiting_restart = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Unfinished jobs waiting for a restart of the client.
    pub fn awaiting_restart(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.awaiting_restart && entry.result.is_none())
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// Records the result of a job that never started, such as one
    /// cancelled while waiting. Like any result it is kept until
    /// acknowledged, and the id is from now on a known duplicate.
    pub fn record_not_started(&mut self, result: JobResult) {
        let id = result.job_target_id.clone();
        self.entries.push(Entry {
            id,
            result: Some(result),
            acked: false,
            never_started: true,
            awaiting_restart: false,
        });
        self.save_best_effort();
    }

    /// Whether the coordinator has acknowledged the result for `id`. An id
    /// that is no longer here was acknowledged long ago and pruned.
    pub fn is_acked(&self, id: &str) -> bool {
        self.entries.iter().find(|entry| entry.id == id).is_none_or(|entry| entry.acked)
    }

    /// Records the result of a started job. A job the ledger has lost track
    /// of (a `client.update` finished after a restart whose ledger did not
    /// survive) is recorded anew, so that its result is still reported.
    pub fn finish(&mut self, result: JobResult) {
        match self.entries.iter_mut().find(|e| e.id == result.job_target_id) {
            Some(entry) => {
                entry.result = Some(result);
                entry.awaiting_restart = false;
            }
            None => self.entries.push(Entry {
                id: result.job_target_id.clone(),
                result: Some(result),
                acked: false,
                never_started: false,
                awaiting_restart: false,
            }),
        }
        self.save_best_effort();
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
        self.save_best_effort();
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

    /// Flush the staged contents before replacing the previous ledger. In
    /// particular, success here is the permission to begin an irreversible job.
    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let write = || -> io::Result<()> {
            let bytes = serde_json::to_vec(&File { entries: self.entries.clone() })?;
            let temporary = path.with_extension("json.tmp");
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        };
        write().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot persist job ledger {}: {error}", path.display(),),
            )
        })
    }

    /// After a job has durably started, retaining its id already prevents
    /// another execution after restart. Keep newer results in memory if a
    /// later write fails, and retry persistence with the next ledger change.
    fn save_best_effort(&self) {
        if let Err(error) = self.save() {
            tracing::error!(%error, "failed to save the job ledger");
        }
    }

    /// Confirm that all in-memory state is durable before deleting recovery data.
    pub(crate) fn persist(&self) -> io::Result<()> {
        self.save()
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
        ledger.start("a").unwrap();
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
        ledger.start("acked").unwrap();
        ledger.finish(succeeded("acked"));
        ledger.ack("acked");
        ledger.start("unacked").unwrap();
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
        ledger.start("running").unwrap();
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
    fn a_job_awaiting_a_restart_stays_unfinished_across_restarts_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        ledger.start("update").unwrap();
        ledger.await_restart("update").unwrap();
        drop(ledger);

        for _ in 0..2 {
            let reloaded = Ledger::load(dir.path()).unwrap();
            let entry = reloaded.unacked().next().unwrap().clone();
            assert_eq!(entry.result, None, "not interrupted by the restart");
            assert_eq!(reloaded.awaiting_restart(), vec!["update".to_owned()]);
        }

        let mut reloaded = Ledger::load(dir.path()).unwrap();
        reloaded.finish(succeeded("update"));
        assert!(reloaded.awaiting_restart().is_empty());
        let entry = Ledger::load(dir.path()).unwrap().unacked().next().unwrap().clone();
        assert_eq!(entry.result, Some(succeeded("update")));
        assert!(!entry.awaiting_restart);
    }

    #[test]
    fn finishing_a_job_the_ledger_does_not_know_records_it() {
        let mut ledger = Ledger::in_memory();
        ledger.finish(succeeded("lost"));
        let entry = ledger.unacked().next().unwrap().clone();
        assert_eq!(entry.result, Some(succeeded("lost")));
        assert!(!entry.never_started);
        assert!(ledger.ack("lost"));
    }

    #[test]
    fn awaiting_restart_must_persist_before_the_update_can_continue() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        ledger.start("update").unwrap();
        let path = dir.path().join(FILE_NAME);
        let previous = std::fs::read(&path).unwrap();
        let blocked = dir.path().join("jobs.json.tmp");
        std::fs::create_dir(&blocked).unwrap();

        assert!(ledger.await_restart("update").is_err());
        assert!(ledger.awaiting_restart().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(ledger.unacked().next().unwrap().result, None);
        std::fs::remove_dir(blocked).unwrap();
        ledger.await_restart("update").unwrap();
        let reloaded = Ledger::load(dir.path()).unwrap();
        assert_eq!(reloaded.awaiting_restart(), ["update"]);
        assert_eq!(reloaded.unacked().next().unwrap().result, None);
    }

    #[test]
    fn awaiting_restart_rejects_jobs_without_an_unfinished_start_record() {
        let mut ledger = Ledger::in_memory();
        assert_eq!(
            ledger.await_restart("missing").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        ledger.start("finished").unwrap();
        ledger.finish(succeeded("finished"));
        assert_eq!(
            ledger.await_restart("finished").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(ledger.awaiting_restart().is_empty());
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
        let entry: Entry =
            serde_json::from_str(r#"{"id":"old","result":null,"acked":false}"#).unwrap();
        assert!(!entry.never_started);
        assert!(!entry.awaiting_restart);
    }

    #[test]
    fn a_corrupt_ledger_is_preserved_and_prevents_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"{ not json").unwrap();
        for _ in 0..2 {
            let error = Ledger::load(dir.path()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(path.to_str().unwrap()));
            assert!(error.to_string().contains("restore a known-good backup"));
            assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        }
        assert!(!dir.path().join("jobs.json.corrupt").exists());
    }

    #[test]
    fn a_failed_start_preserves_prior_history_and_can_be_retried_after_repair() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path()).unwrap();
        ledger.start("previous").unwrap();
        ledger.finish(succeeded("previous"));
        ledger.ack("previous");
        let path = dir.path().join(FILE_NAME);
        let previous = std::fs::read(&path).unwrap();
        let blocked = dir.path().join("jobs.json.tmp");
        std::fs::create_dir(&blocked).unwrap();

        assert!(ledger.start("unsafe").is_err());
        assert!(!ledger.contains("unsafe"));
        assert!(ledger.contains("previous"));
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        let reloaded = Ledger::load(dir.path()).unwrap();
        assert!(reloaded.contains("previous"));
        assert!(!reloaded.contains("unsafe"));

        std::fs::remove_dir(blocked).unwrap();
        ledger.start("unsafe").unwrap();
        assert!(Ledger::load(dir.path()).unwrap().contains("unsafe"));
    }

    #[test]
    fn old_acknowledged_entries_are_pruned_but_unacknowledged_ones_are_kept() {
        let mut ledger = Ledger::in_memory();
        ledger.start("pending-result").unwrap();
        ledger.finish(succeeded("pending-result"));
        for index in 0..MAX_ACKED_ENTRIES + 5 {
            let id = format!("job-{index}");
            ledger.start(&id).unwrap();
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

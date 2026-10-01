//! Job lifecycle on the client.
//!
//! Jobs are queued as they arrive and run one at a time in order. Everything
//! reported about a job (`job.started`, output, result) is retained until
//! the coordinator acknowledges the result and is replayed whenever a new
//! connection is established, so a job outlives connection loss.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::{
    job_kind, reason, ClientMessage, JobResult, JobStatus, ScriptRunPayload, MAX_OUTPUT_BYTES,
};
use tokio::sync::mpsc;

use crate::ledger::Ledger;
use crate::platform::{OutputChunk, Platform, PlatformError, ScriptOutcome, ScriptRequest};

/// A job as received from the coordinator.
#[derive(Debug, Clone)]
pub struct Job {
    pub job_target_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Output retained for one job until its result is acknowledged.
#[derive(Default)]
struct Retained {
    chunks: Vec<ClientMessage>,
    next_seq: u64,
    bytes: usize,
}

struct Inner {
    ledger: Ledger,
    /// Accepted and waiting for their turn. Not persisted: a job that never
    /// started is safe to run when the coordinator sends it again.
    waiting: HashSet<String>,
    retained: HashMap<String, Retained>,
    /// Sender into the current connection, if there is one.
    session: Option<mpsc::UnboundedSender<ClientMessage>>,
}

impl Inner {
    fn send(&self, message: ClientMessage) {
        if let Some(session) = &self.session {
            // A closed session just means the connection is going away; the
            // message is replayed on the next one.
            let _ = session.send(message);
        }
    }
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<Mutex<Inner>>,
    queue: mpsc::UnboundedSender<Job>,
}

impl JobManager {
    /// Creates the manager and the queue its worker consumes. With a state
    /// directory the record of started jobs survives restarts.
    pub fn new(state_dir: Option<&Path>) -> std::io::Result<(JobManager, mpsc::UnboundedReceiver<Job>)> {
        let ledger = match state_dir {
            Some(dir) => Ledger::load(dir)?,
            None => Ledger::in_memory(),
        };
        let (queue, receiver) = mpsc::unbounded_channel();
        let inner = Inner {
            ledger,
            waiting: HashSet::new(),
            retained: HashMap::new(),
            session: None,
        };
        Ok((JobManager { inner: Arc::new(Mutex::new(inner)), queue }, receiver))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes a job from the coordinator. A job target that is already
    /// waiting, running or finished is ignored.
    pub fn accept(&self, job: Job) {
        let mut inner = self.lock();
        let id = &job.job_target_id;
        if inner.ledger.contains(id) || inner.waiting.contains(id) {
            tracing::debug!(job_target_id = %id, "ignoring duplicate job");
            return;
        }
        tracing::info!(job_target_id = %id, kind = %job.kind, "job accepted");
        inner.waiting.insert(id.clone());
        let _ = self.queue.send(job);
    }

    /// Routes messages to a new connection and replays everything the
    /// coordinator has not acknowledged.
    pub fn attach(&self, session: mpsc::UnboundedSender<ClientMessage>) {
        let mut inner = self.lock();
        for entry in inner.ledger.unacked() {
            let _ = session.send(ClientMessage::JobStarted { job_target_id: entry.id.clone() });
            if let Some(retained) = inner.retained.get(&entry.id) {
                for chunk in &retained.chunks {
                    let _ = session.send(chunk.clone());
                }
            }
            if let Some(result) = &entry.result {
                let _ = session.send(ClientMessage::JobResult(result.clone()));
            }
        }
        inner.session = Some(session);
    }

    pub fn detach(&self) {
        self.lock().session = None;
    }

    /// The coordinator has stored the result; nothing more to replay.
    pub fn acknowledge(&self, job_target_id: &str) {
        let mut inner = self.lock();
        if inner.ledger.ack(job_target_id) {
            inner.retained.remove(job_target_id);
        }
    }

    fn begin(&self, job_target_id: &str) {
        let mut inner = self.lock();
        inner.waiting.remove(job_target_id);
        // On disk before the job runs, so it can never run twice.
        inner.ledger.start(job_target_id);
        inner.send(ClientMessage::JobStarted { job_target_id: job_target_id.to_owned() });
    }

    fn output(&self, job_target_id: &str, chunk: OutputChunk) {
        let mut inner = self.lock();
        let retained = inner.retained.entry(job_target_id.to_owned()).or_default();
        // The coordinator stores at most this much. The chunk that crosses
        // the limit is still sent so the coordinator marks the truncation.
        if retained.bytes > MAX_OUTPUT_BYTES {
            return;
        }
        retained.bytes += chunk.data.len();
        retained.next_seq += 1;
        let message = ClientMessage::JobOutput {
            job_target_id: job_target_id.to_owned(),
            seq: retained.next_seq,
            stream: chunk.stream,
            data: chunk.data,
        };
        retained.chunks.push(message.clone());
        inner.send(message);
    }

    fn finish(&self, result: JobResult) {
        let mut inner = self.lock();
        tracing::info!(
            job_target_id = %result.job_target_id,
            status = ?result.status,
            exit_code = ?result.exit_code,
            reason = result.reason.as_deref().unwrap_or("-"),
            "job finished"
        );
        inner.ledger.finish(result.clone());
        inner.send(ClientMessage::JobResult(result));
    }
}

/// Runs queued jobs one at a time until the queue closes.
pub async fn run_worker<P: Platform>(
    jobs: JobManager,
    platform: Arc<P>,
    mut queue: mpsc::UnboundedReceiver<Job>,
) {
    while let Some(job) = queue.recv().await {
        let id = job.job_target_id.clone();
        jobs.begin(&id);
        // In its own task so that a panic in a platform implementation
        // fails the job instead of ending the worker.
        let task = tokio::spawn(execute(jobs.clone(), platform.clone(), job));
        let result = match task.await {
            Ok(result) => result,
            Err(error) => failed(&id, reason::ERROR, Some(format!("job task failed: {error}"))),
        };
        jobs.finish(result);
    }
}

fn failed(job_target_id: &str, reason: &str, message: Option<String>) -> JobResult {
    JobResult {
        job_target_id: job_target_id.to_owned(),
        status: JobStatus::Failed,
        exit_code: None,
        reason: Some(reason.to_owned()),
        message,
    }
}

async fn execute<P: Platform>(jobs: JobManager, platform: Arc<P>, job: Job) -> JobResult {
    let id = job.job_target_id.as_str();
    match job.kind.as_str() {
        job_kind::SCRIPT_RUN => {
            let payload: ScriptRunPayload = match serde_json::from_value(job.payload) {
                Ok(payload) => payload,
                Err(error) => return failed(id, reason::INVALID_PAYLOAD, Some(error.to_string())),
            };
            let request = ScriptRequest {
                shell: payload.shell,
                body: payload.body,
                timeout: Duration::from_secs(payload.timeout_secs),
                run_as: payload.run_as,
            };
            run_script(&jobs, platform.as_ref(), id, request).await
        }
        other => failed(
            id,
            reason::UNSUPPORTED,
            Some(format!("job kind {other:?} is not supported by this client")),
        ),
    }
}

async fn run_script<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    id: &str,
    request: ScriptRequest,
) -> JobResult {
    let (sender, mut receiver) = mpsc::channel::<OutputChunk>(64);
    let run = platform.run_script(request, sender);
    tokio::pin!(run);
    let outcome = loop {
        tokio::select! {
            biased;
            Some(chunk) = receiver.recv() => jobs.output(id, chunk),
            outcome = &mut run => break outcome,
        }
    };
    // Whatever was sent just before the script finished.
    receiver.close();
    while let Some(chunk) = receiver.recv().await {
        jobs.output(id, chunk);
    }

    match outcome {
        Ok(ScriptOutcome::Exited(0)) => JobResult {
            job_target_id: id.to_owned(),
            status: JobStatus::Succeeded,
            exit_code: Some(0),
            reason: None,
            message: None,
        },
        Ok(ScriptOutcome::Exited(code)) => JobResult {
            job_target_id: id.to_owned(),
            status: JobStatus::Failed,
            exit_code: Some(code),
            reason: Some(reason::NONZERO_EXIT.to_owned()),
            message: None,
        },
        Ok(ScriptOutcome::TimedOut) => {
            failed(id, reason::TIMEOUT, Some("the script was stopped at its timeout".into()))
        }
        Err(PlatformError::Unsupported(message)) => failed(id, reason::UNSUPPORTED, Some(message)),
        Err(PlatformError::Failed(message)) => failed(id, reason::ERROR, Some(message)),
    }
}

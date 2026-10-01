//! Job lifecycle on the client.
//!
//! Jobs are queued as they arrive and run one at a time in order. Everything
//! reported about a job (`job.started`, output, result) is retained until
//! the coordinator acknowledges the result and is replayed whenever a new
//! connection is established, so a job outlives connection loss.
//!
//! A job can be cancelled while it waits, and while it runs until it passes
//! its point of no return (see [`JobManager::commit`]). A running job is
//! cancelled by dropping its future, which for a script stops the script's
//! process tree.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::{
    job_kind, reason, settings, AppLaunchPayload, ClientMessage, JobResult, JobStatus, PowerAction,
    PowerPayload, ScriptRunPayload, SettingApplyPayload, WifiJoinPayload, MAX_OUTPUT_BYTES,
};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use crate::ledger::Ledger;
use crate::network::Network;
use crate::platform::{
    OutputChunk, Platform, PlatformError, ScriptOutcome, ScriptRequest, WifiJoinRequest,
};

/// Longest SSID 802.11 allows, in bytes.
const MAX_SSID_BYTES: usize = 32;

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

/// The job the worker is running.
struct Running {
    id: String,
    cancel: CancellationToken,
    /// Past the point of no return: a cancel is ignored from here on.
    committed: bool,
}

struct Inner {
    ledger: Ledger,
    /// Accepted and waiting for their turn. Not persisted: a job that never
    /// started is safe to run when the coordinator sends it again. A job
    /// cancelled while waiting stays in the queue but leaves this set, which
    /// is how the worker knows to skip it.
    waiting: HashSet<String>,
    running: Option<Running>,
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
    /// Woken whenever a result is acknowledged.
    acknowledged: Arc<Notify>,
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
            running: None,
            retained: HashMap::new(),
            session: None,
        };
        let manager = JobManager {
            inner: Arc::new(Mutex::new(inner)),
            queue,
            acknowledged: Arc::new(Notify::new()),
        };
        Ok((manager, receiver))
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
            if !entry.never_started {
                let _ = session.send(ClientMessage::JobStarted { job_target_id: entry.id.clone() });
            }
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
            self.acknowledged.notify_waiters();
        }
    }

    /// Resolves once the coordinator has acknowledged the result of
    /// `job_target_id`.
    async fn wait_for_ack(&self, job_target_id: &str) {
        loop {
            let notified = self.acknowledged.notified();
            tokio::pin!(notified);
            // Registered before the check, so an acknowledgement arriving in
            // between still wakes this waiter.
            notified.as_mut().enable();
            if self.lock().ledger.is_acked(job_target_id) {
                return;
            }
            notified.await;
        }
    }

    /// The coordinator asks to stop a job. A waiting job is taken out of the
    /// queue and reported cancelled at once; a running job that has not
    /// passed its point of no return is stopped by the worker, which then
    /// reports it cancelled. Anything else (unknown, finished, committed) is
    /// ignored: its result is, or will be, reported as usual.
    pub fn cancel(&self, job_target_id: &str) {
        let mut inner = self.lock();
        if inner.waiting.remove(job_target_id) {
            tracing::info!(job_target_id, "job cancelled before it started");
            let result =
                failed(job_target_id, reason::CANCELLED, Some("cancelled before it started".into()));
            // Through the ledger like any result: replayed until
            // acknowledged, and the id is a duplicate from now on, so the
            // job never runs, not even when the coordinator sends it again.
            inner.ledger.record_not_started(result.clone());
            inner.send(ClientMessage::JobResult(result));
            return;
        }
        match &inner.running {
            Some(running) if running.id == job_target_id && !running.committed => {
                tracing::info!(job_target_id, "cancelling the running job");
                running.cancel.cancel();
            }
            Some(running) if running.id == job_target_id => {
                tracing::info!(job_target_id, "job is past the point where it can be cancelled");
            }
            _ => tracing::debug!(job_target_id, "ignoring job.cancel for a job that is neither waiting nor running"),
        }
    }

    /// Marks the point of no return of a running job, such as starting a
    /// process or changing the network: from here on a cancel is ignored.
    /// Returns `false` if the job was cancelled before it got here; it must
    /// then stop without doing anything.
    fn commit(&self, job_target_id: &str) -> bool {
        let mut inner = self.lock();
        match &mut inner.running {
            Some(running) if running.id == job_target_id => {
                if running.cancel.is_cancelled() {
                    return false;
                }
                running.committed = true;
                true
            }
            // Not run by the worker, so nothing can cancel it either.
            _ => true,
        }
    }

    /// Takes a job out of the waiting set and records it as started.
    /// Returns `None` for a job cancelled while waiting, which is skipped.
    fn begin(&self, job_target_id: &str) -> Option<CancellationToken> {
        let mut inner = self.lock();
        if !inner.waiting.remove(job_target_id) {
            return None;
        }
        // On disk before the job runs, so it can never run twice.
        inner.ledger.start(job_target_id);
        inner.send(ClientMessage::JobStarted { job_target_id: job_target_id.to_owned() });
        let cancel = CancellationToken::new();
        inner.running =
            Some(Running { id: job_target_id.to_owned(), cancel: cancel.clone(), committed: false });
        Some(cancel)
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
        if inner.running.as_ref().is_some_and(|running| running.id == result.job_target_id) {
            inner.running = None;
        }
        inner.ledger.finish(result.clone());
        inner.send(ClientMessage::JobResult(result));
    }
}

/// What jobs need beyond the platform.
#[derive(Clone)]
pub struct WorkerContext {
    pub network: Arc<Network>,
    /// How long a `wifi.join` waits for a coordinator on the new network.
    pub handoff_timeout: Duration,
    /// How long a `power` job waits for the coordinator to acknowledge its
    /// result before carrying out the action anyway.
    pub power_ack_timeout: Duration,
}

/// What running a job came to.
struct Outcome {
    result: JobResult,
    /// A power action to carry out once the result is out.
    then: Option<PowerAction>,
}

impl From<JobResult> for Outcome {
    fn from(result: JobResult) -> Outcome {
        Outcome { result, then: None }
    }
}

/// Runs queued jobs one at a time until the queue closes.
pub async fn run_worker<P: Platform>(
    jobs: JobManager,
    platform: Arc<P>,
    context: WorkerContext,
    mut queue: mpsc::UnboundedReceiver<Job>,
) {
    while let Some(job) = queue.recv().await {
        let id = job.job_target_id.clone();
        let Some(cancel) = jobs.begin(&id) else {
            tracing::debug!(job_target_id = %id, "skipping a job cancelled while waiting");
            continue;
        };
        // In its own task so that a panic in a platform implementation
        // fails the job instead of ending the worker, and so that a cancel
        // can drop it wherever it is.
        let mut task = tokio::spawn(execute(jobs.clone(), platform.clone(), context.clone(), job));
        let joined = tokio::select! {
            biased;
            joined = &mut task => joined,
            _ = cancel.cancelled() => {
                task.abort();
                // Resolves once the job's future has been dropped, so what
                // it started has been stopped before the result goes out. A
                // job that finished in the meantime keeps its own result.
                (&mut task).await
            }
        };
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(error) if error.is_cancelled() => {
                failed(&id, reason::CANCELLED, Some("cancelled while running".into())).into()
            }
            Err(error) => failed(&id, reason::ERROR, Some(format!("job task failed: {error}"))).into(),
        };
        jobs.finish(outcome.result);
        if let Some(action) = outcome.then {
            carry_out_power(&jobs, platform.as_ref(), &context, &id, action).await;
        }
    }
}

/// The second half of a `power` job, after its result has been reported:
/// wait until the coordinator has stored it (so that the device going away
/// is not taken for a failure) or the timeout has passed, then act. The
/// worker waits here, so no other job starts just before the device goes
/// down.
async fn carry_out_power<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    context: &WorkerContext,
    id: &str,
    action: PowerAction,
) {
    let timeout = context.power_ack_timeout;
    if tokio::time::timeout(timeout, jobs.wait_for_ack(id)).await.is_err() {
        tracing::warn!(
            job_target_id = %id,
            "power result not acknowledged within {}; carrying out the action anyway",
            describe(timeout)
        );
    }
    tracing::info!(job_target_id = %id, ?action, "carrying out power action");
    // The job has already been reported as succeeded; all that can be done
    // with a failure now is to log it.
    if let Err(error) = platform.power(action).await {
        tracing::error!(job_target_id = %id, ?action, %error, "power action failed after it was reported");
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

fn succeeded(job_target_id: &str, message: String) -> JobResult {
    JobResult {
        job_target_id: job_target_id.to_owned(),
        status: JobStatus::Succeeded,
        exit_code: None,
        reason: None,
        message: Some(message),
    }
}

/// A platform error as a job result.
fn platform_failure(job_target_id: &str, error: PlatformError) -> JobResult {
    let (reason, message) = match error {
        PlatformError::Unsupported(message) => (reason::UNSUPPORTED, message),
        PlatformError::NoUserSession(message) => (reason::NO_USER_SESSION, message),
        PlatformError::Failed(message) => (reason::ERROR, message),
    };
    failed(job_target_id, reason, Some(message))
}

/// What a job reports when it finds at its point of no return that it was
/// cancelled on the way there. Normally the worker has already dropped it.
fn cancelled(job_target_id: &str) -> JobResult {
    failed(job_target_id, reason::CANCELLED, Some("cancelled while running".into()))
}

fn parse<T: DeserializeOwned>(job_target_id: &str, payload: serde_json::Value) -> Result<T, JobResult> {
    serde_json::from_value(payload)
        .map_err(|error| failed(job_target_id, reason::INVALID_PAYLOAD, Some(error.to_string())))
}

async fn execute<P: Platform>(
    jobs: JobManager,
    platform: Arc<P>,
    context: WorkerContext,
    job: Job,
) -> Outcome {
    let id = job.job_target_id.as_str();
    let platform = platform.as_ref();
    let payload = job.payload;
    let outcome = match job.kind.as_str() {
        job_kind::WIFI_JOIN => match parse(id, payload) {
            Ok(payload) => wifi_join(&jobs, platform, &context, id, payload).await,
            Err(result) => result,
        },
        job_kind::SCRIPT_RUN => match parse::<ScriptRunPayload>(id, payload) {
            Ok(payload) => {
                let request = ScriptRequest {
                    shell: payload.shell,
                    body: payload.body,
                    timeout: Duration::from_secs(payload.timeout_secs),
                    run_as: payload.run_as,
                };
                run_script(&jobs, platform, id, request).await
            }
            Err(result) => result,
        },
        job_kind::APP_LAUNCH => match parse(id, payload) {
            Ok(payload) => launch_app(&jobs, platform, id, payload).await,
            Err(result) => result,
        },
        job_kind::SETTING_APPLY => match parse(id, payload) {
            Ok(payload) => apply_setting(&jobs, platform, id, payload).await,
            Err(result) => result,
        },
        job_kind::POWER => match parse::<PowerPayload>(id, payload) {
            Ok(payload) => return power(&jobs, platform, id, payload.action),
            Err(result) => result,
        },
        other => failed(
            id,
            reason::UNSUPPORTED,
            Some(format!("job kind {other:?} is not supported by this client")),
        ),
    };
    outcome.into()
}

/// Scripts have no point of no return: a cancel stops them wherever they are.
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
        Err(error) => platform_failure(id, error),
    }
}

/// Starting a process is the point of no return: a cancel arriving while it
/// starts could not stop it, so it is not attempted.
async fn launch_app<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    id: &str,
    payload: AppLaunchPayload,
) -> JobResult {
    if payload.path.trim().is_empty() {
        return failed(id, reason::INVALID_PAYLOAD, Some("the path is empty".into()));
    }
    if !jobs.commit(id) {
        return cancelled(id);
    }
    let path = payload.path;
    match platform.launch_app(path.clone(), payload.arguments).await {
        Ok(pid) => succeeded(id, format!("started {path} (pid {pid})")),
        Err(error) => platform_failure(id, error),
    }
}

/// A setting is applied whole or not at all as far as cancelling goes:
/// stopping it half way could leave it half applied.
async fn apply_setting<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    id: &str,
    payload: SettingApplyPayload,
) -> JobResult {
    let SettingApplyPayload { setting, value } = payload;
    // A newer coordinator may offer settings this client does not know yet;
    // that is the client lacking support, not a bad request.
    if settings::find(&setting).is_none() {
        return failed(
            id,
            reason::UNSUPPORTED,
            Some(format!("setting {setting:?} is not supported by this client")),
        );
    }
    if let Err(problem) = settings::validate(&setting, &value) {
        return failed(id, reason::INVALID_PAYLOAD, Some(problem));
    }
    if !jobs.commit(id) {
        return cancelled(id);
    }
    let applied = format!("set {setting} to {value}");
    match platform.apply_setting(setting, value).await {
        Ok(()) => succeeded(id, applied),
        Err(error) => platform_failure(id, error),
    }
}

/// The spec has the device act after acknowledging the job, so the result
/// is reported first and the action carried out by the worker afterwards.
/// Whether the platform can do it at all is asked before: once success has
/// been reported a failure can only be logged.
fn power<P: Platform>(jobs: &JobManager, platform: &P, id: &str, action: PowerAction) -> Outcome {
    if let Err(error) = platform.check_power(action) {
        return platform_failure(id, error).into();
    }
    if !jobs.commit(id) {
        return cancelled(id).into();
    }
    let message = match action {
        PowerAction::Restart => "restarting",
        PowerAction::Shutdown => "shutting down",
        PowerAction::Sleep => "going to sleep",
    };
    Outcome { result: succeeded(id, message.into()), then: Some(action) }
}

fn validate_wifi_join(payload: &WifiJoinPayload) -> Result<(), String> {
    if payload.ssid.is_empty() {
        return Err("the SSID is empty".into());
    }
    if payload.ssid.len() > MAX_SSID_BYTES {
        return Err(format!("the SSID is longer than {MAX_SSID_BYTES} bytes"));
    }
    if let Some(url) = &payload.coordinator_url {
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return Err(format!("the coordinator URL {url:?} does not start with ws:// or wss://"));
        }
    }
    Ok(())
}

/// Wi-Fi handoff: join the network, then reach a coordinator from it within
/// the handoff timeout, or go back to where the device came from.
///
/// The result is not sent on the current session (that is closed on
/// purpose) but through the ledger, to whichever coordinator the client
/// reaches next.
///
/// Joining is the point of no return: a cancel after that is ignored, as a
/// network change is not undone.
async fn wifi_join<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    context: &WorkerContext,
    id: &str,
    payload: WifiJoinPayload,
) -> JobResult {
    if let Err(problem) = validate_wifi_join(&payload) {
        return failed(id, reason::INVALID_PAYLOAD, Some(problem));
    }
    let network = &context.network;
    let ssid = payload.ssid;
    let previous = network.override_url();
    let handoff = network.begin_handoff();

    let request = WifiJoinRequest {
        ssid: ssid.clone(),
        passphrase: payload.password,
        // The operator chose this network for good.
        auto_connect: true,
    };
    let joined = {
        let _wifi = network.wifi_lock().await;
        if !jobs.commit(id) {
            return cancelled(id);
        }
        platform.wifi_join(request).await
    };
    match joined {
        Ok(()) => {}
        // Still on the old network, nothing changed.
        Err(error) => return platform_failure(id, error),
    }
    tracing::info!(%ssid, "joined Wi-Fi network; reconnecting");

    // Only sessions established after the join count: one that came up
    // while the join was under way may still be on the old network.
    let before = network.generation();
    if let Some(url) = payload.coordinator_url {
        network.set_override(Some(url));
    }
    network.reconnect_now();

    let timeout = context.handoff_timeout;
    match tokio::time::timeout(timeout, network.wait_for_connection_after(before)).await {
        Ok(()) => succeeded(id, format!("joined {ssid}")),
        Err(_) => {
            tracing::warn!(%ssid, "no coordinator reached on the new network; going back");
            network.set_override(previous);
            drop(handoff);
            // Searching rejoins the hotspot if it is still there.
            network.search_now();
            failed(
                id,
                reason::TIMEOUT,
                Some(format!(
                    "no coordinator could be reached on {ssid} within {}",
                    describe(timeout)
                )),
            )
        }
    }
}

fn describe(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

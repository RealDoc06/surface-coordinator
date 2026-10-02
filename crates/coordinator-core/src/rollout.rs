//! The automatic, staged rollout of the newest cached client release.
//!
//! With auto-update on, [`run`] looks at the newest cached release L and the
//! `client.update` attempts for it, and creates at most one job:
//!
//! - Until some device has really been updated to L (a succeeded target that
//!   is not "already on version"), one outdated online device, the canary,
//!   gets L on its own. While its attempt is unresolved nothing else happens.
//! - If an automatic attempt failed (or expired) before any success, the
//!   rollout is halted until an operator updates a device by hand.
//! - After a success every outdated online device gets L in one job.
//!
//! "Outdated" means online, reporting a version older than L, and without any
//! attempt for L yet, so each device gets one automatic attempt per version.
//! It runs from the sweep, after a release is cached, when a device comes
//! online and when auto-update is turned on.

use protocol::job_kind;
use protocol::update::Version;
use serde::Serialize;
use serde_json::json;

use crate::state::AppState;
use crate::store::{self, target_state, Targets, UpdateAttempt};
use crate::{api, Error};

/// `created_by` of the jobs the rollout creates.
pub const CREATED_BY: &str = "auto-update";

/// `Updates.rollout` in `docs/api.md`.
#[derive(Debug, Serialize)]
pub struct Rollout {
    version: String,
    state: Phase,
    canary_device_id: Option<String>,
    /// Devices reporting L.
    updated: usize,
    /// Devices older than L without a finished attempt for it.
    pending: usize,
    /// Devices older than L whose latest attempt for it failed or expired.
    failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Canary,
    Rolling,
    Halted,
    Done,
}

/// What the rollout of one version is decided from.
struct Facts {
    latest: Version,
    /// Every known device's id and reported version, ordered by id.
    devices: Vec<(String, String)>,
    /// `client.update` targets for `latest`, oldest first.
    attempts: Vec<UpdateAttempt>,
}

fn unresolved(attempt: &UpdateAttempt) -> bool {
    matches!(attempt.state.as_str(), "queued" | "sent" | "running")
}

fn failed(attempt: &UpdateAttempt) -> bool {
    attempt.state == target_state::FAILED || attempt.state == "expired"
}

/// A device was actually updated, not found to be on the version already.
fn really_updated(attempt: &UpdateAttempt) -> bool {
    attempt.state == target_state::SUCCEEDED
        && !attempt.message.as_deref().is_some_and(|message| message.starts_with(store::ALREADY_ON_VERSION))
}

impl Facts {
    fn older(&self, version: &str) -> bool {
        version.parse::<Version>().is_ok_and(|version| version < self.latest)
    }

    fn any_success(&self) -> bool {
        self.attempts.iter().any(really_updated)
    }

    fn halted(&self) -> bool {
        !self.any_success() && self.attempts.iter().any(|attempt| attempt.automatic && failed(attempt))
    }

    /// Devices that would get L next, given who is online.
    fn outdated<'a>(&'a self, online: impl Fn(&str) -> bool + 'a) -> impl Iterator<Item = &'a str> + 'a {
        self.devices
            .iter()
            .filter(move |(id, version)| {
                online(id) && self.older(version) && !self.attempts.iter().any(|attempt| attempt.device_id == *id)
            })
            .map(|(id, _)| id.as_str())
    }

    /// The devices of the job to create now, if any.
    fn next_targets(&self, online: impl Fn(&str) -> bool) -> Vec<String> {
        if self.any_success() {
            return self.outdated(online).map(str::to_owned).collect();
        }
        let canary_busy = self.attempts.iter().any(|attempt| attempt.automatic && unresolved(attempt));
        if self.halted() || canary_busy {
            return Vec::new();
        }
        // Devices are ordered by id, so the canary is the lowest id.
        self.outdated(online).take(1).map(str::to_owned).collect()
    }

    fn status(&self) -> Rollout {
        let (mut updated, mut pending, mut failed_count) = (0, 0, 0);
        for (id, version) in &self.devices {
            if version.parse::<Version>().is_ok_and(|version| version == self.latest) {
                updated += 1;
            } else if self.older(version) {
                // A device's latest attempt is what counts: a failure followed
                // by a manual retry in progress is pending again.
                match self.attempts.iter().rev().find(|attempt| attempt.device_id == *id) {
                    None => pending += 1,
                    Some(attempt) if unresolved(attempt) => pending += 1,
                    Some(attempt) if failed(attempt) => failed_count += 1,
                    // Succeeded: counted as updated once it reports L.
                    Some(_) => {}
                }
            }
        }
        let state = if pending == 0 {
            Phase::Done
        } else if self.halted() {
            Phase::Halted
        } else if !self.any_success() {
            Phase::Canary
        } else {
            Phase::Rolling
        };
        Rollout {
            version: self.latest.to_string(),
            state,
            canary_device_id: self.attempts.iter().find(|attempt| attempt.automatic).map(|a| a.device_id.clone()),
            updated,
            pending,
            failed: failed_count,
        }
    }
}

/// The facts for the newest cached release, if auto-update is on and there is one.
async fn facts(state: &AppState) -> Result<Option<Facts>, Error> {
    if !state.updates.auto_update() {
        return Ok(None);
    }
    let Some(latest) = state.updates.newest() else { return Ok(None) };
    let version = latest.to_string();
    let (devices, attempts) = state
        .store
        .call(move |conn| Ok((store::device_versions(conn)?, store::update_attempts(conn, &version, CREATED_BY)?)))
        .await?;
    Ok(Some(Facts { latest, devices, attempts }))
}

/// `Updates.rollout`: `None` while auto-update is off or nothing is cached.
pub async fn status(state: &AppState) -> Result<Option<Rollout>, Error> {
    Ok(facts(state).await?.map(|facts| facts.status()))
}

/// Takes the next rollout step, if there is one. Failures are logged; the
/// next run tries again.
pub async fn run(state: &AppState) {
    let _one_at_a_time = state.updates.rollout.lock().await;
    let facts = match facts(state).await {
        Ok(Some(facts)) => facts,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, "update rollout failed to read its state");
            return;
        }
    };
    let targets = facts.next_targets(|id| state.connection(id).is_some());
    if targets.is_empty() {
        return;
    }
    let version = facts.latest.to_string();
    tracing::info!(%version, devices = targets.len(), canary = !facts.any_success(), "rolling out a client update");
    let payload = json!({"version": version, "downgrade": false});
    let created =
        api::submit_job(state, job_kind::CLIENT_UPDATE.to_owned(), payload, Targets::Devices(targets), Some(CREATED_BY.to_owned()))
            .await;
    if let Err(error) = created {
        tracing::error!(%version, code = error.code, message = %error.message, "update rollout could not create its job");
    }
}

//! Wire protocol shared by the Surface Coordinator client and coordinator.
//!
//! Every message is a JSON object with a `type` tag. A message whose `type`
//! is not known decodes as `Unknown` and must be ignored by the receiver, and
//! unknown fields are ignored, so either side can be newer than the other.

pub mod handshake;
pub mod hotspot;
pub mod settings;
pub mod update;

use serde::{Deserialize, Serialize};

/// Protocol version carried in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Path of the client WebSocket endpoint on the coordinator.
pub const CLIENT_WS_PATH: &str = "/client/ws";

/// Port on which a coordinator embedded in the manager accepts clients. Fixed,
/// so that a client on the manager's hotspot knows where to connect.
pub const EMBEDDED_CLIENT_PORT: u16 = 47800;

/// Heartbeat interval a coordinator announces unless configured otherwise.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// A peer is considered gone after this many heartbeat intervals of silence.
pub const MISSED_HEARTBEATS_BEFORE_OFFLINE: u32 = 3;

/// Cap on the output stored per job target. Excess is truncated.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Job kinds. `kind` is a free string on the wire so new kinds need no
/// protocol change; a client reports kinds it does not know as `unsupported`.
pub mod job_kind {
    pub const SCRIPT_RUN: &str = "script.run";
    pub const WIFI_JOIN: &str = "wifi.join";
    pub const APP_LAUNCH: &str = "app.launch";
    pub const SETTING_APPLY: &str = "setting.apply";
    pub const POWER: &str = "power";
}

/// Machine-readable reasons carried by a failed `job.result`.
pub mod reason {
    pub const UNSUPPORTED: &str = "unsupported";
    pub const TIMEOUT: &str = "timeout";
    pub const NONZERO_EXIT: &str = "nonzero_exit";
    pub const INVALID_PAYLOAD: &str = "invalid_payload";
    pub const INTERRUPTED: &str = "interrupted";
    pub const ERROR: &str = "error";
    pub const CANCELLED: &str = "cancelled";
    pub const NO_USER_SESSION: &str = "no_user_session";
}

/// Reasons carried by `reject`.
pub mod reject_reason {
    pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
    pub const UNKNOWN_KEY: &str = "unknown_key";
    pub const BAD_MAC: &str = "bad_mac";
    pub const BAD_REQUEST: &str = "bad_request";
}

/// Messages sent by the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "auth")]
    Auth { mac: String },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "job.started")]
    JobStarted { job_target_id: String },
    #[serde(rename = "job.output")]
    JobOutput {
        job_target_id: String,
        /// Starts at 1 and increases by one per chunk of a job target.
        seq: u64,
        stream: OutputStream,
        data: String,
    },
    #[serde(rename = "job.result")]
    JobResult(JobResult),
    #[serde(other, rename = "unknown")]
    Unknown,
}

/// Messages sent by the coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "challenge")]
    Challenge { nonce: String, mac: String },
    #[serde(rename = "welcome")]
    Welcome { heartbeat_interval_ms: u64 },
    #[serde(rename = "reject")]
    Reject { reason: String },
    #[serde(rename = "heartbeat_ack")]
    HeartbeatAck,
    #[serde(rename = "job")]
    Job {
        /// Identifies this job on this device; used to detect duplicates.
        job_target_id: String,
        job_id: String,
        kind: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "job.result_ack")]
    JobResultAck { job_target_id: String },
    /// Stop this job target if it is still running, and report it cancelled.
    #[serde(rename = "job.cancel")]
    JobCancel { job_target_id: String },
    #[serde(other, rename = "unknown")]
    Unknown,
}

impl ClientMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("protocol messages always serialise")
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

impl ServerMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("protocol messages always serialise")
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// First message of the handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    /// Generated at install and persisted on the device.
    pub device_id: String,
    pub hostname: String,
    pub os: String,
    pub client_version: String,
    /// Selects one of the coordinator's enrollment keys.
    pub key_id: String,
    pub nonce: String,
    /// The network the device is on, if it knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkInfo>,
}

/// The Wi-Fi network a device is connected through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Succeeded,
    Failed,
}

/// Final report for one job target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub job_target_id: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// One of the [`reason`] constants when `status` is failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Payload of a `script.run` job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptRunPayload {
    pub shell: Shell,
    pub body: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub run_as: RunAs,
}

/// Payload of a `wifi.join` job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiJoinPayload {
    pub ssid: String,
    /// Absent for an open network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Coordinator to use from now on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_url: Option<String>,
}

/// Payload of an `app.launch` job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLaunchPayload {
    pub path: String,
    #[serde(default)]
    pub arguments: Vec<String>,
}

/// Payload of a `power` job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerPayload {
    pub action: PowerAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    Restart,
    Shutdown,
    Sleep,
}

/// Payload of a `setting.apply` job. `setting` is an id of the
/// [`settings`] catalogue; `value` is checked with [`settings::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingApplyPayload {
    pub setting: String,
    pub value: serde_json::Value,
}

fn default_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    PowerShell,
    Cmd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunAs {
    /// The identity the client itself runs as.
    #[default]
    System,
    /// The user logged in to the device's interactive session.
    User,
}

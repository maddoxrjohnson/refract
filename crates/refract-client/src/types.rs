//! What the client reports to the app. These are serialized for the interface, so field
//! names are camelCase.

use std::fmt;

use refract_core::protocol::Event;
use serde::Serialize;

/// Something for the app to show: a change in the helper's status, or an event the
/// helper sent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientEvent {
    /// `{"kind": "status", "state": "running"}`
    Status(AgentStatus),
    /// `{"kind": "event", "event": "repoChanged", "data": ...}`
    Event(Event),
}

/// Whether the helper is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentStatus {
    /// Being started.
    Starting,
    /// Started; requests are sent to it.
    Running,
    /// Exited or failed to start; the next attempt is in `retry_in_ms`. `attempt`
    /// counts failures in a row. `last_error` is the spawn error, or the helper's last
    /// lines of standard error, or its exit status when it wrote none.
    Restarting {
        attempt: u32,
        retry_in_ms: u64,
        last_error: Option<String>,
    },
    /// Shut down; it will not be started again.
    Stopped,
}

/// Why a request got no reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RequestError {
    /// The helper answered with an error.
    Agent { code: i32, message: String },
    /// No answer within the timeout.
    Timeout,
    /// The helper exited, or could not be written to, before it answered.
    Disconnected,
    /// The helper is starting, restarting or stopped. Requests are not queued.
    NotRunning,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Agent { code, message } => {
                write!(f, "the helper answered with error {code}: {message}")
            }
            RequestError::Timeout => f.write_str("the helper did not answer in time"),
            RequestError::Disconnected => f.write_str("the helper exited before answering"),
            RequestError::NotRunning => f.write_str("the helper is not running"),
        }
    }
}

impl std::error::Error for RequestError {}

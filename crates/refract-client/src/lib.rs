//! The Windows app's side of the protocol with `refract-agent`, the WSL helper.
//!
//! [`AgentClient`] owns the helper process. It starts it through a [`Spawn`], sends
//! requests and matches each response to its caller by id, passes the helper's events
//! on, and restarts the helper with a growing delay ([`Backoff`]) when it exits. Nothing
//! here is specific to Windows: the app supplies a spawner that runs `wsl.exe`, and the
//! tests supply one that runs a fake helper.
//!
//! Everything runs on plain threads. While the helper is quiet, every thread is blocked
//! on a read or a condition variable, so an idle client uses no CPU.

mod backoff;
mod client;
mod supervisor;
mod types;

pub use backoff::Backoff;
pub use client::{AgentClient, Spawn};
pub use types::{AgentStatus, ClientEvent, RequestError};

//! Refract's WSL helper. It finds local repos, reads their git status and Claude Code
//! sessions, watches for changes, and talks to the Windows app over stdin and stdout
//! using the messages in [`refract_core::protocol`].

pub mod config;
pub mod git;
pub mod launch;
pub mod procs;
pub mod scan;
pub mod server;
pub mod sessions;
pub mod setup;
pub mod state;
pub mod watch;

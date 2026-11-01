//! Refract's WSL helper. It finds local repos, reads their git status and Claude Code
//! sessions, watches for changes, and talks to the Windows app over stdin and stdout
//! using the messages in [`refract_core::protocol`].

pub mod config;
pub mod git;
pub mod hook;
pub mod launch;
pub mod limits;
pub mod migrate;
pub mod procs;
pub mod refactor;
pub mod repos;
pub mod scan;
pub mod server;
pub mod sessions;
pub mod setup;
pub mod state;
pub mod subagents;
pub mod usage;
pub mod watch;

/// What `refract-agent --version` prints: the name and the crate version, which is
/// kept equal to the app's.
pub const VERSION_LINE: &str = concat!("refract-agent ", env!("CARGO_PKG_VERSION"));

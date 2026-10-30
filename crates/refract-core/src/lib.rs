//! Pure logic shared by the Refract app and its WSL agent: parsing Claude Code
//! transcripts, classifying session state, choosing what the launch picker
//! preselects, scoring repo mass, adding up token usage, reading plan limits,
//! building launch commands, the rules for moving a repo into WSL, the messages
//! between app and agent, and the app's log lines. Nothing here touches the file
//! system or spawns processes.

pub mod classify;
pub mod diff;
pub mod hook;
pub mod language;
pub mod launch;
pub mod limits;
pub mod logline;
pub mod mass;
pub mod migrate;
pub mod preselect;
pub mod protocol;
pub mod sessions;
pub mod transcript;
pub mod usage;

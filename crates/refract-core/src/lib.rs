//! Pure logic shared by the Refract app and its WSL agent: parsing Claude Code
//! transcripts, classifying session state, choosing what the launch picker
//! preselects, scoring repo mass, adding up token usage, building launch commands and
//! the messages between app and agent. Nothing here touches the file system or spawns
//! processes.

pub mod classify;
pub mod diff;
pub mod hook;
pub mod language;
pub mod launch;
pub mod mass;
pub mod preselect;
pub mod protocol;
pub mod sessions;
pub mod transcript;
pub mod usage;

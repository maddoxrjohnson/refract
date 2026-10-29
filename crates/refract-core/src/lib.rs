//! Pure logic shared by the Refract app and its WSL agent: parsing Claude Code
//! transcripts, classifying session state, choosing what the launch picker
//! preselects, scoring repo mass and the messages between app and agent. Nothing here
//! touches the file system or spawns processes.

pub mod classify;
pub mod language;
pub mod mass;
pub mod preselect;
pub mod protocol;
pub mod sessions;
pub mod transcript;

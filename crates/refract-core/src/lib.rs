//! Pure logic shared by the Refract app and its WSL agent: parsing Claude Code
//! transcripts, classifying session state, choosing what the launch picker
//! preselects, scoring repo mass, adding up token usage, reading plan limits,
//! building launch commands, the rules for moving a repo into WSL, the prompts and
//! result file of a refactor, the status of a linked worktree, the messages between app and agent, and the app's log
//! lines; and for Codex as the overflow agent, its rollout files, whether a repo is ready
//! for it, and the brief a picking-up agent gets. Nothing here touches the file
//! system or spawns processes.

pub mod agent;
pub mod brief;
pub mod classify;
pub mod codex_ready;
pub mod codex_transcript;
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
pub mod refactor;
pub mod sessions;
pub mod transcript;
pub mod usage;
pub mod worktree;

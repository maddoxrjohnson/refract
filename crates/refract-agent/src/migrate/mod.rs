//! Moving a repo from a Windows drive into WSL ("Move to WSL",
//! `.plans/wsl-migration-spec.md`): the helper's half. The rules and comparisons are in
//! [`refract_core::migrate`]; this does the I/O.
//!
//! The app writes the source snapshot (S0 and G0, see [`source`] for the exact format)
//! and sends `migrate {path, dest, snapshot}`. A worker thread ([`worker`]) then:
//!
//! 1. **Preflight**, Linux side (spec §4.1): the source is a repo on a Windows drive,
//!    not a linked worktree and without any, with no merge, rebase, cherry-pick,
//!    revert or bisect running and no symlink leading out of it; Git LFS is installed
//!    if the repo uses it; the destination is on the Linux file system, does not exist
//!    and has room ([`refract_core::migrate::preflight::space_needed`]). A refusal
//!    answers `migrate` with `MIGRATE_FAILED`; otherwise it is answered with the id.
//! 2. **Copy** into `<dest parent>/.refract-migrate-<name>-<id>`, everything but the
//!    skipped build folders ([`tree::copy_tree`]). The source is statted just before;
//!    paths whose size or time differ from S0 changed after the snapshot and are read
//!    again ([`source::refresh`]). drvfs cannot tell the Windows executable bit, so
//!    files get mode 644, except hooks (files right in a git folder's `hooks`, not
//!    ending in `.sample`) and files outside git folders starting with `#!`, which get
//!    755.
//! 3. **Normalise** each repo ([`gitx::normalise`]). Tracked files take their index
//!    mode; skip-worktree, assume-unchanged and intent-to-add entries keep their flags.
//! 4. **Verify**: the copy's manifest and git state against the snapshot, checks 1 to
//!    3; then the source is statted again, and what changed is copied, normalised and
//!    checked again, up to three rounds (check 4). No `claude` process may run in the
//!    source at the end.
//! 5. **Place**: only when checks 1 to 4 pass, the temporary folder is renamed to the
//!    destination (`RENAME_NOREPLACE`). Then the install ([`install`]) and the history
//!    ([`history`]) run, the report is saved ([`store`]) and sent as `migrateReady`.
//!    The copy is statted around the install; what it created, changed or removed
//!    outside the build folders is listed in the report (`installChanges`), and later
//!    checks of the placed copy allow those paths' differences until a re-sync copies
//!    the original's version over them.
//!    A failed report deletes the temporary folder at once.
//!
//! After the report: `migrateRecheck` stats the source against the last check's stat
//! and the placed copy against its own (taken after the install), `migrateBeginDelete`
//! does the same and, when it is safe, marks the migration deleting, `migrateResync`
//! copies what changed into the placed copy and checks again, and `migrateFinish` ends
//! it. Paths that cannot be read are never taken for removed. A placed migration is
//! kept in a state file until then, so after a restart its worker resumes
//! ([`worker::resume`]) and sends the report again.
//! At startup, temporary folders left by a helper that stopped mid-copy are deleted
//! ([`store::leftovers`]).

pub mod fsx;
pub mod gitx;
pub mod history;
pub mod install;
pub mod source;
pub mod store;
pub mod tree;
pub mod worker;

use std::path::PathBuf;

pub use worker::{resume, start, Command, Context, Handle, Phase, Sink, Update};

/// Deletes `leftovers` (see [`store::leftovers`]) on a thread of its own, since a big
/// copy takes a while to delete.
pub fn delete_leftovers(leftovers: Vec<PathBuf>) {
    if leftovers.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("migrate-cleanup".into())
        .spawn(move || {
            for path in leftovers {
                eprintln!(
                    "refract-agent: deleting the leftover copy {}",
                    path.display()
                );
                let _ = fsx::remove_any(&path);
            }
        });
    if let Err(e) = spawned {
        eprintln!("refract-agent: cannot start the cleanup thread: {e}");
    }
}

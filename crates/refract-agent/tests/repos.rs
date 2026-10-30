//! Cloning and finding work trees, against local repos.

mod common;

use std::path::Path;
use std::sync::Once;
use std::time::{Duration, Instant};

use refract_agent::repos::{clone_into, origin_url, work_tree_root, CLONE_TIMEOUT};

use common::*;

#[test]
fn clones_once_and_then_returns_the_existing_clone() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _) = remote(tmp.path(), "kelp-cli");
    let url = bare.to_str().unwrap();
    let dest = tmp.path().join("home/projects/kelp-cli");

    assert_eq!(clone_into(url, &dest, CLONE_TIMEOUT), Ok(dest.clone()));
    assert!(dest.join("src/main.rs").is_file());
    assert_eq!(origin_url(&dest).as_deref(), Some(url));
    assert_eq!(partials(dest.parent().unwrap()), Vec::<String>::new());

    // Already a clone of the same URL, written a little differently.
    let again = url.strip_suffix(".git").unwrap();
    assert_eq!(clone_into(again, &dest, CLONE_TIMEOUT), Ok(dest.clone()));

    // A clone of something else, or any other folder, is in the way.
    let (other, _) = remote(tmp.path(), "reef-maps");
    let err = clone_into(other.to_str().unwrap(), &dest, CLONE_TIMEOUT).unwrap_err();
    assert!(err.ends_with("already exists"), "{err}");
    let plain = tmp.path().join("home/projects/aurora-notes");
    std::fs::create_dir_all(&plain).unwrap();
    let err = clone_into(url, &plain, CLONE_TIMEOUT).unwrap_err();
    assert!(err.ends_with("already exists"), "{err}");
}

#[test]
fn a_failed_clone_leaves_nothing_behind() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("remotes/missing.git");
    let dest = tmp.path().join("projects/missing");
    let err = clone_into(missing.to_str().unwrap(), &dest, CLONE_TIMEOUT).unwrap_err();
    assert!(err.starts_with("git clone"), "{err}");
    assert!(!dest.exists());
}

#[test]
fn a_leftover_partial_clone_is_not_reused() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _) = remote(tmp.path(), "kelp-cli");
    let url = bare.to_str().unwrap();
    // What a clone killed mid-fetch leaves: the origin is set, but nothing is checked out.
    let dest = tmp.path().join("projects/kelp-cli");
    std::fs::create_dir_all(&dest).unwrap();
    git(&dest, &["init", "--quiet"]);
    git(&dest, &["remote", "add", "origin", url]);
    assert_eq!(origin_url(&dest).as_deref(), Some(url));
    let err = clone_into(url, &dest, CLONE_TIMEOUT).unwrap_err();
    assert!(err.ends_with("already exists"), "{err}");
}

#[test]
fn a_clone_that_runs_too_long_is_stopped_and_removed() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let (bare, _) = remote(tmp.path(), "tidepool-api");
    let dest = tmp.path().join("projects/tidepool-api");
    let err = clone_into(bare.to_str().unwrap(), &dest, Duration::ZERO).unwrap_err();
    assert!(err.contains("took longer than 0 seconds"), "{err}");
    assert!(!dest.exists());
    assert_eq!(partials(dest.parent().unwrap()), Vec::<String>::new());
}

#[test]
fn a_hung_clone_and_its_helpers_are_killed() {
    hermetic_git();
    allow_ext_transport();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("projects/reef-maps");
    // A transport that never answers and keeps git's stderr open until it is killed.
    let url = "ext::sh -c sleep% 60";
    let started = Instant::now();
    let err = clone_into(url, &dest, Duration::from_millis(300)).unwrap_err();
    assert!(err.contains("took longer than 0 seconds"), "{err}");
    // Returning at all means the stderr reader finished, so the sleeping helper is gone.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert!(!dest.exists());
    assert_eq!(partials(dest.parent().unwrap()), Vec::<String>::new());
}

/// Lets this process's git use the `ext::` transport, which runs a command as a remote.
fn allow_ext_transport() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "protocol.ext.allow");
        std::env::set_var("GIT_CONFIG_VALUE_0", "always");
    });
}

/// Names of the half-done clones left in `dir`.
fn partials(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.contains(".partial-"))
        .collect()
}

#[test]
fn work_tree_root_finds_the_top_of_the_tree() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("work/aurora-notes");
    init_repo(&repo);
    let top = std::fs::canonicalize(&repo).unwrap();
    assert_eq!(work_tree_root(&repo), Ok(top.clone()));
    assert_eq!(work_tree_root(&repo.join("src")), Ok(top));

    let plain = tmp.path().join("work/notes");
    std::fs::create_dir_all(&plain).unwrap();
    let err = work_tree_root(&plain).unwrap_err();
    assert!(err.ends_with("is not in a git working tree"), "{err}");
    assert!(work_tree_root(&tmp.path().join("work/missing")).is_err());
    assert!(work_tree_root(&repo.join(".git")).is_err());
}

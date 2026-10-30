//! What the migration keeps on disk, under `$XDG_DATA_HOME/refract/migrations`
//! (default `~/.local/share/refract/migrations`):
//!
//! - `<timestamp>-<name>.json`: each report, and `<timestamp>-<name>.install.log` next
//!   to it.
//! - `state/<id>.json`: a migration placed at its destination and not yet finished
//!   ([`Record`]), so its report comes back after a restart (spec §4.5).
//! - `state/<id>.temp.json`: the temporary folder a running migration copies into, and
//!   the helper's pid ([`TempMarker`]), so a later helper knows the folder is its own
//!   to delete.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use refract_core::migrate::manifest::StatEntry;
use refract_core::migrate::report::Report;
use refract_core::migrate::SourceSnapshot;
use serde::{Deserialize, Serialize};

use super::fsx;

/// Version of the [`Record`] format.
const RECORD_VERSION: u32 = 1;

/// The prefix of a temporary copy's folder name: `.refract-migrate-<name>-<id>`.
pub const TEMP_PREFIX: &str = ".refract-migrate-";

/// Where reports and state live.
pub fn dir() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|v| v.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("refract/migrations")
}

fn state_dir() -> PathBuf {
    dir().join("state")
}

/// A migration placed at its destination and waiting for `migrateFinish`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub version: u32,
    pub id: String,
    pub source: String,
    pub dest: String,
    /// The report as last sent. Without its history check, while history is moving.
    pub report: Report,
    pub report_path: PathBuf,
    /// The source as the last check saw it.
    pub snapshot: SourceSnapshot,
    /// The source's stat at the last check, for `migrateRecheck`.
    pub final_stats: Vec<StatEntry>,
    /// Transcripts copied: (original, copy).
    #[serde(default)]
    pub history: Vec<(PathBuf, PathBuf)>,
}

impl Record {
    pub fn new(
        id: &str,
        report: Report,
        report_path: PathBuf,
        snapshot: SourceSnapshot,
        final_stats: Vec<StatEntry>,
    ) -> Record {
        Record {
            version: RECORD_VERSION,
            id: id.to_owned(),
            source: report.source.clone(),
            dest: report.dest.clone(),
            report,
            report_path,
            snapshot,
            final_stats,
            history: Vec::new(),
        }
    }
}

fn record_path(id: &str) -> PathBuf {
    state_dir().join(format!("{id}.json"))
}

pub fn save_record(record: &Record) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    fsx::write_atomic(&record_path(&record.id), &bytes)
        .map_err(|e| format!("cannot save the migration's state: {e}"))
}

pub fn remove_record(id: &str) {
    let _ = fs::remove_file(record_path(id));
}

/// Every saved [`Record`]; unreadable ones are reported on stderr and skipped.
pub fn load_records() -> Vec<Record> {
    let Ok(entries) = fs::read_dir(state_dir()) else {
        return Vec::new();
    };
    let mut records: Vec<Record> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && !p.to_string_lossy().ends_with(".temp.json")
        })
        .filter_map(|p| {
            let parsed = fs::read(&p)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice::<Record>(&b).map_err(|e| e.to_string()));
            match parsed {
                Ok(record) if record.version == RECORD_VERSION => Some(record),
                Ok(_) => None,
                Err(e) => {
                    eprintln!("refract-agent: ignoring {}: {e}", p.display());
                    None
                }
            }
        })
        .collect();
    records.sort_by_key(|r| r.report.started_at);
    records
}

/// The report and install log paths of a migration of the folder `name` started at
/// `at`.
pub fn report_paths(at: DateTime<Utc>, name: &str) -> (PathBuf, PathBuf) {
    let stem = format!("{}-{name}", at.format("%Y%m%d-%H%M%S"));
    let dir = dir();
    (
        dir.join(format!("{stem}.json")),
        dir.join(format!("{stem}.install.log")),
    )
}

pub fn save_report(path: &Path, report: &Report) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    fsx::write_atomic(path, &bytes).map_err(|e| format!("cannot save the report: {e}"))
}

/// A temporary folder a running migration copies into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TempMarker {
    pub path: PathBuf,
    pub pid: u32,
}

fn temp_marker_path(id: &str) -> PathBuf {
    state_dir().join(format!("{id}.temp.json"))
}

/// Notes that this helper is about to create `path` for migration `id`.
pub fn mark_temp(id: &str, path: &Path) -> Result<(), String> {
    let marker = TempMarker {
        path: path.to_owned(),
        pid: std::process::id(),
    };
    let bytes = serde_json::to_vec(&marker).map_err(|e| e.to_string())?;
    fsx::write_atomic(&temp_marker_path(id), &bytes)
        .map_err(|e| format!("cannot save the migration's state: {e}"))
}

pub fn unmark_temp(id: &str) {
    let _ = fs::remove_file(temp_marker_path(id));
}

/// True when `name` is a temporary copy's folder name: [`TEMP_PREFIX`], a name, `-`
/// and a UUID.
pub fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
        return false;
    };
    rest.len() > 37
        && rest.is_char_boundary(rest.len() - 37)
        && rest[rest.len() - 37..].starts_with('-')
        && uuid::Uuid::try_parse(&rest[rest.len() - 36..]).is_ok()
}

fn alive(pid: u32) -> bool {
    pid != std::process::id() && Path::new(&format!("/proc/{pid}")).exists()
}

/// Leftover temporary folders to delete at startup (spec §4.5): those a marker names
/// whose helper is gone, and folders with a temporary copy's name directly in one of
/// `roots` that no live helper's marker names. Markers of gone helpers are removed.
pub fn leftovers(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut live: Vec<PathBuf> = Vec::new();
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(state_dir()) {
        for path in entries.flatten().map(|e| e.path()) {
            if !path.to_string_lossy().ends_with(".temp.json") {
                continue;
            }
            let Some(marker) = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<TempMarker>(&b).ok())
            else {
                continue;
            };
            if alive(marker.pid) {
                live.push(marker.path);
                continue;
            }
            let named = marker
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_temp_name);
            if named {
                found.push(marker.path);
            }
            let _ = fs::remove_file(&path);
        }
    }
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && entry.file_name().to_str().is_some_and(is_temp_name) {
                found.push(entry.path());
            }
        }
    }
    found.retain(|p| !live.contains(p));
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_need_the_prefix_and_a_uuid() {
        let id = "6f1c0000-0000-4000-8000-000000000001";
        assert!(is_temp_name(&format!(".refract-migrate-aurora-notes-{id}")));
        assert!(is_temp_name(&format!(".refract-migrate-a-{id}")));
        assert!(!is_temp_name(&format!(".refract-migrate-{id}")));
        assert!(!is_temp_name(".refract-migrate-aurora-notes-1234"));
        assert!(!is_temp_name(&format!("refract-migrate-aurora-notes-{id}")));
        assert!(!is_temp_name(&format!(".refract-migrate-é-{}", &id[..35])));
    }
}

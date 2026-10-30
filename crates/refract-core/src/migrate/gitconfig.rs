//! The local git config changes that make a copied repo a Linux checkout (spec §3.3
//! step 4), and the check that the copy's config differs by those alone (spec §5
//! check 2).
//!
//! Input is the output of `git config --local -l`: one `key=value` per line, with
//! section and variable names lowercased by git and subsection names as written
//! (`remote.origin.url`). A key with no value (a bare boolean, meaning true) prints
//! with no `=`.
//!
//! Removed: `core.autocrlf` (any value), `core.filemode=false`, `core.symlinks=false`
//! and `core.ignorecase=true`. Then `core.filemode=true` is set. The helper applies
//! [`ConfigChanges`] with `git config --local --unset-all <key>` for each of `unset`
//! and `git config --local --replace-all <key> <value>` for each of `set`, so keys
//! given more than once are handled too.

use serde::{Deserialize, Serialize};

/// One `key=value` line; `value` is `None` for a bare key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
}

impl ConfigEntry {
    /// The key with its section and variable name lowercased and its subsection (the
    /// middle part, if any) kept.
    fn normal_key(&self) -> String {
        normal_key(&self.key)
    }
}

fn normal_key(key: &str) -> String {
    match (key.split_once('.'), key.rsplit_once('.')) {
        (Some((section, _)), Some((middle_end, name))) if middle_end.len() > section.len() => {
            let sub = &middle_end[section.len()..];
            format!("{}{sub}.{}", section.to_lowercase(), name.to_lowercase())
        }
        _ => key.to_lowercase(),
    }
}

/// Parses `git config --local -l` output. Empty lines are skipped. Values may hold
/// `=`; only the first one splits.
pub fn parse_config(text: &str) -> Vec<ConfigEntry> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once('=') {
            Some((key, value)) => ConfigEntry {
                key: key.into(),
                value: Some(value.into()),
            },
            None => ConfigEntry {
                key: line.into(),
                value: None,
            },
        })
        .collect()
}

/// Reads a git boolean: a bare key, `true`/`yes`/`on`/`1` and `false`/`no`/`off`/`0`/
/// empty, ignoring case. `None` for anything else.
fn git_bool(value: Option<&str>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// What to change in the copy's local config.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfigChanges {
    /// Keys to remove, every value of each.
    pub unset: Vec<String>,
    /// Keys to set, replacing every value.
    pub set: Vec<(String, String)>,
}

/// The changes for a config, as the module docs list them. `set` is always
/// `core.filemode=true`.
pub fn config_changes(entries: &[ConfigEntry]) -> ConfigChanges {
    let mut unset: Vec<String> = Vec::new();
    for entry in entries {
        let value = entry.value.as_deref();
        let remove = match entry.normal_key().as_str() {
            "core.autocrlf" => true,
            "core.symlinks" => git_bool(value) == Some(false),
            "core.ignorecase" => git_bool(value) == Some(true),
            _ => false,
        };
        let key = entry.normal_key();
        if remove && !unset.contains(&key) {
            unset.push(key);
        }
    }
    ConfigChanges {
        unset,
        set: vec![("core.filemode".into(), "true".into())],
    }
}

/// Applies `changes` to `entries` the way git would, up to order.
fn apply(entries: &[ConfigEntry], changes: &ConfigChanges) -> Vec<ConfigEntry> {
    let mut out: Vec<ConfigEntry> = entries
        .iter()
        .filter(|e| {
            let key = e.normal_key();
            !changes.unset.contains(&key) && !changes.set.iter().any(|(k, _)| *k == key)
        })
        .cloned()
        .collect();
    for (key, value) in &changes.set {
        out.push(ConfigEntry {
            key: key.clone(),
            value: Some(value.clone()),
        });
    }
    out
}

/// Checks that `after` is `before` with exactly the [`config_changes`] of `before`
/// applied, ignoring order. Returns each unexpected difference as a sentence.
pub fn check_config(before: &[ConfigEntry], after: &[ConfigEntry]) -> Result<(), Vec<String>> {
    let normal = |entries: Vec<ConfigEntry>| {
        let mut entries: Vec<ConfigEntry> = entries
            .into_iter()
            .map(|e| ConfigEntry {
                key: e.normal_key(),
                value: e.value,
            })
            .collect();
        entries.sort();
        entries
    };
    let mut expected = normal(apply(before, &config_changes(before)));
    let actual = normal(after.to_vec());
    let mut problems = Vec::new();
    for entry in actual {
        match expected.iter().position(|e| *e == entry) {
            Some(i) => {
                expected.remove(i);
            }
            None => problems.push(format!("Unexpected config entry {}.", show(&entry))),
        }
    }
    for entry in expected {
        problems.push(format!("Config entry {} is missing.", show(&entry)));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn show(entry: &ConfigEntry) -> String {
    match &entry.value {
        Some(value) => format!("{}={value}", entry.key),
        None => entry.key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: &str = "core.repositoryformatversion=0\n\
        core.filemode=false\n\
        core.bare=false\n\
        core.logallrefupdates=true\n\
        core.symlinks=false\n\
        core.ignorecase=true\n\
        core.autocrlf=true\n\
        remote.origin.url=https://example.com/dev/aurora-notes.git\n\
        remote.origin.fetch=+refs/heads/*:refs/remotes/origin/*\n\
        branch.main.remote=origin\n\
        branch.main.merge=refs/heads/main\n";

    const LINUX: &str = "core.repositoryformatversion=0\n\
        core.bare=false\n\
        core.logallrefupdates=true\n\
        remote.origin.url=https://example.com/dev/aurora-notes.git\n\
        remote.origin.fetch=+refs/heads/*:refs/remotes/origin/*\n\
        branch.main.remote=origin\n\
        branch.main.merge=refs/heads/main\n\
        core.filemode=true\n";

    #[test]
    fn windows_keys_are_removed_and_filemode_set() {
        let changes = config_changes(&parse_config(WINDOWS));
        assert_eq!(
            changes.unset,
            vec!["core.symlinks", "core.ignorecase", "core.autocrlf"]
        );
        assert_eq!(
            changes.set,
            vec![("core.filemode".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn settings_that_suit_linux_are_kept() {
        let entries =
            parse_config("core.symlinks=true\ncore.ignorecase=false\ncore.autocrlf=input\n");
        assert_eq!(config_changes(&entries).unset, vec!["core.autocrlf"]);
        // A bare key is true; values are read as git reads them.
        let entries = parse_config("core.ignorecase\ncore.symlinks=Off\n");
        assert_eq!(
            config_changes(&entries).unset,
            vec!["core.ignorecase", "core.symlinks"]
        );
    }

    #[test]
    fn values_may_hold_equals_signs() {
        let entries = parse_config("alias.lg=log --format=%h\ncore.bare\n");
        assert_eq!(entries[0].value.as_deref(), Some("log --format=%h"));
        assert_eq!(entries[1].value, None);
    }

    #[test]
    fn the_expected_changes_pass_in_any_order() {
        assert_eq!(
            check_config(&parse_config(WINDOWS), &parse_config(LINUX)),
            Ok(())
        );
        let mut shuffled = parse_config(LINUX);
        shuffled.reverse();
        assert_eq!(check_config(&parse_config(WINDOWS), &shuffled), Ok(()));
    }

    #[test]
    fn an_unexpected_key_change_fails() {
        let changed = LINUX.replace("aurora-notes.git", "kelp-cli.git");
        let problems = check_config(&parse_config(WINDOWS), &parse_config(&changed)).unwrap_err();
        assert_eq!(
            problems,
            vec![
                "Unexpected config entry remote.origin.url=https://example.com/dev/kelp-cli.git.",
                "Config entry remote.origin.url=https://example.com/dev/aurora-notes.git is missing.",
            ]
        );
        // Leaving a Windows key in place fails too.
        let kept = format!("{LINUX}core.autocrlf=true\n");
        assert!(check_config(&parse_config(WINDOWS), &parse_config(&kept)).is_err());
        // So does forgetting filemode.
        let no_mode = LINUX.replace("core.filemode=true\n", "");
        assert!(check_config(&parse_config(WINDOWS), &parse_config(&no_mode)).is_err());
    }

    #[test]
    fn subsection_case_is_kept_and_names_ignore_case() {
        assert_eq!(normal_key("Remote.Origin.URL"), "remote.Origin.url");
        assert_eq!(normal_key("Core.AutoCRLF"), "core.autocrlf");
        assert_eq!(normal_key("branch.feat.x.remote"), "branch.feat.x.remote");
        let entries = parse_config("Core.AutoCRLF=true\n");
        assert_eq!(config_changes(&entries).unset, vec!["core.autocrlf"]);
    }
}

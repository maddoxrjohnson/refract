//! The Windows side of a launch (see `docs/launch.md`): the arguments for `code`, and
//! the bare layout added to the Refract profile's settings. [`setup`] has the
//! first-run steps and [`windows`] closes the window the first run opens. Nothing
//! here depends on the operating system, so it is tested on Linux and used by the
//! Windows app.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::Value;

pub mod setup;
pub mod windows;

/// The VS Code profile every Refract window opens with, unless the caller names another.
pub const PROFILE: &str = "Refract";

/// Settings the Refract profile gets when they are not already set.
pub const BARE_LAYOUT: &[(&str, Setting)] = &[
    ("workbench.activityBar.location", Setting::Str("hidden")),
    ("workbench.statusBar.visible", Setting::Bool(false)),
    ("workbench.startupEditor", Setting::Str("none")),
    ("workbench.layoutControl.enabled", Setting::Bool(false)),
    (
        "workbench.secondarySideBar.defaultVisibility",
        Setting::Str("hidden"),
    ),
    ("workbench.tips.enabled", Setting::Bool(false)),
    ("window.commandCenter", Setting::Bool(false)),
    ("chat.commandCenter.enabled", Setting::Bool(false)),
    (
        "terminal.integrated.defaultLocation",
        Setting::Str("editor"),
    ),
];

/// A value in [`BARE_LAYOUT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Str(&'static str),
    Bool(bool),
}

impl From<Setting> for CstInputValue {
    fn from(setting: Setting) -> CstInputValue {
        match setting {
            Setting::Str(s) => CstInputValue::String(s.into()),
            Setting::Bool(b) => CstInputValue::Bool(b),
        }
    }
}

/// Arguments for `code` that open `folder_uri` in the profile called `profile`.
pub fn vscode_args(profile: &str, folder_uri: &str) -> Vec<String> {
    vec![
        "--profile".into(),
        profile.into(),
        "--folder-uri".into(),
        folder_uri.into(),
    ]
}

/// The directory name of the profile called `name`, from the text of VS Code's
/// `globalStorage/storage.json`. `None` until VS Code has created the profile.
pub fn find_profile_dir(storage_json: &str, name: &str) -> Option<String> {
    let storage: Value = serde_json::from_str(storage_json).ok()?;
    storage
        .get("userDataProfiles")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))?
        .get("location")?
        .as_str()
        .map(str::to_owned)
}

/// Adds every [`BARE_LAYOUT`] setting missing from `settings_text`, the text of a
/// `settings.json` (JSON with comments and trailing commas), and returns the new text.
/// Values already set are never changed, and comments and formatting are kept.
/// Returns `None` when nothing is missing. A missing or blank file gets every setting.
pub fn merge_bare_layout(settings_text: Option<&str>) -> io::Result<Option<String>> {
    let text = settings_text
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("{}\n");
    let root = CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|e| invalid(format!("settings.json does not parse: {e}")))?;
    let object = match root.value() {
        None => root.object_value_or_set(),
        Some(value) => value
            .as_object()
            .ok_or_else(|| invalid("settings.json is not an object".into()))?,
    };
    let mut added = false;
    for (key, value) in BARE_LAYOUT {
        if object.get(key).is_none() {
            object.append(key, (*value).into());
            added = true;
        }
    }
    Ok(added.then(|| root.to_string()))
}

/// `globalStorage/storage.json` in VS Code's user directory (`%APPDATA%\Code\User`).
pub fn storage_json(user_dir: &Path) -> PathBuf {
    user_dir.join("globalStorage/storage.json")
}

/// The directory name of the profile called `profile`, read from the user
/// directory's `storage.json`. `None` while the file or the profile does not exist.
pub fn read_profile_dir(user_dir: &Path, profile: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(storage_json(user_dir)) {
        Ok(text) => Ok(find_profile_dir(&text, profile)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Finds the profile called `profile` in VS Code's user directory and adds the bare
/// layout to its `settings.json`, writing through a temporary file. Returns false, and
/// changes nothing, while the profile does not exist yet.
pub fn ensure_profile_settings(user_dir: &Path, profile: &str) -> io::Result<bool> {
    let Some(location) = read_profile_dir(user_dir, profile)? else {
        return Ok(false);
    };
    let dir = user_dir.join("profiles").join(location);
    let path = dir.join("settings.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(merged) = merge_bare_layout(text.as_deref())? {
        fs::create_dir_all(&dir)?;
        write_replacing(&path, &merged)?;
    }
    Ok(true)
}

/// Writes `text` to a uniquely named temporary file beside `path` and renames it over
/// `path`, so readers never see a partial file. A symlink is written through: the file
/// it points to is replaced and the link is kept. An existing file keeps its
/// permissions.
fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    let target = resolve_symlinks(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".refract-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    match fs::metadata(&target) {
        Ok(meta) => tmp.as_file().set_permissions(meta.permissions())?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tmp.as_file().sync_all()?;
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

/// The path `path` finally points to, following symlinks one at a time so that a
/// link to a file that does not exist yet still resolves.
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut target = path.to_owned();
    for _ in 0..40 {
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&target)?;
                target = match target.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            _ => return Ok(target),
        }
    }
    Err(invalid(format!("too many symlinks at {}", path.display())))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STORAGE: &str = r#"{
        "telemetry.machineId": "0000",
        "userDataProfiles": [
            {"location": "-5b2c1a7e", "name": "Work", "icon": "briefcase"},
            {"location": "1f3d9c40", "name": "Refract"}
        ],
        "profileAssociations": {"workspaces": {}, "emptyWindows": {}}
    }"#;

    fn parsed(text: &str) -> Value {
        let root = CstRootNode::parse(text, &ParseOptions::default()).unwrap();
        root.to_serde_value().unwrap()
    }

    #[test]
    fn code_args() {
        assert_eq!(
            vscode_args(
                PROFILE,
                "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api"
            ),
            [
                "--profile",
                "Refract",
                "--folder-uri",
                "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api"
            ]
        );
    }

    #[test]
    fn profile_dir_is_found_by_name() {
        assert_eq!(
            find_profile_dir(STORAGE, "Refract").as_deref(),
            Some("1f3d9c40")
        );
        assert_eq!(
            find_profile_dir(STORAGE, "Work").as_deref(),
            Some("-5b2c1a7e")
        );
        assert_eq!(find_profile_dir(STORAGE, "Missing"), None);
        assert_eq!(find_profile_dir(r#"{"theme": "dark"}"#, "Refract"), None);
        assert_eq!(find_profile_dir("not json", "Refract"), None);
    }

    #[test]
    fn empty_settings_get_every_key() {
        for text in [
            None,
            Some(""),
            Some("  \n"),
            Some("{}"),
            Some("// nothing yet\n"),
        ] {
            let merged = merge_bare_layout(text).unwrap().unwrap();
            let value: Value = parsed(&merged);
            let object = value.as_object().unwrap();
            assert_eq!(object.len(), BARE_LAYOUT.len(), "{text:?}");
            assert_eq!(object["workbench.activityBar.location"], "hidden");
            assert_eq!(object["workbench.statusBar.visible"], false);
            assert_eq!(object["terminal.integrated.defaultLocation"], "editor");
        }
        let merged = merge_bare_layout(None).unwrap().unwrap();
        let strict: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(strict.as_object().unwrap().len(), BARE_LAYOUT.len());
        assert!(merged.contains("\n  \"workbench.startupEditor\": \"none\""));
    }

    #[test]
    fn existing_values_and_comments_are_kept() {
        let text = r#"{
    // Keep the status bar: I like it.
    "workbench.statusBar.visible": true,
    "editor.fontSize": 15, /* trailing comment */
    "workbench.startupEditor": "welcomePage",
}
"#;
        let merged = merge_bare_layout(Some(text)).unwrap().unwrap();
        assert!(merged.contains("// Keep the status bar: I like it."));
        assert!(merged.contains("/* trailing comment */"));
        let value = parsed(&merged);
        assert_eq!(value["workbench.statusBar.visible"], true);
        assert_eq!(value["workbench.startupEditor"], "welcomePage");
        assert_eq!(value["editor.fontSize"], 15);
        assert_eq!(value["workbench.activityBar.location"], "hidden");
        assert_eq!(value.as_object().unwrap().len(), BARE_LAYOUT.len() + 1);

        assert_eq!(merge_bare_layout(Some(&merged)).unwrap(), None);
    }

    #[test]
    fn broken_settings_are_errors() {
        assert!(merge_bare_layout(Some("{\"a\": ")).is_err());
        assert!(merge_bare_layout(Some("[1, 2]")).is_err());
    }

    #[test]
    fn profile_settings_are_merged_once_the_profile_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path();
        assert!(!ensure_profile_settings(user, PROFILE).unwrap());

        fs::create_dir_all(user.join("globalStorage")).unwrap();
        fs::write(
            user.join("globalStorage/storage.json"),
            r#"{"userDataProfiles": []}"#,
        )
        .unwrap();
        assert!(!ensure_profile_settings(user, PROFILE).unwrap());

        fs::write(user.join("globalStorage/storage.json"), STORAGE).unwrap();
        let settings = user.join("profiles/1f3d9c40/settings.json");
        assert!(ensure_profile_settings(user, PROFILE).unwrap());
        let first = fs::read_to_string(&settings).unwrap();
        assert_eq!(parsed(&first)["window.commandCenter"], false);

        fs::write(
            &settings,
            "{\n  // mine\n  \"window.commandCenter\": true\n}\n",
        )
        .unwrap();
        assert!(ensure_profile_settings(user, PROFILE).unwrap());
        let second = fs::read_to_string(&settings).unwrap();
        assert!(second.contains("// mine"));
        assert_eq!(parsed(&second)["window.commandCenter"], true);
        assert_eq!(
            parsed(&second).as_object().unwrap().len(),
            BARE_LAYOUT.len()
        );
        let names: Vec<_> = fs::read_dir(settings.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);
    }

    #[test]
    fn profiles_are_found_by_the_given_name() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path();
        assert_eq!(read_profile_dir(user, "Work").unwrap(), None);
        fs::create_dir_all(user.join("globalStorage")).unwrap();
        fs::write(storage_json(user), STORAGE).unwrap();
        assert_eq!(
            read_profile_dir(user, "Work").unwrap().as_deref(),
            Some("-5b2c1a7e")
        );
        assert!(ensure_profile_settings(user, "Work").unwrap());
        assert!(user.join("profiles/-5b2c1a7e/settings.json").is_file());
        assert!(!user.join("profiles/1f3d9c40").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_writes_through_symlinks_and_keeps_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let file = real.join("settings.json");
        fs::write(&file, "{}").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        let link = tmp.path().join("settings.json");
        symlink("real/settings.json", &link).unwrap();

        write_replacing(&link, "{\"a\": 1}").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"a\": 1}");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let names: Vec<_> = fs::read_dir(&real)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.json"]);

        let dangling = tmp.path().join("new.json");
        symlink("real/new.json", &dangling).unwrap();
        write_replacing(&dangling, "[]").unwrap();
        assert_eq!(fs::read_to_string(real.join("new.json")).unwrap(), "[]");
    }
}

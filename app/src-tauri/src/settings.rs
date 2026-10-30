//! App settings, kept as JSON in the app's config directory
//! (`%APPDATA%\<identifier>\settings.json`). Tokens are never in here; see `creds`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::open::check_url;
use crate::remote::gitlab;

const FILE: &str = "settings.json";
/// Longest project key kept in `hidden`; real ones are a path or a remote's name.
const MAX_KEY_LEN: usize = 1024;

/// Reads and writes of the file happen one at a time.
static FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The GitLab instance, such as `https://gitlab.example.com`; `None` turns
    /// GitLab off.
    pub gitlab_url: Option<String>,
    pub show_forks: bool,
    pub show_archived: bool,
    /// Project keys the user hid.
    pub hidden: Vec<String>,
}

/// Why reading or saving settings failed, sent as `{"kind": "invalid", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsError {
    pub kind: SettingsErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingsErrorKind {
    /// A value was refused; nothing was saved.
    Invalid,
    /// The file could not be written.
    Io,
    /// The task itself failed.
    Internal,
}

impl SettingsError {
    fn new(kind: SettingsErrorKind, message: impl Into<String>) -> SettingsError {
        SettingsError {
            kind,
            message: message.into(),
        }
    }
}

/// A GitLab address as stored: `https://` with a host, no user name or password, no
/// query or fragment, and no trailing `/`. Blank means none.
pub fn normalize_gitlab_url(url: Option<&str>) -> Result<Option<String>, SettingsError> {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let invalid = |message: String| SettingsError::new(SettingsErrorKind::Invalid, message);
    // Checked first, and without repeating the address, which may hold a token.
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(invalid(
            "The GitLab address cannot have a user name or token in it; put the token in the token field."
                .into(),
        ));
    }
    let url = check_url(url).map_err(|err| invalid(err.message))?;
    if url.contains(['?', '#']) {
        return Err(invalid(format!(
            "The GitLab address cannot have a query or fragment: {url}"
        )));
    }
    let mut url = url.trim_end_matches('/').to_owned();
    // Store the scheme in lower case, as the rest of the app compares it.
    url.replace_range(..8, "https://");
    if gitlab::host(&url).is_none() {
        return Err(invalid(format!("The GitLab address has no host: {url}")));
    }
    Ok(Some(url))
}

/// `settings` as they are saved: the GitLab address normalized, hidden keys trimmed,
/// with blanks, overlong ones and repeats dropped.
pub fn normalize(settings: Settings) -> Result<Settings, SettingsError> {
    let gitlab_url = normalize_gitlab_url(settings.gitlab_url.as_deref())?;
    let mut hidden: Vec<String> = Vec::new();
    for key in settings.hidden {
        let key = key.trim();
        if !key.is_empty() && key.len() <= MAX_KEY_LEN && !hidden.iter().any(|k| k == key) {
            hidden.push(key.to_owned());
        }
    }
    Ok(Settings {
        gitlab_url,
        hidden,
        ..settings
    })
}

/// Reads settings from `text`. Unknown fields are ignored and missing ones take
/// their defaults.
pub fn parse(text: &str) -> serde_json::Result<Settings> {
    serde_json::from_str(text)
}

fn file_path(app: &AppHandle) -> io::Result<PathBuf> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join(FILE))
        .map_err(io::Error::other)
}

/// The settings in `path`. A missing file gives the defaults, and so does one that
/// cannot be read, after saying why on stderr.
pub fn load_from(path: &Path) -> Settings {
    let _guard = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Settings::default(),
        Err(err) => {
            eprintln!(
                "refract: cannot read {}: {err}; using defaults",
                path.display()
            );
            return Settings::default();
        }
    };
    match parse(&text)
        .map_err(|err| err.to_string())
        .and_then(|s| normalize(s).map_err(|err| err.message))
    {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!(
                "refract: {} is not valid: {err}; using defaults",
                path.display()
            );
            Settings::default()
        }
    }
}

/// Writes `settings` to `path` through a temporary file, so a crash cannot leave
/// half a file.
pub fn save_to(path: &Path, settings: &Settings) -> io::Result<()> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

/// The current settings; the defaults if there is no file yet.
pub fn load(app: &AppHandle) -> Settings {
    match file_path(app) {
        Ok(path) => load_from(&path),
        Err(err) => {
            eprintln!("refract: no config directory: {err}; using default settings");
            Settings::default()
        }
    }
}

#[tauri::command]
pub async fn settings_get(app: AppHandle) -> Result<Settings, SettingsError> {
    tauri::async_runtime::spawn_blocking(move || load(&app))
        .await
        .map_err(|err| {
            SettingsError::new(
                SettingsErrorKind::Internal,
                format!("Reading settings failed: {err}"),
            )
        })
}

/// Checks and saves `settings`; the next remote listing uses them.
#[tauri::command]
pub async fn settings_set(app: AppHandle, settings: Settings) -> Result<(), SettingsError> {
    let settings = normalize(settings)?;
    tauri::async_runtime::spawn_blocking(move || {
        let path = file_path(&app).map_err(|err| {
            SettingsError::new(
                SettingsErrorKind::Io,
                format!("No place to save settings: {err}"),
            )
        })?;
        save_to(&path, &settings).map_err(|err| {
            SettingsError::new(
                SettingsErrorKind::Io,
                format!("Cannot save settings to {}: {err}", path.display()),
            )
        })
    })
    .await
    .map_err(|err| {
        SettingsError::new(
            SettingsErrorKind::Internal,
            format!("Saving settings failed: {err}"),
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_wire_format() {
        let defaults = parse("{}").unwrap();
        assert_eq!(defaults, Settings::default());
        assert_eq!(
            serde_json::to_value(&defaults).unwrap(),
            json!({"gitlabUrl": null, "showForks": false, "showArchived": false, "hidden": []})
        );
        let read =
            parse(r#"{"gitlabUrl": "https://gitlab.example.com", "showForks": true, "later": 1}"#)
                .unwrap();
        assert_eq!(
            read.gitlab_url.as_deref(),
            Some("https://gitlab.example.com")
        );
        assert!(read.show_forks && !read.show_archived);
    }

    #[test]
    fn gitlab_addresses_are_normalized() {
        let ok = |url| normalize_gitlab_url(Some(url)).unwrap();
        assert_eq!(
            ok("  https://gitlab.example.com/ "),
            Some("https://gitlab.example.com".into())
        );
        assert_eq!(
            ok("HTTPS://gitlab.example.com/sub//"),
            Some("https://gitlab.example.com/sub".into())
        );
        assert_eq!(ok(""), None);
        assert_eq!(normalize_gitlab_url(None).unwrap(), None);
        for bad in [
            "http://gitlab.example.com",
            "gitlab.example.com",
            "https://",
            "https://gitlab.example.com/?x=1",
            "https://gitlab example.com",
        ] {
            let err = normalize_gitlab_url(Some(bad)).unwrap_err();
            assert_eq!(err.kind, SettingsErrorKind::Invalid, "{bad}");
        }
        assert_eq!(
            ok("https://gitlab.example.com:8443/"),
            Some("https://gitlab.example.com:8443".into())
        );
    }

    #[test]
    fn gitlab_addresses_with_user_info_are_refused_unrepeated() {
        for bad in [
            "https://oauth2:glpat-secret@gitlab.example.com",
            "https://glpat-secret@gitlab.example.com/sub",
            "http://user:glpat-secret@gitlab.example.com",
            "https://user:glpat secret@gitlab.example.com",
            "user:glpat-secret@gitlab.example.com",
        ] {
            let err = normalize_gitlab_url(Some(bad)).unwrap_err();
            assert_eq!(err.kind, SettingsErrorKind::Invalid, "{bad}");
            assert!(err.message.contains("token field"), "{}", err.message);
            assert!(!err.message.contains("secret"), "{}", err.message);
        }
        // An `@` past the host is a path, not user info.
        assert!(normalize_gitlab_url(Some("https://gitlab.example.com/@group")).is_ok());
    }

    #[test]
    fn hidden_keys_are_cleaned() {
        let settings = normalize(Settings {
            hidden: vec![
                " /home/dev/projects/kelp ".into(),
                "remote:github.com/example-dev/tidepool-api".into(),
                "".into(),
                "/home/dev/projects/kelp".into(),
                "x".repeat(MAX_KEY_LEN + 1),
            ],
            ..Settings::default()
        })
        .unwrap();
        assert_eq!(
            settings.hidden,
            [
                "/home/dev/projects/kelp",
                "remote:github.com/example-dev/tidepool-api"
            ]
        );
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("refract-settings-{}", std::process::id()));
        let path = dir.join("nested").join(FILE);
        assert_eq!(load_from(&path), Settings::default());
        let settings = Settings {
            gitlab_url: Some("https://gitlab.example.com".into()),
            show_forks: true,
            show_archived: false,
            hidden: vec!["remote:gitlab.example.com/example-group/reef-survey".into()],
        };
        save_to(&path, &settings).unwrap();
        assert_eq!(load_from(&path), settings);
        let changed = Settings {
            show_archived: true,
            ..settings
        };
        save_to(&path, &changed).unwrap();
        assert_eq!(load_from(&path), changed);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_from(&path), Settings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

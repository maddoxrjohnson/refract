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
/// Most rows kept in the cost estimate's price table.
pub const MAX_PRICE_ROWS: usize = 50;
/// Longest model match kept; real ones are a few characters of a model id.
const MAX_MATCH_LEN: usize = 100;
/// Highest price accepted, in US dollars per million tokens; real ones are below 100.
const MAX_PRICE: f64 = 10_000.0;

/// The default price table, shared with the interface (its "Reset to defaults"). Taken
/// from Anthropic's pricing page on 2026-10-30; the file says so in `takenOn`.
const DEFAULT_PRICES: &str = include_str!("../../src/usage/defaultPrices.json");

/// Reads and writes of the file happen one at a time.
static FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The GitLab instance, such as `https://gitlab.example.com`; `None` turns
    /// GitLab off.
    pub gitlab_url: Option<String>,
    pub show_forks: bool,
    pub show_archived: bool,
    /// Project keys the user hid.
    pub hidden: Vec<String>,
    /// The usage widget's optional cost estimate (spec §3.5). A value of the wrong
    /// shape gives the defaults rather than failing the whole file.
    #[serde(deserialize_with = "lenient_cost_estimate")]
    pub cost_estimate: CostEstimate,
}

/// Reads `costEstimate`, falling back to the defaults, after saying why on stderr, when
/// it is not the right shape.
fn lenient_cost_estimate<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<CostEstimate, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|err| {
        eprintln!("refract: the saved cost estimate is not valid: {err}; using its defaults");
        CostEstimate::default()
    }))
}

/// Whether the usage widget estimates cost, and at what prices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CostEstimate {
    pub enabled: bool,
    /// Tried in order; the first row whose `match` is in the model id prices it.
    pub prices: Vec<PriceRow>,
}

impl Default for CostEstimate {
    fn default() -> CostEstimate {
        CostEstimate {
            enabled: false,
            prices: default_prices(),
        }
    }
}

/// Prices for the models whose id holds `pattern`, ignoring case, in US dollars per
/// million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceRow {
    #[serde(rename = "match")]
    pub pattern: String,
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

/// The prices shipped with the app.
pub fn default_prices() -> Vec<PriceRow> {
    #[derive(Deserialize)]
    struct File {
        prices: Vec<PriceRow>,
    }
    // Checked by a test, so this cannot fail in a build that passed them.
    serde_json::from_str::<File>(DEFAULT_PRICES)
        .map(|f| f.prices)
        .unwrap_or_default()
}

/// `cost` as it is saved: matches trimmed, every price finite, from 0 to
/// [`MAX_PRICE`], at most [`MAX_PRICE_ROWS`] rows.
pub fn normalize_cost_estimate(cost: CostEstimate) -> Result<CostEstimate, SettingsError> {
    let invalid = |message: String| SettingsError::new(SettingsErrorKind::Invalid, message);
    if cost.prices.len() > MAX_PRICE_ROWS {
        return Err(invalid(format!(
            "The price table can have at most {MAX_PRICE_ROWS} rows."
        )));
    }
    let mut prices = Vec::with_capacity(cost.prices.len());
    for row in cost.prices {
        let pattern = row.pattern.trim().to_owned();
        if pattern.is_empty() {
            return Err(invalid("Every price row needs a model to match.".into()));
        }
        if pattern.chars().count() > MAX_MATCH_LEN {
            return Err(invalid(format!(
                "A model match can be at most {MAX_MATCH_LEN} characters."
            )));
        }
        let values = [
            row.input,
            row.output,
            row.cache_write_5m,
            row.cache_write_1h,
            row.cache_read,
        ];
        if values.iter().any(|v| !(0.0..=MAX_PRICE).contains(v)) {
            return Err(invalid(format!(
                "Prices for \"{pattern}\" must be numbers from 0 to {MAX_PRICE}."
            )));
        }
        prices.push(PriceRow { pattern, ..row });
    }
    Ok(CostEstimate {
        enabled: cost.enabled,
        prices,
    })
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
/// with blanks, overlong ones and repeats dropped, and the price table checked.
pub fn normalize(settings: Settings) -> Result<Settings, SettingsError> {
    let gitlab_url = normalize_gitlab_url(settings.gitlab_url.as_deref())?;
    let cost_estimate = normalize_cost_estimate(settings.cost_estimate)?;
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
        cost_estimate,
        ..settings
    })
}

/// Settings read from the file, normalized as on save, except that a price table that
/// fails its checks is replaced by the defaults rather than failing the rest.
pub fn normalize_loaded(mut settings: Settings) -> Result<Settings, SettingsError> {
    let cost = std::mem::take(&mut settings.cost_estimate);
    settings.cost_estimate = normalize_cost_estimate(cost).unwrap_or_else(|err| {
        eprintln!(
            "refract: the saved price table is not valid: {}; using its defaults",
            err.message
        );
        CostEstimate::default()
    });
    normalize(settings)
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
        .and_then(|s| normalize_loaded(s).map_err(|err| err.message))
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
        let mut wire = serde_json::to_value(&defaults).unwrap();
        let cost = wire
            .as_object_mut()
            .unwrap()
            .remove("costEstimate")
            .unwrap();
        assert_eq!(
            wire,
            json!({"gitlabUrl": null, "showForks": false, "showArchived": false, "hidden": []})
        );
        assert_eq!(cost["enabled"], false);
        assert_eq!(
            cost["prices"][0],
            json!({"match": "fable-5-1", "input": 10.0, "output": 50.0, "cacheWrite5m": 12.5,
                "cacheWrite1h": 20.0, "cacheRead": 0.25})
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
    fn default_prices_are_valid_and_ordered() {
        let prices = default_prices();
        assert!(prices.len() >= 10 && prices.len() <= MAX_PRICE_ROWS);
        let checked = normalize_cost_estimate(CostEstimate::default()).unwrap();
        assert_eq!(checked.prices, prices);
        // A row never hides a later one: an earlier match is never part of a later one,
        // or `opus-5` would price `opus-5-5`.
        for (i, earlier) in prices.iter().enumerate() {
            for later in &prices[i + 1..] {
                assert!(
                    !later.pattern.contains(&earlier.pattern),
                    "{} hides {}",
                    earlier.pattern,
                    later.pattern
                );
            }
        }
        // Each model id seen in real transcripts has a row.
        for model in [
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-sonnet-5-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
        ] {
            assert!(prices.iter().any(|p| model.contains(&p.pattern)), "{model}");
        }
    }

    #[test]
    fn old_files_get_the_default_prices() {
        let read = parse(r#"{"showForks": true}"#).unwrap();
        assert_eq!(read.cost_estimate, CostEstimate::default());
        assert!(!read.cost_estimate.enabled);
        let read = parse(r#"{"costEstimate": {"enabled": true}}"#).unwrap();
        assert!(read.cost_estimate.enabled);
        assert_eq!(read.cost_estimate.prices, default_prices());
        let read = parse(r#"{"costEstimate": {"enabled": true, "prices": []}}"#).unwrap();
        assert!(read.cost_estimate.prices.is_empty());
    }

    #[test]
    fn price_tables_are_checked() {
        let row = |pattern: &str, input: f64| PriceRow {
            pattern: pattern.into(),
            input,
            output: 1.0,
            cache_write_5m: 0.0,
            cache_write_1h: 0.0,
            cache_read: 0.0,
        };
        let check = |prices: Vec<PriceRow>| {
            normalize_cost_estimate(CostEstimate {
                enabled: true,
                prices,
            })
        };
        assert_eq!(
            check(vec![row("  opus ", 1.5)]).unwrap().prices,
            [row("opus", 1.5)]
        );
        for bad in [
            vec![row("opus", -1.0)],
            vec![row("opus", f64::NAN)],
            vec![row("opus", f64::INFINITY)],
            vec![row("  ", 1.0)],
            vec![row(&"x".repeat(MAX_MATCH_LEN + 1), 1.0)],
            vec![row("opus", 1.0); MAX_PRICE_ROWS + 1],
        ] {
            assert_eq!(check(bad).unwrap_err().kind, SettingsErrorKind::Invalid);
        }
        assert!(check(vec![row("opus", 1.0); MAX_PRICE_ROWS]).is_ok());
        assert!(check(vec![row("opus", MAX_PRICE)]).is_ok());
        assert!(check(vec![row("opus", MAX_PRICE + 0.01)]).is_err());
    }

    #[test]
    fn a_bad_price_table_in_the_file_resets_only_the_prices() {
        let dir = tempfile_dir("bad-prices");
        let path = dir.join(FILE);
        let keep = r#""gitlabUrl": "https://gitlab.example.com", "showForks": true, "hidden": ["/home/dev/projects/kelp"]"#;
        let expected = Settings {
            gitlab_url: Some("https://gitlab.example.com".into()),
            show_forks: true,
            hidden: vec!["/home/dev/projects/kelp".into()],
            ..Settings::default()
        };
        for cost in [
            // Fails the checks: a negative price.
            r#"{"enabled": true, "prices": [{"match": "opus", "input": -1, "output": 0, "cacheWrite5m": 0, "cacheWrite1h": 0, "cacheRead": 0}]}"#,
            // The wrong shape.
            r#"{"enabled": "yes", "prices": "none"}"#,
            r#"[1, 2]"#,
        ] {
            std::fs::write(&path, format!(r#"{{{keep}, "costEstimate": {cost}}}"#)).unwrap();
            assert_eq!(load_from(&path), expected, "{cost}");
        }
        // Saving still refuses a bad table.
        let settings = parse(r#"{"costEstimate": {"enabled": true, "prices": [{"match": "opus", "input": -1, "output": 0, "cacheWrite5m": 0, "cacheWrite1h": 0, "cacheRead": 0}]}}"#).unwrap();
        assert!(normalize(settings).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("refract-settings-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
            cost_estimate: CostEstimate {
                enabled: true,
                prices: vec![PriceRow {
                    pattern: "sonnet".into(),
                    input: 2.0,
                    output: 10.0,
                    cache_write_5m: 2.5,
                    cache_write_1h: 4.0,
                    cache_read: 0.1,
                }],
            },
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

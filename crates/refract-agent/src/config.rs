//! Where to look for repos and sessions.
//!
//! An optional TOML file at `$REFRACT_AGENT_CONFIG`, or else
//! `$XDG_CONFIG_HOME/refract/agent.toml` (default `~/.config/refract/agent.toml`):
//!
//! ```toml
//! roots = ["~/projects", "~/code"]   # replaces the default roots
//! extra = ["~/work/tidepool-api"]    # single repos outside the roots
//! hidden = ["~/projects/old-thing"]  # repos never reported
//! depth = 3                          # how deep to look under each root
//! limits_file = "~/.claude/usage-state.json"  # plan limits from the status line
//!                                             # (default: in $CLAUDE_CONFIG_DIR or ~/.claude)
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// How many directory levels below a root are searched by default.
pub const DEFAULT_DEPTH: usize = 3;

/// The agent's settings, with `~` expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directories searched for repos.
    pub roots: Vec<PathBuf>,
    /// Repos reported even though they are under no root.
    pub extra: Vec<PathBuf>,
    /// Repos never reported.
    pub hidden: Vec<PathBuf>,
    /// Directory levels searched below each root.
    pub depth: usize,
    /// Claude Code's `projects` directory.
    pub claude_projects: PathBuf,
    /// The file a status line script writes plan limits to (see
    /// [`refract_core::limits`]), by default `usage-state.json` in Claude Code's config
    /// directory, like `claude_projects`. Only ever read.
    pub limits_file: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    roots: Option<Vec<String>>,
    extra: Vec<String>,
    hidden: Vec<String>,
    depth: Option<usize>,
    limits_file: Option<String>,
}

impl Config {
    /// Loads the config from the environment and the optional config file. A broken
    /// file is reported on stderr and ignored.
    pub fn load() -> Config {
        let home = home_dir();
        let path = file_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        match Config::parse(&text, &home, &claude_dir, windows_desktop) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("refract-agent: ignoring {}: {e}", path.display());
                Config::parse("", &home, &claude_dir, windows_desktop).expect("empty config")
            }
        }
    }

    /// Builds a config from TOML text. `desktop` is only called when the file sets no
    /// roots, since finding the Windows Desktop may start `cmd.exe`.
    pub fn parse(
        text: &str,
        home: &Path,
        claude_dir: &Path,
        desktop: impl FnOnce() -> Option<PathBuf>,
    ) -> Result<Config, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let expand = |paths: Vec<String>| -> Vec<PathBuf> {
            paths.iter().map(|p| expand_home(p, home)).collect()
        };
        let roots = match file.roots {
            Some(roots) => expand(roots),
            None => {
                let mut roots = vec![home.join("projects"), home.join("code")];
                roots.extend(desktop());
                roots
            }
        };
        Ok(Config {
            roots,
            extra: expand(file.extra),
            hidden: expand(file.hidden),
            depth: file.depth.unwrap_or(DEFAULT_DEPTH),
            claude_projects: claude_dir.join("projects"),
            limits_file: file.limits_file.map_or_else(
                || claude_dir.join("usage-state.json"),
                |p| expand_home(&p, home),
            ),
        })
    }
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// Where the config file is, whether or not it exists.
pub fn file_path() -> PathBuf {
    std::env::var_os("REFRACT_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join(".config"))
                .join("refract/agent.toml")
        })
}

/// Appends `repo` to `extra` in the config file at `file`, creating the file if it is
/// missing, and keeps the rest of the file as it was, comments included. Returns false
/// when `extra` already names it. The error is a message for the app.
pub fn add_extra(file: &Path, repo: &Path) -> Result<bool, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("cannot update {}: {e}", file.display()))?;
    let Some(repo_str) = repo.to_str() else {
        return Err(format!("not a UTF-8 path: {}", repo.display()));
    };
    let extra = doc
        .entry("extra")
        .or_insert_with(|| toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(|| format!("extra in {} is not a list", file.display()))?;
    let home = home_dir();
    if extra
        .iter()
        .filter_map(|v| v.as_str())
        .any(|p| expand_home(p, &home) == repo)
    {
        return Ok(false);
    }
    extra.push(repo_str);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(file, doc.to_string())
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(true)
}

/// Removes `repo` from `hidden` in the config file at `file`, keeping the rest of the
/// file as it was. Returns false when the file or `hidden` does not name it. The error
/// is a message for the app.
pub fn remove_hidden(file: &Path, repo: &Path) -> Result<bool, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("cannot update {}: {e}", file.display()))?;
    let Some(hidden) = doc.get_mut("hidden") else {
        return Ok(false);
    };
    let hidden = hidden
        .as_array_mut()
        .ok_or_else(|| format!("hidden in {} is not a list", file.display()))?;
    let home = home_dir();
    let before = hidden.len();
    hidden.retain(|v| v.as_str().is_none_or(|p| expand_home(p, &home) != repo));
    if hidden.len() == before {
        return Ok(false);
    }
    std::fs::write(file, doc.to_string())
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(true)
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_owned(),
        None => PathBuf::from(path),
    }
}

/// The Windows Desktop seen from WSL, if it exists. The Windows user name comes from
/// `$REFRACT_WINDOWS_USER`, or else `cmd.exe /c echo %USERNAME%`.
fn windows_desktop() -> Option<PathBuf> {
    let user = std::env::var("REFRACT_WINDOWS_USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(windows_user_from_cmd)?;
    let profile = Path::new("/mnt/c/Users").join(user);
    [profile.join("Desktop"), profile.join("OneDrive/Desktop")]
        .into_iter()
        .find(|p| p.is_dir())
}

fn windows_user_from_cmd() -> Option<String> {
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/c", "echo %USERNAME%"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // cmd.exe warns about UNC paths when started from a Linux directory.
    if Path::new("/mnt/c").is_dir() {
        cmd.current_dir("/mnt/c");
    }
    let out = cmd.output().ok()?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !user.is_empty() && !user.contains('%')).then_some(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/dev";

    fn parse(text: &str) -> Config {
        Config::parse(
            text,
            Path::new(HOME),
            Path::new("/home/dev/.claude"),
            || Some(PathBuf::from("/mnt/c/Users/dev/Desktop")),
        )
        .unwrap()
    }

    #[test]
    fn defaults_include_desktop() {
        let c = parse("");
        assert_eq!(
            c.roots,
            [
                PathBuf::from("/home/dev/projects"),
                PathBuf::from("/home/dev/code"),
                PathBuf::from("/mnt/c/Users/dev/Desktop"),
            ]
        );
        assert_eq!(c.depth, DEFAULT_DEPTH);
        assert_eq!(
            c.claude_projects,
            PathBuf::from("/home/dev/.claude/projects")
        );
        assert_eq!(
            c.limits_file,
            PathBuf::from("/home/dev/.claude/usage-state.json")
        );
    }

    #[test]
    fn limits_file_can_be_moved() {
        let c = parse("limits_file = \"~/state/limits.json\"");
        assert_eq!(c.limits_file, PathBuf::from("/home/dev/state/limits.json"));
        let other_dir = Config::parse("", Path::new(HOME), Path::new("/srv/claude"), || None);
        assert_eq!(
            other_dir.unwrap().limits_file,
            PathBuf::from("/srv/claude/usage-state.json")
        );
        let c = parse("limits_file = \"/srv/limits.json\"");
        assert_eq!(c.limits_file, PathBuf::from("/srv/limits.json"));
        let err = Config::parse(
            "limit_file = \"x\"",
            Path::new(HOME),
            Path::new("/c"),
            || None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn configured_roots_replace_defaults_without_asking_for_desktop() {
        let c = Config::parse(
            "roots = [\"~/src\", \"/srv/repos\"]\nextra = [\"~/x/aurora-notes\"]\ndepth = 1",
            Path::new(HOME),
            Path::new("/c"),
            || panic!("desktop looked up"),
        )
        .unwrap();
        assert_eq!(
            c.roots,
            [PathBuf::from("/home/dev/src"), PathBuf::from("/srv/repos")]
        );
        assert_eq!(c.extra, [PathBuf::from("/home/dev/x/aurora-notes")]);
        assert_eq!(c.depth, 1);
    }

    #[test]
    fn add_extra_keeps_the_rest_of_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("refract/agent.toml");
        let repo = Path::new("/srv/work/aurora-notes");
        assert_eq!(add_extra(&file, repo), Ok(true));
        let first = std::fs::read_to_string(&file).unwrap();
        let c = parse(&first);
        assert_eq!(c.extra, [PathBuf::from("/srv/work/aurora-notes")]);

        let text =
            "# my repos\nroots = [\"~/src\"] # only this\nextra = [\"~/x/kelp-cli\"]\ndepth = 2\n";
        std::fs::write(&file, text).unwrap();
        assert_eq!(add_extra(&file, repo), Ok(true));
        assert_eq!(add_extra(&file, repo), Ok(false));
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.starts_with("# my repos\nroots = [\"~/src\"] # only this\n"));
        let c = parse(&after);
        assert_eq!(c.roots, [PathBuf::from("/home/dev/src")]);
        assert_eq!(c.depth, 2);
        assert_eq!(c.extra.len(), 2);
        assert_eq!(c.extra[1], PathBuf::from("/srv/work/aurora-notes"));

        std::fs::write(&file, "extra = \"oops\"\n").unwrap();
        assert!(add_extra(&file, repo).is_err());
        std::fs::write(&file, "extra = [\n").unwrap();
        assert!(add_extra(&file, repo).is_err());
    }

    #[test]
    fn remove_hidden_keeps_the_rest_of_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("agent.toml");
        let repo = Path::new("/srv/work/aurora-notes");
        assert_eq!(remove_hidden(&file, repo), Ok(false));

        let text = "# my repos\nroots = [\"~/src\"] # only this\nhidden = [\"/srv/work/aurora-notes\", \"~/x/kelp-cli\"]\n";
        std::fs::write(&file, text).unwrap();
        assert_eq!(remove_hidden(&file, repo), Ok(true));
        assert_eq!(remove_hidden(&file, repo), Ok(false));
        let after = std::fs::read_to_string(&file).unwrap();
        assert!(after.starts_with("# my repos\nroots = [\"~/src\"] # only this\n"));
        let c = parse(&after);
        assert_eq!(c.roots, [PathBuf::from("/home/dev/src")]);
        assert_eq!(c.hidden, [PathBuf::from("/home/dev/x/kelp-cli")]);

        std::fs::write(&file, "roots = []\n").unwrap();
        assert_eq!(remove_hidden(&file, repo), Ok(false));
        std::fs::write(&file, "hidden = \"oops\"\n").unwrap();
        assert!(remove_hidden(&file, repo).is_err());
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = Config::parse("rots = []", Path::new(HOME), Path::new("/c"), || None);
        assert!(err.is_err());
    }
}

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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    roots: Option<Vec<String>>,
    extra: Vec<String>,
    hidden: Vec<String>,
    depth: Option<usize>,
}

impl Config {
    /// Loads the config from the environment and the optional config file. A broken
    /// file is reported on stderr and ignored.
    pub fn load() -> Config {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
        let path = std::env::var_os("REFRACT_AGENT_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"))
                    .join("refract/agent.toml")
            });
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
        })
    }
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
    fn unknown_keys_are_errors() {
        let err = Config::parse("rots = []", Path::new(HOME), Path::new("/c"), || None);
        assert!(err.is_err());
    }
}

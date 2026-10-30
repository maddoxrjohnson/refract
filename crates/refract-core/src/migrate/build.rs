//! Which build folders a migration leaves behind (spec §3.2), and what reinstalls them
//! afterwards (spec §3.5).
//!
//! A folder is skipped only when git ignores the folder itself **and** its name matches
//! a rule. "Git ignores the folder itself" means `git status --porcelain=v2
//! --ignored=matching` lists it as `! <path>/`: git only does that when nothing inside
//! is tracked, so tracked content is never skipped. Rules apply at any depth, but not
//! inside `.git` and not inside a folder already skipped.
//!
//! Only skipped folders at the repo root get an install step, and only `node_modules`
//! and the Python virtual environments need one. A nested `node_modules` (a workspace
//! package) is normally restored by the root install.

use serde::{Deserialize, Serialize};

use crate::migrate::manifest::RelPath;

/// Why a folder counts as a build folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BuildFolder {
    /// `node_modules`.
    NodeModules,
    /// `.venv`, `venv` or `env` holding a `pyvenv.cfg`.
    PythonVenv,
    /// `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`.
    PythonCache,
    /// `target` with a `CACHEDIR.TAG` inside or a `Cargo.toml` next to it.
    CargoTarget,
    /// `.next`, `.nuxt`, `.turbo`, `.parcel-cache`, `.svelte-kit`, `.gradle`.
    ToolCache,
    /// `dist`, `build`, `out`.
    Output,
}

/// What [`skip_rule`] needs to know about one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirFacts<'a> {
    /// The folder's own name, not its path.
    pub name: &'a str,
    /// Git ignores the folder itself (see the module docs).
    pub ignored: bool,
    /// It holds a file named `pyvenv.cfg`.
    pub has_pyvenv_cfg: bool,
    /// It holds a file named `CACHEDIR.TAG`.
    pub has_cachedir_tag: bool,
    /// Its parent folder holds a `Cargo.toml`.
    pub sibling_cargo_toml: bool,
}

/// The rule that makes a directory a skippable build folder, or `None` when it is
/// copied. Names match exactly, case included. `.venv`, `venv` and `env` all need a
/// `pyvenv.cfg`, so a folder named `env` holding settings is copied.
pub fn skip_rule(dir: &DirFacts<'_>) -> Option<BuildFolder> {
    if !dir.ignored {
        return None;
    }
    match dir.name {
        "node_modules" => Some(BuildFolder::NodeModules),
        ".venv" | "venv" | "env" if dir.has_pyvenv_cfg => Some(BuildFolder::PythonVenv),
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox" => {
            Some(BuildFolder::PythonCache)
        }
        "target" if dir.has_cachedir_tag || dir.sibling_cargo_toml => {
            Some(BuildFolder::CargoTarget)
        }
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".gradle" => {
            Some(BuildFolder::ToolCache)
        }
        "dist" | "build" | "out" => Some(BuildFolder::Output),
        _ => None,
    }
}

/// A folder left out of the copy, as the dialog and the report list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedFolder {
    pub path: RelPath,
    pub rule: BuildFolder,
    /// Total size of the files inside, in bytes.
    pub bytes: u64,
    /// Number of files inside.
    pub files: u64,
}

/// The files at the repo root that pick the install command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RootFiles {
    /// Names of the files at the root (only the lockfile and config names below matter).
    pub names: Vec<String>,
    /// `packageManager` from `package.json`, such as `pnpm@9.1.0` or `yarn@4.2.2`.
    pub package_manager: Option<String>,
    /// The yarn.lock is Berry's (it has a `__metadata:` block) rather than v1's
    /// (`# yarn lockfile v1`).
    pub yarn_lock_berry: bool,
}

impl RootFiles {
    /// Reads the facts from the root's file names and, when present, the text of
    /// `package.json` and the start of `yarn.lock` (the first few lines are enough). A
    /// `package.json` that does not parse counts as having no `packageManager`.
    pub fn new(names: Vec<String>, package_json: Option<&str>, yarn_lock: Option<&str>) -> Self {
        let package_manager = package_json
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|json| json.get("packageManager")?.as_str().map(str::to_owned));
        let yarn_lock_berry = yarn_lock.is_some_and(|text| {
            text.lines()
                .take(20)
                .any(|line| line.trim_end() == "__metadata:")
        });
        RootFiles {
            names,
            package_manager,
            yarn_lock_berry,
        }
    }

    fn has(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }
}

/// A command run in the destination after the checks pass (through `bash -lc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStep {
    /// The skipped folder it recreates.
    pub folder: RelPath,
    /// The program that must exist in WSL, looked up with `command -v`.
    pub tool: String,
    /// The shell command, run in the destination.
    pub command: String,
    /// What to run to get `tool` when it is missing.
    pub install_hint: String,
}

/// The install steps for the skipped folders at the repo root (spec §3.5), one per
/// folder that needs one, in the order of `skipped`.
///
/// `node_modules` takes, in order of precedence: the tool named by `packageManager`,
/// then `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`/`bun.lockb`, and otherwise npm
/// (`npm ci` with a `package-lock.json` or `npm-shrinkwrap.json`, `npm install`
/// without). Yarn is Berry (`--immutable`) when there is a `.yarnrc.yml`, when
/// `packageManager` names yarn 2 or later, or when the lockfile is Berry's; otherwise
/// v1 (`--frozen-lockfile`).
///
/// A Python environment takes `uv sync` with a `uv.lock` and `poetry install` with a
/// `poetry.lock` (both only for `.venv`, where they put it), and otherwise a fresh
/// venv of the same name with `requirements.txt` installed. With none of these there
/// is nothing to reinstall from, and no step.
pub fn install_plan(skipped: &[SkippedFolder], root: &RootFiles) -> Vec<InstallStep> {
    skipped
        .iter()
        .filter(|folder| !folder.path.as_str().contains('/'))
        .filter_map(|folder| match folder.rule {
            BuildFolder::NodeModules if root.has("package.json") => {
                Some(node_step(&folder.path, root))
            }
            BuildFolder::PythonVenv => python_step(&folder.path, root),
            _ => None,
        })
        .collect()
}

fn step(folder: &RelPath, tool: &str, command: String, hint: &str) -> InstallStep {
    InstallStep {
        folder: folder.clone(),
        tool: tool.into(),
        command,
        install_hint: hint.into(),
    }
}

const NODE_HINT: &str = "Install Node.js in WSL, for example with nvm: \
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash \
    && nvm install --lts";

fn node_step(folder: &RelPath, root: &RootFiles) -> InstallStep {
    // `packageManager` is `<name>@<version>`, the version possibly followed by `+<hash>`.
    let named = root
        .package_manager
        .as_deref()
        .map(|pm| match pm.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (pm, None),
        });
    let tool = named
        .map(|(name, _)| name)
        .filter(|name| matches!(*name, "pnpm" | "yarn" | "bun" | "npm"))
        .or_else(|| {
            [
                ("pnpm-lock.yaml", "pnpm"),
                ("yarn.lock", "yarn"),
                ("bun.lock", "bun"),
                ("bun.lockb", "bun"),
            ]
            .into_iter()
            .find(|(lock, _)| root.has(lock))
            .map(|(_, tool)| tool)
        })
        .unwrap_or("npm");
    match tool {
        "pnpm" => step(
            folder,
            "pnpm",
            "pnpm install --frozen-lockfile".into(),
            "corepack enable pnpm",
        ),
        "yarn" => {
            let major =
                named.and_then(|(_, version)| version?.split('.').next()?.parse::<u32>().ok());
            let berry = root.has(".yarnrc.yml")
                || major.is_some_and(|major| major >= 2)
                || root.yarn_lock_berry;
            let flag = if berry {
                "--immutable"
            } else {
                "--frozen-lockfile"
            };
            step(
                folder,
                "yarn",
                format!("yarn install {flag}"),
                "corepack enable yarn",
            )
        }
        "bun" => step(
            folder,
            "bun",
            "bun install".into(),
            "curl -fsSL https://bun.sh/install | bash",
        ),
        _ => {
            let locked = root.has("package-lock.json") || root.has("npm-shrinkwrap.json");
            let command = if locked { "npm ci" } else { "npm install" };
            step(folder, "npm", command.into(), NODE_HINT)
        }
    }
}

fn python_step(folder: &RelPath, root: &RootFiles) -> Option<InstallStep> {
    let name = folder.as_str();
    if name == ".venv" && root.has("uv.lock") {
        return Some(step(
            folder,
            "uv",
            "uv sync".into(),
            "curl -LsSf https://astral.sh/uv/install.sh | sh",
        ));
    }
    if name == ".venv" && root.has("poetry.lock") {
        return Some(step(
            folder,
            "poetry",
            "poetry install".into(),
            "sudo apt install pipx && pipx install poetry",
        ));
    }
    if root.has("requirements.txt") {
        let quoted = shell_quote(name);
        return Some(step(
            folder,
            "python3",
            format!("python3 -m venv {quoted} && {quoted}/bin/pip install -r requirements.txt"),
            "sudo apt install python3-venv python3-pip",
        ));
    }
    None
}

/// `word` as one POSIX shell word: as it is when it holds only safe characters, else
/// in single quotes.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/+@%=:,".contains(&b));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(name: &str) -> DirFacts<'_> {
        DirFacts {
            name,
            ignored: true,
            ..DirFacts::default()
        }
    }

    #[test]
    fn plain_build_folders_are_skipped_when_ignored() {
        for (name, rule) in [
            ("node_modules", BuildFolder::NodeModules),
            ("__pycache__", BuildFolder::PythonCache),
            (".pytest_cache", BuildFolder::PythonCache),
            (".mypy_cache", BuildFolder::PythonCache),
            (".ruff_cache", BuildFolder::PythonCache),
            (".tox", BuildFolder::PythonCache),
            (".next", BuildFolder::ToolCache),
            (".nuxt", BuildFolder::ToolCache),
            (".turbo", BuildFolder::ToolCache),
            (".parcel-cache", BuildFolder::ToolCache),
            (".svelte-kit", BuildFolder::ToolCache),
            (".gradle", BuildFolder::ToolCache),
            ("dist", BuildFolder::Output),
            ("build", BuildFolder::Output),
            ("out", BuildFolder::Output),
        ] {
            assert_eq!(skip_rule(&ignored(name)), Some(rule), "{name}");
        }
    }

    #[test]
    fn tracked_folders_are_never_skipped() {
        for name in [
            "node_modules",
            "dist",
            "build",
            "out",
            ".next",
            "__pycache__",
        ] {
            let tracked = DirFacts {
                ignored: false,
                ..ignored(name)
            };
            assert_eq!(skip_rule(&tracked), None, "{name}");
        }
        let venv = DirFacts {
            name: ".venv",
            ignored: false,
            has_pyvenv_cfg: true,
            ..DirFacts::default()
        };
        assert_eq!(skip_rule(&venv), None);
        let target = DirFacts {
            name: "target",
            ignored: false,
            has_cachedir_tag: true,
            sibling_cargo_toml: true,
            ..DirFacts::default()
        };
        assert_eq!(skip_rule(&target), None);
    }

    #[test]
    fn venvs_need_pyvenv_cfg() {
        for name in [".venv", "venv", "env"] {
            assert_eq!(skip_rule(&ignored(name)), None, "{name} without pyvenv.cfg");
            let venv = DirFacts {
                has_pyvenv_cfg: true,
                ..ignored(name)
            };
            assert_eq!(skip_rule(&venv), Some(BuildFolder::PythonVenv), "{name}");
        }
        // A pyvenv.cfg does not make any other folder a venv.
        let other = DirFacts {
            has_pyvenv_cfg: true,
            ..ignored("config")
        };
        assert_eq!(skip_rule(&other), None);
    }

    #[test]
    fn target_needs_a_cache_tag_or_a_cargo_toml_beside_it() {
        assert_eq!(skip_rule(&ignored("target")), None);
        let tagged = DirFacts {
            has_cachedir_tag: true,
            ..ignored("target")
        };
        assert_eq!(skip_rule(&tagged), Some(BuildFolder::CargoTarget));
        let cargo = DirFacts {
            sibling_cargo_toml: true,
            ..ignored("target")
        };
        assert_eq!(skip_rule(&cargo), Some(BuildFolder::CargoTarget));
    }

    #[test]
    fn other_and_differently_cased_names_are_copied() {
        for name in [
            "src",
            "Node_Modules",
            "DIST",
            "dist2",
            ".env",
            "targets",
            "vendor",
        ] {
            assert_eq!(skip_rule(&ignored(name)), None, "{name}");
        }
    }

    fn skipped(path: &str, rule: BuildFolder) -> SkippedFolder {
        SkippedFolder {
            path: RelPath::new(path).unwrap(),
            rule,
            bytes: 1,
            files: 1,
        }
    }

    fn root(names: &[&str]) -> RootFiles {
        RootFiles::new(names.iter().map(|n| n.to_string()).collect(), None, None)
    }

    fn commands(plan: &[InstallStep]) -> Vec<(&str, &str)> {
        plan.iter()
            .map(|s| (s.tool.as_str(), s.command.as_str()))
            .collect()
    }

    #[test]
    fn node_modules_install_follows_the_lockfile() {
        let modules = [skipped("node_modules", BuildFolder::NodeModules)];
        for (names, expect) in [
            (
                &["package.json", "package-lock.json"][..],
                ("npm", "npm ci"),
            ),
            (&["package.json", "npm-shrinkwrap.json"], ("npm", "npm ci")),
            (&["package.json"], ("npm", "npm install")),
            (
                &["package.json", "pnpm-lock.yaml"],
                ("pnpm", "pnpm install --frozen-lockfile"),
            ),
            (
                &["package.json", "yarn.lock"],
                ("yarn", "yarn install --frozen-lockfile"),
            ),
            (
                &["package.json", "yarn.lock", ".yarnrc.yml"],
                ("yarn", "yarn install --immutable"),
            ),
            (&["package.json", "bun.lockb"], ("bun", "bun install")),
            (&["package.json", "bun.lock"], ("bun", "bun install")),
        ] {
            let plan = install_plan(&modules, &root(names));
            assert_eq!(commands(&plan), vec![expect], "{names:?}");
        }
    }

    #[test]
    fn yarn_berry_is_told_from_v1() {
        let modules = [skipped("node_modules", BuildFolder::NodeModules)];
        let names = || vec!["package.json".to_string(), "yarn.lock".to_string()];
        let berry_lock =
            "# This file is generated by running \"yarn install\".\n\n__metadata:\n  version: 8\n";
        let v1_lock = "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n# yarn lockfile v1\n";
        let by_lock = RootFiles::new(names(), Some("{}"), Some(berry_lock));
        assert_eq!(
            install_plan(&modules, &by_lock)[0].command,
            "yarn install --immutable"
        );
        let v1 = RootFiles::new(names(), Some("{}"), Some(v1_lock));
        assert_eq!(
            install_plan(&modules, &v1)[0].command,
            "yarn install --frozen-lockfile"
        );
        let by_field = RootFiles::new(
            names(),
            Some(r#"{"name": "aurora-notes", "packageManager": "yarn@4.2.2+sha512.abc"}"#),
            Some(v1_lock),
        );
        assert_eq!(
            install_plan(&modules, &by_field)[0].command,
            "yarn install --immutable"
        );
        let classic = RootFiles::new(
            names(),
            Some(r#"{"packageManager": "yarn@1.22.22"}"#),
            Some(v1_lock),
        );
        assert_eq!(
            install_plan(&modules, &classic)[0].command,
            "yarn install --frozen-lockfile"
        );
    }

    #[test]
    fn package_manager_field_wins_over_lockfiles() {
        let modules = [skipped("node_modules", BuildFolder::NodeModules)];
        let files = RootFiles::new(
            vec!["package.json".into(), "package-lock.json".into()],
            Some(r#"{"packageManager": "pnpm@9.1.0"}"#),
            None,
        );
        assert_eq!(
            commands(&install_plan(&modules, &files)),
            vec![("pnpm", "pnpm install --frozen-lockfile")]
        );
        // An unknown manager falls back to the lockfiles; broken JSON is ignored.
        let odd = RootFiles::new(
            vec!["package.json".into(), "pnpm-lock.yaml".into()],
            Some(r#"{"packageManager": "deno@2"}"#),
            None,
        );
        assert_eq!(install_plan(&modules, &odd)[0].tool, "pnpm");
        let broken = RootFiles::new(vec!["package.json".into()], Some("{not json"), None);
        assert_eq!(install_plan(&modules, &broken)[0].command, "npm install");
    }

    #[test]
    fn python_environments_follow_their_lockfile() {
        let venv = [skipped(".venv", BuildFolder::PythonVenv)];
        assert_eq!(
            commands(&install_plan(&venv, &root(&["pyproject.toml", "uv.lock"]))),
            vec![("uv", "uv sync")]
        );
        assert_eq!(
            commands(&install_plan(
                &venv,
                &root(&["pyproject.toml", "poetry.lock"])
            )),
            vec![("poetry", "poetry install")]
        );
        assert_eq!(
            commands(&install_plan(&venv, &root(&["requirements.txt"]))),
            vec![(
                "python3",
                "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt"
            )]
        );
        assert!(install_plan(&venv, &root(&["setup.py"])).is_empty());
        // uv and poetry put their environment in `.venv`, so `venv` uses pip.
        let plain = [skipped("venv", BuildFolder::PythonVenv)];
        assert_eq!(
            install_plan(&plain, &root(&["uv.lock", "requirements.txt"]))[0].command,
            "python3 -m venv venv && venv/bin/pip install -r requirements.txt"
        );
    }

    #[test]
    fn only_root_folders_that_need_it_get_a_step() {
        let folders = [
            skipped("dist", BuildFolder::Output),
            skipped("target", BuildFolder::CargoTarget),
            skipped(
                "packages/tidepool-ui/node_modules",
                BuildFolder::NodeModules,
            ),
            skipped("node_modules", BuildFolder::NodeModules),
            skipped("__pycache__", BuildFolder::PythonCache),
        ];
        let plan = install_plan(&folders, &root(&["package.json", "package-lock.json"]));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].folder.as_str(), "node_modules");
        assert!(plan[0].install_hint.contains("nvm"));
        // node_modules with no package.json has nothing to install from.
        let lone = [skipped("node_modules", BuildFolder::NodeModules)];
        assert!(install_plan(&lone, &root(&["README.md"])).is_empty());
    }

    #[test]
    fn shell_words_are_quoted() {
        assert_eq!(shell_quote(".venv"), ".venv");
        assert_eq!(shell_quote("my env"), "'my env'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        let odd = [SkippedFolder {
            path: RelPath::new("env;x").unwrap(),
            rule: BuildFolder::PythonVenv,
            bytes: 0,
            files: 0,
        }];
        let root = RootFiles::new(vec!["requirements.txt".into()], None, None);
        assert_eq!(
            install_plan(&odd, &root)[0].command,
            "python3 -m venv 'env;x' && 'env;x'/bin/pip install -r requirements.txt"
        );
    }
}

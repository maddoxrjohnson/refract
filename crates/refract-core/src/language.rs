//! A repo's main language, which tints its gem (spec §3.2).
//!
//! Best effort: tracked files are counted by extension and the language with the most
//! files wins. Files that are not code (Markdown, JSON, lock files, images) are ignored.

use std::collections::BTreeMap;

/// Languages by file extension (lowercase, without the dot).
const EXTENSIONS: &[(&str, &str)] = &[
    ("rs", "Rust"),
    ("ts", "TypeScript"),
    ("tsx", "TypeScript"),
    ("mts", "TypeScript"),
    ("cts", "TypeScript"),
    ("js", "JavaScript"),
    ("jsx", "JavaScript"),
    ("mjs", "JavaScript"),
    ("cjs", "JavaScript"),
    ("py", "Python"),
    ("go", "Go"),
    ("java", "Java"),
    ("kt", "Kotlin"),
    ("kts", "Kotlin"),
    ("scala", "Scala"),
    ("c", "C"),
    ("h", "C"),
    ("cc", "C++"),
    ("cpp", "C++"),
    ("cxx", "C++"),
    ("hpp", "C++"),
    ("hh", "C++"),
    ("cs", "C#"),
    ("fs", "F#"),
    ("swift", "Swift"),
    ("m", "Objective-C"),
    ("rb", "Ruby"),
    ("php", "PHP"),
    ("lua", "Lua"),
    ("dart", "Dart"),
    ("ex", "Elixir"),
    ("exs", "Elixir"),
    ("erl", "Erlang"),
    ("hs", "Haskell"),
    ("ml", "OCaml"),
    ("clj", "Clojure"),
    ("zig", "Zig"),
    ("nim", "Nim"),
    ("jl", "Julia"),
    ("r", "R"),
    ("sh", "Shell"),
    ("bash", "Shell"),
    ("zsh", "Shell"),
    ("ps1", "PowerShell"),
    ("sql", "SQL"),
    ("html", "HTML"),
    ("css", "CSS"),
    ("scss", "CSS"),
    ("vue", "Vue"),
    ("svelte", "Svelte"),
    ("ipynb", "Jupyter Notebook"),
    ("tex", "TeX"),
    ("s", "Assembly"),
    ("asm", "Assembly"),
];

/// Returns the language with the most tracked files, or `None` if no file is code.
/// Ties go to the alphabetically first language, so the result is stable.
pub fn main_language<'a, I>(paths: I) -> Option<&'static str>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for path in paths {
        if let Some(language) = language_of(path) {
            *counts.entry(language).or_default() += 1;
        }
    }
    let mut best: Option<(&'static str, usize)> = None;
    for (language, count) in counts {
        if best.is_none_or(|(_, top)| count > top) {
            best = Some((language, count));
        }
    }
    best.map(|(language, _)| language)
}

fn language_of(path: &str) -> Option<&'static str> {
    let file = path.rsplit('/').next()?;
    let (stem, ext) = file.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, language)| *language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_common_language_wins() {
        let files = ["src/main.rs", "src/lib.rs", "web/app.ts", "README.md"];
        assert_eq!(main_language(files), Some("Rust"));
    }

    #[test]
    fn related_extensions_count_together() {
        let files = ["a.ts", "b.tsx", "c.js", "d.py"];
        assert_eq!(main_language(files), Some("TypeScript"));
    }

    #[test]
    fn non_code_files_are_ignored() {
        let files = [
            "README.md",
            "package.json",
            "Cargo.lock",
            "logo.png",
            ".gitignore",
        ];
        assert_eq!(main_language(files), None);
    }

    #[test]
    fn ties_are_broken_alphabetically() {
        assert_eq!(main_language(["x.py", "y.go"]), Some("Go"));
    }

    #[test]
    fn extensions_ignore_case_and_dotfiles_have_none() {
        assert_eq!(main_language(["MAIN.PY", ".py"]), Some("Python"));
        assert_eq!(main_language([".rs"]), None);
    }

    #[test]
    fn empty_repo_has_no_language() {
        assert_eq!(main_language(std::iter::empty::<&str>()), None);
    }
}

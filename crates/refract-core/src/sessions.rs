//! Where Claude Code keeps a working directory's transcripts.
//!
//! Transcripts live in `~/.claude/projects/<encoded cwd>/<session id>.jsonl`. The encoding
//! replaces every character that is not an ASCII letter or digit with `-` and keeps case,
//! so `/home/dev/projects/aurora_notes` becomes `-home-dev-projects-aurora-notes` and a
//! dot directory gives a double dash (`/home/dev/.config` → `-home-dev--config`).
//! Checked against real project directories (Claude Code 2.x, 2026), including paths
//! with spaces, underscores and dots.
//!
//! The encoding is lossy (`a-b`, `a_b` and `a/b` collide), so callers must still confirm
//! a transcript belongs to a repo by its `cwd` field. Windows drives mounted under
//! `/mnt/` are case-insensitive and the same folder shows up with different casing
//! (`/mnt/c/Users/...` and `/mnt/c/users/...`), so paths there compare ignoring ASCII
//! case.

/// Encodes a working directory the way Claude Code names its project directory.
pub fn encode_project_dir(cwd: &str) -> String {
    let cwd = trim_trailing_slash(cwd);
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// True when a project directory named `dir_name` may hold transcripts for `cwd`.
pub fn project_dir_matches(dir_name: &str, cwd: &str) -> bool {
    let encoded = encode_project_dir(cwd);
    if is_windows_mount(cwd) {
        dir_name.eq_ignore_ascii_case(&encoded)
    } else {
        dir_name == encoded
    }
}

/// True when two absolute paths name the same directory, ignoring a trailing slash and,
/// under `/mnt/`, ASCII case.
pub fn paths_match(a: &str, b: &str) -> bool {
    let (a, b) = (trim_trailing_slash(a), trim_trailing_slash(b));
    if is_windows_mount(a) && is_windows_mount(b) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// True when `path` is `dir` or lies under it, ignoring a trailing slash and, under
/// `/mnt/`, ASCII case.
pub fn path_within(path: &str, dir: &str) -> bool {
    let (path, dir) = (trim_trailing_slash(path), trim_trailing_slash(dir));
    if dir == "/" {
        return path.starts_with('/');
    }
    match (path.get(..dir.len()), path.as_bytes().get(dir.len())) {
        (Some(head), Some(b'/')) => paths_match(head, dir),
        _ => paths_match(path, dir),
    }
}

/// True when `path` belongs to `repo` rather than to a repo nested in it: it lies within
/// `repo`, and within none of `others` that lie within `repo` themselves. So a session
/// goes to the deepest known repo holding its directory, such as a worktree inside its
/// main repo's folder. `others` may include `repo` itself.
pub fn within_deepest(path: &str, repo: &str, others: &[&str]) -> bool {
    path_within(path, repo)
        && !others.iter().any(|other| {
            !paths_match(other, repo) && path_within(other, repo) && path_within(path, other)
        })
}

/// True for paths on a Windows drive mounted under `/mnt/`.
pub fn is_windows_mount(path: &str) -> bool {
    path.get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("/mnt/"))
}

fn trim_trailing_slash(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed,
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slashes_become_dashes() {
        assert_eq!(
            encode_project_dir("/home/dev/projects/tidepool-api"),
            "-home-dev-projects-tidepool-api"
        );
    }

    #[test]
    fn dots_spaces_and_underscores_become_dashes() {
        assert_eq!(
            encode_project_dir("/home/dev/.cache/plugin/1.2.0"),
            "-home-dev--cache-plugin-1-2-0"
        );
        assert_eq!(
            encode_project_dir("/mnt/c/Users/dev/Desktop/Course 101/hw_2"),
            "-mnt-c-Users-dev-Desktop-Course-101-hw-2"
        );
    }

    #[test]
    fn case_is_kept() {
        assert_eq!(
            encode_project_dir("/home/dev/AuroraNotes"),
            "-home-dev-AuroraNotes"
        );
    }

    #[test]
    fn non_ascii_characters_become_dashes() {
        assert_eq!(encode_project_dir("/home/dev/café"), "-home-dev-caf-");
    }

    #[test]
    fn trailing_slash_is_ignored() {
        assert_eq!(
            encode_project_dir("/home/dev/projects/aurora-notes/"),
            "-home-dev-projects-aurora-notes"
        );
        assert_eq!(encode_project_dir("/"), "-");
    }

    #[test]
    fn linux_paths_match_case_sensitively() {
        assert!(project_dir_matches(
            "-home-dev-projects-aurora-notes",
            "/home/dev/projects/aurora_notes"
        ));
        assert!(!project_dir_matches(
            "-home-dev-projects-Aurora-notes",
            "/home/dev/projects/aurora-notes"
        ));
        assert!(!paths_match("/home/dev/Aurora", "/home/dev/aurora"));
        assert!(paths_match("/home/dev/aurora/", "/home/dev/aurora"));
    }

    #[test]
    fn windows_mount_paths_match_ignoring_case() {
        assert!(project_dir_matches(
            "-mnt-c-users-dev-desktop-tidepool-api",
            "/mnt/c/Users/dev/Desktop/tidepool-api"
        ));
        assert!(paths_match(
            "/mnt/c/users/dev/desktop/tidepool-api",
            "/mnt/c/Users/dev/Desktop/tidepool-api"
        ));
        assert!(!paths_match(
            "/mnt/c/Users/dev/Desktop/tidepool-api",
            "/home/dev/Desktop/tidepool-api"
        ));
    }

    #[test]
    fn lossy_collisions_share_a_directory() {
        assert_eq!(
            encode_project_dir("/home/dev/a-b"),
            encode_project_dir("/home/dev/a_b")
        );
        assert!(!paths_match("/home/dev/a-b", "/home/dev/a_b"));
    }

    #[test]
    fn paths_within_a_directory() {
        let repo = "/home/dev/projects/tidepool-api";
        assert!(path_within(repo, repo));
        assert!(path_within("/home/dev/projects/tidepool-api/src/", repo));
        assert!(path_within(
            "/home/dev/projects/tidepool-api/src/a.rs",
            "/home/dev/projects/tidepool-api/"
        ));
        assert!(!path_within(
            "/home/dev/projects/tidepool-api-old/src",
            repo
        ));
        assert!(!path_within("/home/dev/projects", repo));
        assert!(!path_within("/home/dev/projects/Tidepool-api/src", repo));
        assert!(path_within(
            "/mnt/c/users/dev/tidepool-api/src",
            "/mnt/c/Users/dev/Tidepool-API"
        ));
        assert!(path_within("/home/dev", "/"));
    }

    #[test]
    fn paths_go_to_the_deepest_repo() {
        let main = "/home/dev/projects/tidepool-api";
        let nested = "/home/dev/projects/tidepool-api/.claude/worktrees/wide-cards";
        let sibling = "/home/dev/projects/tidepool-api-old";
        let others = [main, nested, sibling];
        assert!(within_deepest(main, main, &others));
        assert!(within_deepest(&format!("{main}/src"), main, &others));
        assert!(!within_deepest(nested, main, &others));
        assert!(!within_deepest(&format!("{nested}/src"), main, &others));
        assert!(within_deepest(&format!("{nested}/src"), nested, &others));
        assert!(!within_deepest(sibling, main, &others));
        // A repo around this one does not take its paths away.
        assert!(within_deepest(nested, nested, &["/home/dev/projects"]));
    }
}

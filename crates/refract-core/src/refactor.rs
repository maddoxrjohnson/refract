//! The pure parts of a one-button refactor (see `.plans/refactor-spec.md`): the prompts
//! its agent gets, the result file it writes, and how its worktree and branch are named.
//! The helper and its tests both use these, so the wording lives in one place.

use chrono::NaiveDateTime;
use serde::Deserialize;

use crate::protocol::CheckResult;

/// The result file's name, in the worktree's own git directory, where it is never
/// committed.
pub const RESULT_FILE: &str = "refract-refactor.json";

/// A result file larger than this is not read, and counts as invalid.
pub const MAX_RESULT_BYTES: u64 = 256 * 1024;

/// Where refactor worktrees go, relative to the home directory. A dot directory inside
/// `~/projects`, so VS Code trusts it and the repo search skips it.
pub const WORKTREES_DIR: &str = "projects/.refract-worktrees";

/// The prefix of every refactor branch.
pub const BRANCH_PREFIX: &str = "refactor/";

/// The terminal tab's name for a refactor agent.
pub const TITLE: &str = "Refactor";

/// The longest focus line kept, in characters.
pub const MAX_FOCUS_CHARS: usize = 500;

/// What a resume without `fix` sends the agent.
pub const CONTINUE_PROMPT: &str = "Continue the refactor from where it stopped.";

/// An example result file, as the prompt shows it.
pub const EXAMPLE_RESULT: &str = r#"{"checks":[{"command":"cargo test","before":"pass","after":"pass"},{"command":"cargo clippy -- -D warnings","before":"fail","after":"fail"}],"refactors":[{"title":"Split parse.rs into lexer and parser","commit":"a1b2c3d"}],"summary":"Split the parser and removed two unused helpers; all checks as before."}"#;

/// The opening prompt of a refactor agent working on `branch`, which writes its result
/// to `result_path` (absolute), with the user's optional `focus` line.
pub fn prompt(branch: &str, result_path: &str, focus: Option<&str>) -> String {
    let mut text = format!(
        "You are refactoring this repository in a git worktree on branch `{branch}` (your own \
copy; the user's working copy is elsewhere). Goal: up to 3 behaviour-preserving refactors with \
the best payoff (duplication, oversized files or functions, dead code, unclear names, tangled \
modules). No new features, no public API or behaviour changes, no dependency upgrades. Never \
push, and never merge or switch branches; the user merges from Refract.
1. Set up the project so its checks can run (install dependencies as the README/lockfiles say).
2. Find the test, lint and type-check commands and run them for a baseline.
3. Propose the plan: each refactor, why, files touched, risk. Wait for approval.
4. For each refactor: make it, re-run the checks, commit it alone with a clear message. If a \
check that passed in the baseline now fails and you can't fix it within the refactor, revert it.
5. Write `{result_path}` (it is outside the working tree and never committed) and finish with \
a short summary.

The file is JSON like this:
{EXAMPLE_RESULT}
\"before\" is a check's outcome in the baseline and \"after\" once you are done; each is \
\"pass\", \"fail\" or \"notRun\". Each refactor's \"commit\" is its commit's id. If nothing is \
worth refactoring, make no commits and write the file with an empty \"refactors\" list."
    );
    if let Some(focus) = focus.and_then(clean_focus) {
        text.push_str("\nFocus on: ");
        text.push_str(&focus);
    }
    text
}

/// What a resume with `fix` sends the agent: rebase `branch` onto `target` after a
/// merge hit conflicts, and update the result file at `result_path`.
pub fn fix_prompt(branch: &str, target: &str, result_path: &str) -> String {
    format!(
        "Merging `{branch}` into `{target}` stopped: rebasing it onto `{target}` hit conflicts, so \
the rebase was undone. Rebase `{branch}` onto `{target}` (`git rebase {target}`), resolving each \
conflict so both sides' intent is kept. Re-run the checks, keep one commit per refactor, and \
update `{result_path}`. Never push, and never merge or switch branches; the user merges from \
Refract."
    )
}

/// The focus line as the prompt takes it: one line, trimmed, at most
/// [`MAX_FOCUS_CHARS`] characters; `None` when nothing is left.
pub fn clean_focus(focus: &str) -> Option<String> {
    let line: String = focus
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FOCUS_CHARS)
        .collect();
    (!line.is_empty()).then_some(line)
}

/// What the agent reports in its result file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResultFile {
    pub checks: Vec<CheckResult>,
    pub refactors: Vec<ReportedRefactor>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// One refactor the agent reports.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReportedRefactor {
    pub title: String,
    #[serde(default)]
    pub commit: Option<String>,
}

/// Parses a result file. `None` when it is larger than [`MAX_RESULT_BYTES`], not JSON,
/// lacks `checks` or `refactors`, or names an outcome other than `pass`, `fail` and
/// `notRun`. Unknown fields are ignored.
pub fn parse_result(bytes: &[u8]) -> Option<ResultFile> {
    if bytes.len() as u64 > MAX_RESULT_BYTES {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// The time part of worktree and branch names: `20261031-1405`, from local time.
pub fn stamp(local: NaiveDateTime) -> String {
    local.format("%Y%m%d-%H%M").to_string()
}

/// `base` for the first try, then `base-2`, `base-3`, … for later ones (`n` from 1).
pub fn numbered(base: &str, n: u32) -> String {
    if n <= 1 {
        base.to_owned()
    } else {
        format!("{base}-{n}")
    }
}

/// The worktree folder's name for a refactor of `repo_name`, before numbering.
pub fn worktree_name(repo_name: &str, stamp: &str) -> String {
    format!("{repo_name}-{stamp}")
}

/// The branch name, before numbering.
pub fn branch_name(stamp: &str) -> String {
    format!("{BRANCH_PREFIX}{stamp}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CheckOutcome;

    const RESULT: &str = "/home/dev/projects/tidepool-api/.git/worktrees/x/refract-refactor.json";

    #[test]
    fn the_prompt_names_the_branch_the_result_file_and_the_focus() {
        let text = prompt("refactor/20261031-1405", RESULT, None);
        assert!(text.starts_with("You are refactoring this repository"));
        assert!(text.contains("on branch `refactor/20261031-1405`"));
        assert!(text.contains(&format!("5. Write `{RESULT}`")));
        assert!(text.contains(EXAMPLE_RESULT));
        assert!(text.contains("Wait for approval."));
        assert!(text.contains("Never push"));
        assert!(!text.contains("Focus on:"));

        let focused = prompt("refactor/x", RESULT, Some("  the parser\n  and its tests "));
        assert!(focused.ends_with("\nFocus on: the parser and its tests"));
        assert_eq!(
            prompt("refactor/x", RESULT, Some(" \n ")),
            prompt("refactor/x", RESULT, None)
        );
        let long = "a".repeat(2 * MAX_FOCUS_CHARS);
        let line = clean_focus(&long).unwrap();
        assert_eq!(line.chars().count(), MAX_FOCUS_CHARS);
    }

    #[test]
    fn the_fix_prompt_names_the_target() {
        let text = fix_prompt("refactor/x", "main", RESULT);
        assert!(text.contains("`git rebase main`"));
        assert!(text.contains("keep one commit per refactor"));
        assert!(text.contains(RESULT));
    }

    #[test]
    fn the_example_result_parses() {
        let result = parse_result(EXAMPLE_RESULT.as_bytes()).unwrap();
        assert_eq!(result.checks.len(), 2);
        assert_eq!(result.checks[0].before, CheckOutcome::Pass);
        assert_eq!(result.checks[1].after, CheckOutcome::Fail);
        assert_eq!(result.refactors[0].commit.as_deref(), Some("a1b2c3d"));
        assert!(result.summary.unwrap().starts_with("Split the parser"));
    }

    #[test]
    fn result_files_parse_strictly_and_ignore_unknown_fields() {
        let empty = parse_result(br#"{"checks":[],"refactors":[],"extra":1}"#).unwrap();
        assert!(empty.refactors.is_empty() && empty.summary.is_none());
        let not_run = parse_result(
            br#"{"checks":[{"command":"npm test","before":"notRun","after":"pass","ms":3}],"refactors":[{"title":"t"}]}"#,
        )
        .unwrap();
        assert_eq!(not_run.checks[0].before, CheckOutcome::NotRun);
        assert_eq!(not_run.refactors[0].commit, None);
        for bad in [
            &b""[..],
            b"not json",
            b"[]",
            br#"{"refactors":[]}"#,
            br#"{"checks":[]}"#,
            br#"{"checks":[{"command":"x","before":"passed","after":"pass"}],"refactors":[]}"#,
            br#"{"checks":[{"command":"x","before":"pass"}],"refactors":[]}"#,
        ] {
            assert_eq!(parse_result(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn a_huge_result_file_is_invalid() {
        let mut huge = br#"{"checks":[],"refactors":[],"summary":""#.to_vec();
        huge.resize(MAX_RESULT_BYTES as usize + 1, b'a');
        huge.extend_from_slice(br#""}"#);
        assert_eq!(parse_result(&huge), None);
        let mut fits = br#"{"checks":[],"refactors":[],"summary":""#.to_vec();
        fits.resize(MAX_RESULT_BYTES as usize - 2, b'a');
        fits.extend_from_slice(br#""}"#);
        assert!(parse_result(&fits).is_some());
    }

    #[test]
    fn names_carry_the_stamp_and_a_number() {
        let at = NaiveDateTime::parse_from_str("2026-10-31 14:05:59", "%Y-%m-%d %H:%M:%S").unwrap();
        let stamp = stamp(at);
        assert_eq!(stamp, "20261031-1405");
        assert_eq!(branch_name(&stamp), "refactor/20261031-1405");
        assert_eq!(
            worktree_name("tidepool-api", &stamp),
            "tidepool-api-20261031-1405"
        );
        assert_eq!(
            numbered("refactor/20261031-1405", 1),
            "refactor/20261031-1405"
        );
        assert_eq!(
            numbered("refactor/20261031-1405", 3),
            "refactor/20261031-1405-3"
        );
    }
}

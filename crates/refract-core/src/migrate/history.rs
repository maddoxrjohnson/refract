//! Moving Claude Code session history with the repo (spec §3.4).
//!
//! Transcripts live in `~/.claude/projects/<encoded cwd>/` (see
//! [`crate::sessions::encode_project_dir`]). They are copied to the folder for the new
//! path, and on each line the top-level `cwd`, when it is the old path or under it, is
//! rewritten to the new path. A session started in a subfolder has its own project
//! folder; [`moved_cwd`] gives its new working directory and so its new folder name.
//!
//! The rewrite is byte-preserving: only the bytes of the `cwd` value change, so key
//! order, number formatting and string escapes elsewhere stay exactly as written.
//! Under `/mnt/` the old path matches ignoring ASCII case, as everywhere else in
//! Refract ([`crate::sessions::path_within`]); the part below it keeps its case.

use std::borrow::Cow;

use serde_json::Value;

use crate::sessions::{encode_project_dir, path_within};

/// The project folder names for the old and the new working directory.
pub fn project_dirs(old_cwd: &str, new_cwd: &str) -> (String, String) {
    (encode_project_dir(old_cwd), encode_project_dir(new_cwd))
}

/// Where `cwd` lands when `old` moves to `new`: `new` plus whatever lies below `old`.
/// `None` when `cwd` is not `old` or under it, by whole path components
/// (`/a/foo-bar` is not under `/a/foo`).
pub fn moved_cwd(cwd: &str, old: &str, new: &str) -> Option<String> {
    if !path_within(cwd, old) {
        return None;
    }
    let old = trim_slash(old);
    let new = trim_slash(new);
    // path_within compared `old` against the start of `cwd` byte for byte (ignoring
    // ASCII case at most), so the rest starts at the same byte offset.
    let rest = cwd.get(old.len()..).unwrap_or("");
    Some(format!("{new}{rest}"))
}

fn trim_slash(path: &str) -> &str {
    match path.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed,
        _ => path,
    }
}

/// Rewrites one transcript line (without its line ending). Lines that are blank, or
/// JSON that is not an object or has no matching string `cwd`, come back unchanged.
/// Fails on a line that is not JSON.
pub fn rewrite_line<'a>(line: &'a str, old: &str, new: &str) -> Result<Cow<'a, str>, String> {
    if line.trim().is_empty() {
        return Ok(Cow::Borrowed(line));
    }
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
        return Ok(Cow::Borrowed(line));
    };
    let Some(moved) = moved_cwd(cwd, old, new) else {
        return Ok(Cow::Borrowed(line));
    };
    let Some((start, end)) = top_level_value_span(line, "cwd") else {
        return Err("could not find the cwd field".into());
    };
    let quoted = serde_json::to_string(&moved).map_err(|e| e.to_string())?;
    Ok(Cow::Owned(format!(
        "{}{quoted}{}",
        &line[..start],
        &line[end..]
    )))
}

/// Rewrites a whole transcript, line by line, keeping each line's ending (`\n` or
/// `\r\n`) and a missing final newline. Fails with the 1-based line number on a line
/// that is not JSON.
pub fn rewrite_transcript(text: &str, old: &str, new: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let (body, ending) = split_ending(line);
        let body = rewrite_line(body, old, new).map_err(|e| format!("line {}: {e}", n + 1))?;
        out.push_str(&body);
        out.push_str(ending);
    }
    Ok(out)
}

fn split_ending(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

/// Checks a rewritten transcript against the original: the same number of lines,
/// every non-blank line parses, and each line equals the original except for a
/// top-level `cwd` moved by [`moved_cwd`]. Fails with the first bad line's number.
pub fn verify_transcript(original: &str, copy: &str, old: &str, new: &str) -> Result<(), String> {
    let a: Vec<&str> = original.split_inclusive('\n').collect();
    let b: Vec<&str> = copy.split_inclusive('\n').collect();
    if a.len() != b.len() {
        return Err(format!(
            "the copy has {} lines, the original {}",
            b.len(),
            a.len()
        ));
    }
    for (n, (x, y)) in a.iter().zip(&b).enumerate() {
        let fail = |what: &str| Err(format!("line {}: {what}", n + 1));
        let (x, y) = (split_ending(x).0, split_ending(y).0);
        if x.trim().is_empty() || y.trim().is_empty() {
            if x != y {
                return fail("blank in only one of them");
            }
            continue;
        }
        let (Ok(mut expected), Ok(actual)) = (
            serde_json::from_str::<Value>(x),
            serde_json::from_str::<Value>(y),
        ) else {
            return fail("does not parse");
        };
        let moved = expected
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(|cwd| moved_cwd(cwd, old, new));
        if let (Some(moved), Some(object)) = (moved, expected.as_object_mut()) {
            object.insert("cwd".into(), Value::String(moved));
        }
        if expected != actual {
            return fail("differs in more than cwd");
        }
    }
    Ok(())
}

/// The byte range of the value of the last top-level `key` in a line holding one
/// valid JSON object, or `None` when there is none.
fn top_level_value_span(line: &str, key: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut found = None;
    loop {
        i = skip_ws(bytes, i);
        match bytes.get(i)? {
            b'}' => return found,
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let key_end = string_end(bytes, i)?;
        let name: String = serde_json::from_str(&line[i..key_end]).ok()?;
        i = skip_ws(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        let start = skip_ws(bytes, i + 1);
        let end = value_end(bytes, start)?;
        if name == key {
            found = Some((start, end));
        }
        i = end;
    }
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// The index just past the string starting at `i` (which holds `"`).
fn string_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match bytes.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// The index just past the value starting at `i`.
fn value_end(bytes: &[u8], i: usize) -> Option<usize> {
    match bytes.get(i)? {
        b'"' => string_end(bytes, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match bytes.get(j)? {
                    b'"' => {
                        j = string_end(bytes, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while !matches!(
                bytes.get(j),
                None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            ) {
                j += 1;
            }
            Some(j)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "/mnt/c/Users/dev/Desktop/aurora-notes";
    const NEW: &str = "/home/dev/projects/aurora-notes";

    #[test]
    fn cwd_is_rewritten_and_nothing_else_changes() {
        let line = r#"{"type":"user","cwd":"/mnt/c/Users/dev/Desktop/aurora-notes","n":1.50,"text":"café \/ \"cwd\":\"x\"","nested":{"cwd":"/mnt/c/Users/dev/Desktop/aurora-notes"}}"#;
        let out = rewrite_line(line, OLD, NEW).unwrap();
        assert_eq!(
            out,
            r#"{"type":"user","cwd":"/home/dev/projects/aurora-notes","n":1.50,"text":"café \/ \"cwd\":\"x\"","nested":{"cwd":"/mnt/c/Users/dev/Desktop/aurora-notes"}}"#
        );
    }

    #[test]
    fn subfolders_move_and_lookalike_folders_do_not() {
        assert_eq!(
            moved_cwd("/mnt/c/Users/dev/Desktop/aurora-notes/src/app", OLD, NEW).as_deref(),
            Some("/home/dev/projects/aurora-notes/src/app")
        );
        assert_eq!(moved_cwd("/a/foo-bar", "/a/foo", "/b/foo"), None);
        assert_eq!(moved_cwd("/a/foobar/x", "/a/foo", "/b/foo"), None);
        assert_eq!(moved_cwd("/a", "/a/foo", "/b/foo"), None);
        assert_eq!(
            moved_cwd("/a/foo", "/a/foo/", "/b/foo/").as_deref(),
            Some("/b/foo")
        );
        // Windows drives match ignoring case; the rest keeps its case.
        assert_eq!(
            moved_cwd("/mnt/c/users/dev/desktop/Aurora-Notes/Docs", OLD, NEW).as_deref(),
            Some("/home/dev/projects/aurora-notes/Docs")
        );
        let line = r#"{"cwd": "/a/foo-bar", "x": 1}"#;
        assert!(matches!(
            rewrite_line(line, "/a/foo", "/b/foo").unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn lines_without_a_matching_cwd_are_left_alone() {
        for line in [
            r#"{"type":"summary","summary":"Tide tables"}"#,
            r#"{"cwd":null}"#,
            r#"{"cwd":7}"#,
            r#"[1,2]"#,
            "   ",
            "",
        ] {
            assert_eq!(rewrite_line(line, OLD, NEW).unwrap(), line, "{line}");
        }
        assert!(rewrite_line(r#"{"cwd":"#, OLD, NEW).is_err());
    }

    #[test]
    fn spacing_escapes_and_duplicate_keys_are_handled() {
        // Spaces around the colon, an escaped key, and the last duplicate (the one
        // JSON parsers keep) is rewritten.
        let line = r#"{ "cwd" : "/a/foo/x" , "list":[{"a":"}"},2] , "cwd" : "/a/foo" }"#;
        let out = rewrite_line(line, "/a/foo", "/b/foo").unwrap();
        assert_eq!(
            out,
            r#"{ "cwd" : "/a/foo/x" , "list":[{"a":"}"},2] , "cwd" : "/b/foo" }"#
        );
        // New paths with characters JSON escapes come out escaped.
        let out = rewrite_line(r#"{"cwd":"/a/foo"}"#, "/a/foo", "/b/\"q\"").unwrap();
        assert_eq!(out, r#"{"cwd":"/b/\"q\""}"#);
    }

    fn transcript() -> String {
        [
            r#"{"type":"user","cwd":"/mnt/c/Users/dev/Desktop/aurora-notes","sessionId":"s1"}"#,
            r#"{"type":"summary","summary":"x"}"#,
            "",
            r#"{"type":"assistant","cwd":"/mnt/c/Users/dev/Desktop/aurora-notes/src","x":[1,2]}"#,
        ]
        .join("\n")
            + "\r\n"
    }

    #[test]
    fn whole_transcripts_rewrite_and_verify() {
        let original = transcript();
        let copy = rewrite_transcript(&original, OLD, NEW).unwrap();
        assert_eq!(copy.matches(NEW).count(), 2);
        assert!(!copy.contains(OLD));
        assert!(copy.ends_with("}\r\n"));
        assert_eq!(verify_transcript(&original, &copy, OLD, NEW), Ok(()));
        let unterminated = original.trim_end();
        let copy = rewrite_transcript(unterminated, OLD, NEW).unwrap();
        assert!(copy.ends_with('}'));
        assert_eq!(
            rewrite_transcript("{}\n{oops\n", OLD, NEW).unwrap_err(),
            "line 2: not JSON: key must be a string at line 1 column 2"
        );
    }

    #[test]
    fn verification_catches_other_changes() {
        let original = transcript();
        let good = rewrite_transcript(&original, OLD, NEW).unwrap();
        // A line dropped.
        let short: String = good.split_inclusive('\n').skip(1).collect();
        assert!(verify_transcript(&original, &short, OLD, NEW)
            .unwrap_err()
            .contains("lines"));
        // Another field changed.
        let edited = good.replace(r#""sessionId":"s1""#, r#""sessionId":"s2""#);
        assert_eq!(
            verify_transcript(&original, &edited, OLD, NEW),
            Err("line 1: differs in more than cwd".into())
        );
        // A cwd left unmoved.
        assert_eq!(
            verify_transcript(&original, &original, OLD, NEW),
            Err("line 1: differs in more than cwd".into())
        );
        // A line that no longer parses.
        let broken = good.replacen('}', "", 1);
        assert_eq!(
            verify_transcript(&original, &broken, OLD, NEW),
            Err("line 1: does not parse".into())
        );
    }

    #[test]
    fn project_folders_use_the_session_encoding() {
        assert_eq!(
            project_dirs(OLD, NEW),
            (
                "-mnt-c-Users-dev-Desktop-aurora-notes".to_string(),
                "-home-dev-projects-aurora-notes".to_string()
            )
        );
    }
}

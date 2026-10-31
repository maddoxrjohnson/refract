//! The optional status hook (spec §6.2): the status files its script writes, and the
//! entries the installer adds to and removes from Claude Code's `settings.json`.
//!
//! Claude Code's `settings.json` keeps hooks as
//! `"hooks": {"<Event>": [{"matcher": "...", "hooks": [{"type": "command", "command": "..."}]}]}`,
//! where `matcher` is left out for events that take none. The script is registered
//! once per event in [`EVENTS`], with the status and the status directory as its
//! arguments, and Refract's entries are recognised by the script path ([`SCRIPT_MARK`])
//! in their command, whichever form the command has.
//!
//! Install and uninstall are pure functions over the file's bytes, so install then
//! uninstall can give back the very same bytes. Keys keep their order (serde_json's
//! `preserve_order`), and an unchanged file is never rewritten.

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::classify::HookStatus;

/// One Claude Code hook event the script runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookEvent {
    /// The event's key under `hooks`.
    pub name: &'static str,
    /// The status the script writes for it.
    pub status: HookStatus,
    /// The matcher, for events that take one.
    pub matcher: Option<&'static str>,
}

/// The events the script is registered for. `PostToolUse` marks a turn as working
/// again after a permission prompt or a question was answered. `Notification` only
/// counts for permission prompts and MCP input requests; its `idle_prompt` (sent after
/// a minute at the prompt) would wrongly say the agent is blocked. `AskUserQuestion`
/// and `ExitPlanMode` wait on you without a `Notification`, so their `PreToolUse`
/// counts too. `SessionStart` leaves out `compact`, which happens mid-turn.
pub const EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "SessionStart",
        status: HookStatus::Finished,
        matcher: Some("startup|resume|clear"),
    },
    HookEvent {
        name: "UserPromptSubmit",
        status: HookStatus::Working,
        matcher: None,
    },
    HookEvent {
        name: "PostToolUse",
        status: HookStatus::Working,
        matcher: Some("*"),
    },
    HookEvent {
        name: "Notification",
        status: HookStatus::WaitingOnUser,
        matcher: Some("permission_prompt|elicitation_dialog"),
    },
    HookEvent {
        name: "PreToolUse",
        status: HookStatus::WaitingOnUser,
        matcher: Some("AskUserQuestion|ExitPlanMode"),
    },
    HookEvent {
        name: "Stop",
        status: HookStatus::Finished,
        matcher: None,
    },
    HookEvent {
        name: "SessionEnd",
        status: HookStatus::Ended,
        matcher: None,
    },
];

/// Part of the script's path that marks a hook command as Refract's.
pub const SCRIPT_MARK: &str = "/refract/status-hook.sh";

/// The argument the script takes for `status`, which is also how status files name it.
pub fn status_arg(status: HookStatus) -> &'static str {
    match status {
        HookStatus::Working => "working",
        HookStatus::WaitingOnUser => "waitingOnUser",
        HookStatus::Finished => "finished",
        HookStatus::Ended => "ended",
    }
}

/// `value` single-quoted for the shell.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The hook command for `status`, writing status files to `status_dir`:
/// `[ -f '<script>' ] && sh '<script>' <status> '<status_dir>' || cat >/dev/null; exit 0`.
/// With the script gone it drains its input and still exits 0 without a word, since
/// Claude Code takes exit code 2 (what `sh` gives for a missing file) as blocking the
/// prompt or the stop.
pub fn command(script: &str, status: HookStatus, status_dir: &str) -> String {
    let script = quote(script);
    format!(
        "[ -f {script} ] && sh {script} {} {} || cat >/dev/null; exit 0",
        status_arg(status),
        quote(status_dir)
    )
}

/// What one status file says:
/// `{"status":"working","event":"UserPromptSubmit","at":1773489600}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatusFile {
    pub status: HookStatus,
    /// The Claude Code event that wrote it.
    #[serde(default)]
    pub event: Option<String>,
    /// When, in Unix seconds.
    #[serde(default)]
    pub at: Option<i64>,
}

/// Parses a status file. `None` when it is not one (empty, cut short, unknown status).
pub fn parse_status_file(bytes: &[u8]) -> Option<StatusFile> {
    serde_json::from_slice(bytes).ok()
}

/// The session id a status file is named for: `<uuid>.json`. Temporary files
/// (`.<uuid>.<pid>.tmp`) and anything else give `None`.
pub fn status_file_session(name: &str) -> Option<&str> {
    name.strip_suffix(".json").filter(|id| is_uuid(id))
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Reads `settings.json` bytes as an object. Empty or blank bytes (a missing file) are
/// an empty object.
fn parse_settings(bytes: &[u8]) -> Result<Map<String, Value>, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice(bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("settings.json is not a JSON object".into()),
        Err(e) => Err(format!("settings.json is not valid JSON: {e}")),
    }
}

/// Serializes settings the way Claude Code writes them: two-space indent and a final
/// newline.
fn to_bytes(settings: Map<String, Value>) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(&Value::Object(settings)).expect("JSON value");
    out.push(b'\n');
    out
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(SCRIPT_MARK))
}

/// Number of Refract hook commands anywhere under `hooks`.
fn count_ours(settings: &Map<String, Value>) -> usize {
    let Some(Value::Object(hooks)) = settings.get("hooks") else {
        return 0;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks")?.as_array())
        .flatten()
        .filter(|hook| is_ours(hook))
        .count()
}

/// True when every event has exactly the entry [`add`] would make, and there are no
/// other Refract entries.
fn has_install(settings: &Map<String, Value>, script: &str, status_dir: &str) -> bool {
    let Some(Value::Object(hooks)) = settings.get("hooks") else {
        return false;
    };
    let all_present = EVENTS.iter().all(|event| {
        let wanted =
            json!({"type": "command", "command": command(script, event.status, status_dir)});
        hooks
            .get(event.name)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group.get("matcher").and_then(Value::as_str) == event.matcher
                        && group
                            .get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|list| list.contains(&wanted))
                })
            })
    });
    all_present && count_ours(settings) == EVENTS.len()
}

/// Removes every Refract hook command. A group, event list or `hooks` object left
/// empty by that is removed too; one that was already empty stays. Returns true when
/// something was removed.
fn strip(settings: &mut Map<String, Value>) -> bool {
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return false;
    };
    let mut removed = false;
    hooks.retain(|_, groups| {
        let Value::Array(groups) = groups else {
            return true;
        };
        let mut touched = false;
        groups.retain_mut(|group| {
            let Some(Value::Array(list)) = group.get_mut("hooks") else {
                return true;
            };
            let before = list.len();
            list.retain(|hook| !is_ours(hook));
            if list.len() == before {
                return true;
            }
            touched = true;
            !list.is_empty()
        });
        removed |= touched;
        !(touched && groups.is_empty())
    });
    if removed && hooks.is_empty() {
        settings.shift_remove("hooks");
    }
    removed
}

/// Appends one group per event, creating `hooks` and the event lists as needed.
fn add(settings: &mut Map<String, Value>, script: &str, status_dir: &str) -> Result<(), String> {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(hooks) = hooks else {
        return Err("\"hooks\" in settings.json is not an object".into());
    };
    for event in EVENTS {
        let groups = hooks
            .entry(event.name)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(groups) = groups else {
            return Err(format!(
                "\"hooks.{}\" in settings.json is not a list",
                event.name
            ));
        };
        let mut group = Map::new();
        if let Some(matcher) = event.matcher {
            group.insert("matcher".into(), matcher.into());
        }
        group.insert(
            "hooks".into(),
            json!([{"type": "command", "command": command(script, event.status, status_dir)}]),
        );
        groups.push(Value::Object(group));
    }
    Ok(())
}

/// True when `settings` (the bytes of `settings.json`, empty when it is missing) has
/// the hook installed for `script` and `status_dir`, exactly as [`install`] leaves it.
pub fn is_installed(settings: &[u8], script: &str, status_dir: &str) -> Result<bool, String> {
    Ok(has_install(&parse_settings(settings)?, script, status_dir))
}

/// True when `settings` holds any Refract hook command, installed fully or not.
pub fn has_entries(settings: &[u8]) -> Result<bool, String> {
    Ok(count_ours(&parse_settings(settings)?) > 0)
}

/// `settings.json` with the hook installed for `script`, writing status files to
/// `status_dir`. `settings` is empty when the file is missing. Every other key and hook
/// is kept, in order. Already installed, the bytes come back unchanged; a partial or
/// outdated install is replaced.
pub fn install(settings: &[u8], script: &str, status_dir: &str) -> Result<Vec<u8>, String> {
    let original = parse_settings(settings)?;
    if has_install(&original, script, status_dir) {
        return Ok(settings.to_vec());
    }
    // Stripping an install that was all of `hooks` removes the key and `add` appends
    // it again, so an update would move the block to the end; put it back in place.
    let at = original.keys().position(|key| key == "hooks");
    let mut next = original;
    strip(&mut next);
    add(&mut next, script, status_dir)?;
    if let Some(at) = at {
        next = with_key_at(next, "hooks", at);
    }
    Ok(to_bytes(next))
}

/// `map` with `key` moved to position `at` (or the end, when `at` is past it).
fn with_key_at(mut map: Map<String, Value>, key: &str, at: usize) -> Map<String, Value> {
    let Some(value) = map.shift_remove(key) else {
        return map;
    };
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.insert(at.min(entries.len()), (key.to_owned(), value));
    entries.into_iter().collect()
}

/// `settings` without empty event lists under `hooks`, and without `hooks` when that
/// leaves it empty.
fn without_empty_lists(mut settings: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::Object(hooks)) = settings.get_mut("hooks") {
        hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
        if hooks.is_empty() {
            settings.shift_remove("hooks");
        }
    }
    settings
}

/// `settings.json` as it was before the install, as the backup keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Original<'a> {
    /// There was no file.
    Missing,
    /// The file's bytes, possibly empty.
    Bytes(&'a [u8]),
}

/// What uninstalling leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uninstalled {
    /// Write these bytes.
    Write(Vec<u8>),
    /// There was no `settings.json` before the install and nothing else is in it now:
    /// remove the file.
    Remove,
}

/// `settings.json` with every Refract hook command removed. `original` is what the
/// backup saved before the install, when there is one. When what is left
/// equals the backup as JSON, not counting empty event lists (an empty list the user
/// had looks like one install created), the backup's bytes are given back verbatim, so install then
/// uninstall is byte-for-byte identical; otherwise anything edited since the install
/// is kept and the file is written in Claude Code's format. With no Refract entries
/// the bytes come back unchanged.
pub fn uninstall(settings: &[u8], original: Option<Original<'_>>) -> Result<Uninstalled, String> {
    let mut next = parse_settings(settings)?;
    if !strip(&mut next) {
        return Ok(Uninstalled::Write(settings.to_vec()));
    }
    if let Some(original) = original {
        let backup = match original {
            Original::Missing => &b""[..],
            Original::Bytes(bytes) => bytes,
        };
        if parse_settings(backup)
            .is_ok_and(|b| without_empty_lists(b) == without_empty_lists(next.clone()))
        {
            return Ok(match original {
                Original::Missing => Uninstalled::Remove,
                Original::Bytes(bytes) => Uninstalled::Write(bytes.to_vec()),
            });
        }
    }
    Ok(Uninstalled::Write(to_bytes(next)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "/home/dev/.local/share/refract/status-hook.sh";
    const DIR: &str = "/home/dev/.local/state/refract/sessions";
    const ID: &str = "3f6c1a52-0000-4000-8000-000000000001";

    /// A user's settings with other keys and their own hooks, formatted by hand.
    const USER: &str = r#"{
    "model": "opus",
    "permissions": { "allow": ["Bash(cargo test:*)"] },
    "hooks": {
        "UserPromptSubmit": [
            { "hooks": [ { "type": "command", "command": "python3 guard.py", "timeout": 5 } ] }
        ],
        "PreToolUse": [
            { "matcher": "Bash", "hooks": [ { "type": "command", "command": "lint.sh" } ] }
        ]
    },
    "theme": "dark"
}
"#;

    fn value(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    #[test]
    fn status_files_parse() {
        let file =
            parse_status_file(br#"{"status":"waitingOnUser","event":"Notification","at":17}"#)
                .unwrap();
        assert_eq!(file.status, HookStatus::WaitingOnUser);
        assert_eq!(file.event.as_deref(), Some("Notification"));
        assert_eq!(file.at, Some(17));
        assert_eq!(
            parse_status_file(b"{\"status\":\"ended\"}\n")
                .unwrap()
                .status,
            HookStatus::Ended
        );
        for bad in [
            &b""[..],
            b"{\"status\":\"work",
            b"{\"status\":\"busy\"}",
            b"[]",
            b"\xff",
        ] {
            assert_eq!(parse_status_file(bad), None);
        }
        for event in EVENTS {
            let line = format!(r#"{{"status":"{}"}}"#, status_arg(event.status));
            assert_eq!(
                parse_status_file(line.as_bytes()).unwrap().status,
                event.status
            );
        }
    }

    #[test]
    fn status_file_names_give_session_ids() {
        assert_eq!(status_file_session(&format!("{ID}.json")), Some(ID));
        assert_eq!(status_file_session(&format!(".{ID}.123.tmp")), None);
        assert_eq!(status_file_session(&format!("{ID}.json.tmp")), None);
        assert_eq!(status_file_session("notes.json"), None);
    }

    #[test]
    fn commands_quote_their_paths_and_always_succeed() {
        assert_eq!(
            command(SCRIPT, HookStatus::WaitingOnUser, DIR),
            format!(
                "[ -f '{SCRIPT}' ] && sh '{SCRIPT}' waitingOnUser '{DIR}' || cat >/dev/null; exit 0"
            )
        );
        assert_eq!(
            command(
                "/home/o'neil/x/refract/status-hook.sh",
                HookStatus::Working,
                "/s/o'neil"
            ),
            r"[ -f '/home/o'\''neil/x/refract/status-hook.sh' ] && sh '/home/o'\''neil/x/refract/status-hook.sh' working '/s/o'\''neil' || cat >/dev/null; exit 0"
        );
    }

    #[test]
    fn old_command_forms_are_recognised_and_replaced() {
        let old = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": format!("sh '{SCRIPT}' finished")}
        ]}]}});
        let old = serde_json::to_vec(&old).unwrap();
        assert!(has_entries(&old).unwrap());
        assert!(!is_installed(&old, SCRIPT, DIR).unwrap());
        let repaired = install(&old, SCRIPT, DIR).unwrap();
        assert!(is_installed(&repaired, SCRIPT, DIR).unwrap());
        // Another status directory is an outdated install too.
        assert!(!is_installed(&repaired, SCRIPT, "/elsewhere").unwrap());
        assert_eq!(
            uninstall(&old, None).unwrap(),
            Uninstalled::Write(b"{}\n".to_vec())
        );
    }

    /// `settings` with the six entries installs before `PreToolUse` was added.
    fn with_old_event_set(settings: &str) -> Vec<u8> {
        let mut v = value(settings.as_bytes());
        let hooks = v
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}));
        for event in EVENTS.iter().filter(|e| e.name != "PreToolUse") {
            let mut group = json!({"hooks": [
                {"type": "command", "command": command(SCRIPT, event.status, DIR)}
            ]});
            if let Some(matcher) = event.matcher {
                group["matcher"] = matcher.into();
            }
            let groups = hooks
                .as_object_mut()
                .unwrap()
                .entry(event.name)
                .or_insert_with(|| json!([]));
            groups.as_array_mut().unwrap().push(group);
        }
        to_bytes(v.as_object().unwrap().clone())
    }

    #[test]
    fn a_fresh_install_registers_every_event_in_order() {
        let v = value(&install(b"", SCRIPT, DIR).unwrap());
        assert_eq!(
            keys(&v["hooks"]),
            [
                "SessionStart",
                "UserPromptSubmit",
                "PostToolUse",
                "Notification",
                "PreToolUse",
                "Stop",
                "SessionEnd"
            ]
        );
        assert_eq!(
            v["hooks"]["PreToolUse"],
            json!([{"matcher": "AskUserQuestion|ExitPlanMode", "hooks": [
                {"type": "command", "command": command(SCRIPT, HookStatus::WaitingOnUser, DIR)}
            ]}])
        );
    }

    #[test]
    fn the_old_event_set_is_upgraded_in_place() {
        let old = with_old_event_set(USER);
        assert!(has_entries(&old).unwrap());
        assert!(!is_installed(&old, SCRIPT, DIR).unwrap());
        let upgraded = install(&old, SCRIPT, DIR).unwrap();
        assert!(is_installed(&upgraded, SCRIPT, DIR).unwrap());
        assert_eq!(install(&upgraded, SCRIPT, DIR).unwrap(), upgraded);
        // The old entries are replaced, not doubled, and the user's hooks stay.
        let settings = parse_settings(&upgraded).unwrap();
        assert_eq!(count_ours(&settings), EVENTS.len());
        let v = value(&upgraded);
        assert_eq!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(
            v["hooks"]["PreToolUse"][1]["matcher"],
            "AskUserQuestion|ExitPlanMode"
        );
        // The backup taken before the first install still restores the very bytes.
        assert_eq!(
            uninstall(&upgraded, Some(Original::Bytes(USER.as_bytes()))).unwrap(),
            Uninstalled::Write(USER.as_bytes().to_vec())
        );
        // Settings with no `PreToolUse` before the first install get none back.
        let bare = r#"{"theme":"dark"}"#;
        let upgraded = install(&with_old_event_set(bare), SCRIPT, DIR).unwrap();
        assert_eq!(
            uninstall(&upgraded, Some(Original::Bytes(bare.as_bytes()))).unwrap(),
            Uninstalled::Write(bare.as_bytes().to_vec())
        );
    }

    #[test]
    fn an_update_keeps_the_hooks_block_where_it_was() {
        // `hooks` holding only Refract entries, with a key added after it later.
        let old = with_old_event_set(r#"{"theme":"dark"}"#);
        let mut v = value(&old);
        v.as_object_mut()
            .unwrap()
            .insert("model".into(), "opus".into());
        let old = to_bytes(v.as_object().unwrap().clone());
        assert_eq!(keys(&value(&old)), ["theme", "hooks", "model"]);
        let upgraded = install(&old, SCRIPT, DIR).unwrap();
        assert_eq!(keys(&value(&upgraded)), ["theme", "hooks", "model"]);
        assert!(is_installed(&upgraded, SCRIPT, DIR).unwrap());
    }

    #[test]
    fn an_empty_settings_file_comes_back_empty() {
        let installed = install(b"", SCRIPT, DIR).unwrap();
        assert_eq!(
            uninstall(&installed, Some(Original::Bytes(b""))).unwrap(),
            Uninstalled::Write(Vec::new())
        );
    }

    #[test]
    fn install_keeps_other_keys_and_hooks_in_order() {
        let out = install(USER.as_bytes(), SCRIPT, DIR).unwrap();
        let v = value(&out);
        assert_eq!(keys(&v), ["model", "permissions", "hooks", "theme"]);
        assert_eq!(
            keys(&v["hooks"]),
            [
                "UserPromptSubmit",
                "PreToolUse",
                "SessionStart",
                "PostToolUse",
                "Notification",
                "Stop",
                "SessionEnd"
            ]
        );
        let prompt = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(prompt.len(), 2);
        assert_eq!(prompt[0]["hooks"][0]["command"], "python3 guard.py");
        assert_eq!(
            prompt[1],
            json!({"hooks": [{"type": "command", "command": command(SCRIPT, HookStatus::Working, DIR)}]})
        );
        assert_eq!(
            v["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|elicitation_dialog"
        );
        assert_eq!(
            v["hooks"]["SessionStart"][0]["matcher"],
            "startup|resume|clear"
        );
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(
            v["hooks"]["PreToolUse"][1],
            json!({"matcher": "AskUserQuestion|ExitPlanMode", "hooks": [
                {"type": "command", "command": command(SCRIPT, HookStatus::WaitingOnUser, DIR)}
            ]})
        );
        assert!(is_installed(&out, SCRIPT, DIR).unwrap());
        assert!(!is_installed(USER.as_bytes(), SCRIPT, DIR).unwrap());
        assert!(String::from_utf8(out).unwrap().ends_with("}\n"));
    }

    #[test]
    fn install_is_idempotent() {
        let once = install(USER.as_bytes(), SCRIPT, DIR).unwrap();
        assert_eq!(install(&once, SCRIPT, DIR).unwrap(), once);
        // A partial install (one entry removed by hand) is repaired, not doubled.
        let mut partial = value(&once);
        partial["hooks"]["Stop"] = json!([]);
        let partial = serde_json::to_vec_pretty(&partial).unwrap();
        assert!(!is_installed(&partial, SCRIPT, DIR).unwrap());
        let repaired = install(&partial, SCRIPT, DIR).unwrap();
        assert!(is_installed(&repaired, SCRIPT, DIR).unwrap());
        assert_eq!(
            count_ours(&parse_settings(&repaired).unwrap()),
            EVENTS.len()
        );
    }

    #[test]
    fn install_then_uninstall_is_byte_identical_via_backup() {
        let installed = install(USER.as_bytes(), SCRIPT, DIR).unwrap();
        assert_ne!(installed, USER.as_bytes());
        assert_eq!(
            uninstall(&installed, Some(Original::Bytes(USER.as_bytes()))).unwrap(),
            Uninstalled::Write(USER.as_bytes().to_vec())
        );
        // Without a backup the content is still the original's, reformatted.
        let Uninstalled::Write(bare) = uninstall(&installed, None).unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(value(&bare), value(USER.as_bytes()));
        assert_eq!(
            keys(&value(&bare)),
            ["model", "permissions", "hooks", "theme"]
        );
    }

    #[test]
    fn uninstall_drops_what_install_created_only() {
        let original = r#"{"theme":"dark","hooks":{"Stop":[],"PreCompact":[]}}"#;
        let installed = install(original.as_bytes(), SCRIPT, DIR).unwrap();
        // An empty list the user had and install did not touch stays.
        let Uninstalled::Write(out) = uninstall(&installed, None).unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(
            value(&out),
            json!({"theme": "dark", "hooks": {"PreCompact": []}})
        );
        // With the backup, even the emptied `Stop` list comes back exactly.
        assert_eq!(
            uninstall(&installed, Some(Original::Bytes(original.as_bytes()))).unwrap(),
            Uninstalled::Write(original.as_bytes().to_vec())
        );

        let bare = install(br#"{"theme":"dark"}"#, SCRIPT, DIR).unwrap();
        let Uninstalled::Write(out) = uninstall(&bare, None).unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(value(&out), json!({"theme": "dark"}));
    }

    #[test]
    fn a_user_edit_after_install_survives_uninstall() {
        let installed = install(USER.as_bytes(), SCRIPT, DIR).unwrap();
        let mut edited = value(&installed);
        edited["theme"] = "light".into();
        edited["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(json!({"hooks": [{"type": "command", "command": "notify-send done"}]}));
        let edited = serde_json::to_vec_pretty(&edited).unwrap();
        let Uninstalled::Write(out) =
            uninstall(&edited, Some(Original::Bytes(USER.as_bytes()))).unwrap()
        else {
            panic!("expected bytes");
        };
        let v = value(&out);
        assert_eq!(v["theme"], "light");
        assert_eq!(
            v["hooks"]["Stop"],
            json!([{"hooks": [{"type": "command", "command": "notify-send done"}]}])
        );
        assert_eq!(count_ours(&parse_settings(&out).unwrap()), 0);
        assert_eq!(
            keys(&v["hooks"]),
            ["UserPromptSubmit", "PreToolUse", "Stop"]
        );
    }

    #[test]
    fn a_hook_sharing_a_group_with_ours_is_kept() {
        let settings = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": command(SCRIPT, HookStatus::Finished, DIR)},
            {"type": "command", "command": "say done"}
        ]}]}});
        let bytes = serde_json::to_vec(&settings).unwrap();
        let Uninstalled::Write(out) = uninstall(&bytes, None).unwrap() else {
            panic!("expected bytes");
        };
        assert_eq!(
            value(&out),
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
        );
    }

    #[test]
    fn a_missing_file_is_created_and_removed_again() {
        let installed = install(b"", SCRIPT, DIR).unwrap();
        assert!(is_installed(&installed, SCRIPT, DIR).unwrap());
        assert_eq!(keys(&value(&installed)), ["hooks"]);
        assert_eq!(
            uninstall(&installed, Some(Original::Missing)).unwrap(),
            Uninstalled::Remove
        );
    }

    #[test]
    fn uninstall_without_our_entries_changes_nothing() {
        assert_eq!(
            uninstall(USER.as_bytes(), Some(Original::Bytes(b"{}"))).unwrap(),
            Uninstalled::Write(USER.as_bytes().to_vec())
        );
        assert!(!has_entries(USER.as_bytes()).unwrap());
    }

    #[test]
    fn broken_settings_are_refused() {
        assert!(install(b"{\"model\": ", SCRIPT, DIR).is_err());
        assert!(install(b"[1, 2]", SCRIPT, DIR).is_err());
        assert!(install(br#"{"hooks": []}"#, SCRIPT, DIR).is_err());
        assert!(install(br#"{"hooks": {"Stop": {}}}"#, SCRIPT, DIR).is_err());
        assert!(uninstall(b"{oops", None).is_err());
    }
}

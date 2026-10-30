//! Plan-limit percentages from Claude Code's status line (spec §3.5, Claude usage
//! widget).
//!
//! Claude Code gives the status line command a `rate_limits` object on stdin, with
//! `five_hour` and `seven_day` windows that each hold `used_percentage` and
//! `resets_at`. It writes them nowhere itself, so a status line script that wants
//! Refract to show them relays them into a small JSON file:
//!
//! ```json
//! {"updated": 1793361600.5,
//!  "five_hour": {"used": 2.0, "resets_at": 1793370000},
//!  "seven_day": {"used": 6.0, "resets_at": 1793800000}}
//! ```
//!
//! `updated` is when the script last ran and `resets_at` when the window resets, both
//! in Unix seconds (a fraction is allowed); `used` is a percentage. Either window may
//! be `null` or left out. A window whose `used` is `null` or missing is left out too,
//! as is one whose `resets_at` is, since without it a stale percentage cannot be told
//! from a current one. Anything else is taken as no reading at all: text that is not
//! JSON, a missing or unreadable `updated`, a window that is not an object, a
//! percentage that is not a number or is negative or over 1000, or a time that is not
//! a number of seconds (milliseconds are refused as out of range). Unknown keys are ignored. The numbers are never estimated: no file, no
//! percentages.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The largest percentage taken as a reading; anything above is a broken file.
pub const MAX_PERCENT: f64 = 1000.0;

/// One window's reading.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    /// How much of the window's limit was used, as a percentage.
    pub used_percent: f64,
    /// When the window resets.
    pub resets_at: DateTime<Utc>,
}

/// The plan limits as the status line last saw them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanLimits {
    /// The rolling 5-hour window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<LimitWindow>,
    /// The weekly (7-day) window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<LimitWindow>,
    /// When the status line wrote the reading.
    pub updated_at: DateTime<Utc>,
}

/// Reads the status line's file (see the module docs). `None` when it is malformed.
pub fn parse(text: &str) -> Option<PlanLimits> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let updated_at = unix_time(object.get("updated")?)?;
    Some(PlanLimits {
        five_hour: window(object.get("five_hour"))?,
        seven_day: window(object.get("seven_day"))?,
        updated_at,
    })
}

/// A window: `Some(None)` when it is absent or `null`, `None` when it is malformed.
fn window(value: Option<&Value>) -> Option<Option<LimitWindow>> {
    let object = match value {
        None | Some(Value::Null) => return Some(None),
        Some(value) => value.as_object()?,
    };
    let used = match object.get("used") {
        None | Some(Value::Null) => return Some(None),
        Some(value) => value.as_f64()?,
    };
    if !used.is_finite() || !(0.0..=MAX_PERCENT).contains(&used) {
        return None;
    }
    let resets_at = match object.get("resets_at") {
        None | Some(Value::Null) => return Some(None),
        Some(value) => unix_time(value)?,
    };
    Some(Some(LimitWindow {
        used_percent: used,
        resets_at,
    }))
}

/// Unix seconds, possibly with a fraction, to a time; `None` when out of range, which
/// includes milliseconds (anything past the year 33658).
fn unix_time(value: &Value) -> Option<DateTime<Utc>> {
    let secs = value.as_f64()?;
    if !secs.is_finite() || secs < 0.0 || secs > 1e12 {
        return None;
    }
    DateTime::from_timestamp_millis((secs * 1000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn reads_the_status_line_file() {
        let limits = parse(
            r#"{"updated": 1793361600.5, "five_hour": {"used": 2.0, "resets_at": 1793370000},
                "seven_day": {"used": 6, "resets_at": 1793800000}, "note": "ignored"}"#,
        )
        .unwrap();
        assert_eq!(
            limits,
            PlanLimits {
                five_hour: Some(LimitWindow {
                    used_percent: 2.0,
                    resets_at: at(1_793_370_000),
                }),
                seven_day: Some(LimitWindow {
                    used_percent: 6.0,
                    resets_at: at(1_793_800_000),
                }),
                updated_at: DateTime::from_timestamp_millis(1_793_361_600_500).unwrap(),
            }
        );
    }

    #[test]
    fn either_window_may_be_missing_or_null() {
        let limits = parse(
            r#"{"updated": 1793361600, "five_hour": null, "seven_day": {"used": 100, "resets_at": 1793800000}}"#,
        )
        .unwrap();
        assert_eq!(limits.five_hour, None);
        assert_eq!(limits.seven_day.unwrap().used_percent, 100.0);
        // No reset time: that window is left out, not the whole reading.
        let unknown_reset = parse(
            r#"{"updated": 1793361600, "five_hour": {"used": 4, "resets_at": null},
                "seven_day": {"used": 9, "resets_at": 1793800000}}"#,
        )
        .unwrap();
        assert_eq!(unknown_reset.five_hour, None);
        assert_eq!(unknown_reset.seven_day.unwrap().used_percent, 9.0);
        // No percentage: likewise.
        for five_hour in [
            r#"{"used": null, "resets_at": 1793370000}"#,
            r#"{"resets_at": 1793370000}"#,
        ] {
            let text = format!(
                r#"{{"updated": 1793361600, "five_hour": {five_hour}, "seven_day": {{"used": 9, "resets_at": 1793800000}}}}"#
            );
            let limits = parse(&text).unwrap();
            assert_eq!(limits.five_hour, None, "{text}");
            assert!(limits.seven_day.is_some());
        }
        let neither = parse(r#"{"updated": 1793361600}"#).unwrap();
        assert_eq!((neither.five_hour, neither.seven_day), (None, None));
        assert_eq!(neither.updated_at, at(1_793_361_600));
    }

    #[test]
    fn malformed_files_give_nothing() {
        for text in [
            "",
            "{",
            "[]",
            "null",
            r#"{"five_hour": {"used": 2, "resets_at": 1793370000}}"#,
            r#"{"updated": "yesterday"}"#,
            r#"{"updated": -5}"#,
            r#"{"updated": 1793361600, "five_hour": 3}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": -1, "resets_at": 1793370000}}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": 1000.5, "resets_at": 1793370000}}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": 1e999, "resets_at": 1793370000}}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": "2", "resets_at": 1793370000}}"#,
            r#"{"updated": true}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": true, "resets_at": 1793370000}}"#,
            r#"{"updated": 1793361600000}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": 2, "resets_at": 1793370000000}}"#,
            r#"{"updated": 1793361600, "five_hour": {"used": 2, "resets_at": "soon"}}"#,
            r#"{"updated": 1793361600, "seven_day": {"used": 2, "resets_at": 1e300}}"#,
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
        // The bounds themselves are readings.
        let edge = r#"{"updated": 0, "five_hour": {"used": 0, "resets_at": 0},
                       "seven_day": {"used": 1000, "resets_at": 0}}"#;
        assert_eq!(parse(edge).unwrap().seven_day.unwrap().used_percent, 1000.0);
    }

    #[test]
    fn serializes_with_iso_times_and_leaves_out_missing_windows() {
        let limits = PlanLimits {
            five_hour: Some(LimitWindow {
                used_percent: 37.0,
                resets_at: "2026-03-14T20:30:00Z".parse().unwrap(),
            }),
            seven_day: None,
            updated_at: "2026-03-14T18:00:00Z".parse().unwrap(),
        };
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "fiveHour": {"usedPercent": 37.0, "resetsAt": "2026-03-14T20:30:00Z"},
                "updatedAt": "2026-03-14T18:00:00Z"})
        );
        assert_eq!(serde_json::from_value::<PlanLimits>(json).unwrap(), limits);
    }
}

//! Token usage totals from Claude Code transcripts (spec §3.5, Claude usage widget).
//!
//! Every API response is an `assistant` entry whose `message.usage` holds its token
//! counts. As noted in [`crate::transcript`], one response is written as several lines
//! that repeat the same `message.id`, `requestId` and usage, so records are deduplicated
//! by `message.id` (falling back to `requestId`). Checked against real transcripts
//! (Claude Code 2.x, 2026), the same message can also appear in two files, for example
//! a session and its fork, so [`summarize`] deduplicates across files as well.
//!
//! - Sidechain entries and subagent transcripts count: they are real cost.
//! - Synthetic API error messages carry zero usage and add nothing; records whose four
//!   counts are all zero are not kept.
//! - Entries without `message.usage` or without a readable `timestamp` are skipped.
//!
//! Windows include their start and run to `now`: the rolling 5 hours and 7 days start
//! exactly 5 h and 7 × 24 h before `now`, and today starts at local midnight.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, FixedOffset, NaiveTime, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::sessions::path_within;

/// Length of the rolling short window.
pub const FIVE_HOURS: TimeDelta = TimeDelta::hours(5);

/// Length of the rolling long window, and how long records are kept.
pub const SEVEN_DAYS: TimeDelta = TimeDelta::days(7);

/// Token counts, as the widget shows them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    /// `cache_read_input_tokens`.
    pub cache_read: u64,
    /// `cache_creation_input_tokens`.
    pub cache_write: u64,
}

impl TokenTotals {
    /// All four counts added up.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, other: TokenTotals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    fn max(self, other: TokenTotals) -> TokenTotals {
        TokenTotals {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_read: self.cache_read.max(other.cache_read),
            cache_write: self.cache_write.max(other.cache_write),
        }
    }
}

/// One repo's share of the last 7 days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoUsage {
    /// The repo's path, or the session's working directory when it matches no repo.
    pub path: String,
    pub name: String,
    pub last_7_days: TokenTotals,
}

/// The result of `usage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    /// Since local midnight.
    pub today: TokenTotals,
    pub last_7_days: TokenTotals,
    /// Rolling, ending now.
    pub last_5_hours: TokenTotals,
    /// Running `claude` sessions.
    pub live_agents: u32,
    /// Heaviest first.
    pub by_repo: Vec<RepoUsage>,
    pub computed_at: DateTime<Utc>,
    /// False until the helper has read the transcripts once; the totals are zeros
    /// until then, not real ones.
    pub ready: bool,
}

/// The usage of one API response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    /// `message.id`, else `requestId`; `None` when the entry has neither, and then the
    /// record is never deduplicated.
    pub key: Option<String>,
    /// The entry's timestamp; for a repeated message, that of its first line.
    pub timestamp: DateTime<Utc>,
    pub tokens: TokenTotals,
}

/// Collects usage records from one transcript, line by line, so a growing file can be
/// fed only its new lines.
#[derive(Debug, Clone, Default)]
pub struct UsageParser {
    records: Vec<UsageRecord>,
    /// Index into `records` of each key.
    seen: HashMap<String, usize>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

impl UsageParser {
    /// Reads one line. Lines that are not JSON, or not an `assistant` entry with usage
    /// and a timestamp, add nothing. Once the session's cwd is known, lines that do not
    /// mention `"usage"` are skipped without parsing, which keeps a first read of large
    /// transcripts cheap.
    pub fn push_line(&mut self, line: &str) {
        if self.cwd.is_some() && !line.contains("\"usage\"") {
            return;
        }
        let Ok(entry) = serde_json::from_str::<Line>(line.trim()) else {
            return;
        };
        if self.cwd.is_none() {
            self.cwd = entry.cwd;
        }
        if entry.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(message) = entry.message else { return };
        let Some(usage) = message.usage else { return };
        let Some(timestamp) = entry.timestamp.as_deref().and_then(parse_timestamp) else {
            return;
        };
        let tokens = TokenTotals {
            input: usage.input_tokens.unwrap_or(0),
            output: usage.output_tokens.unwrap_or(0),
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
        };
        let key = message.id.or(entry.request_id);
        if let Some(&i) = key.as_ref().and_then(|k| self.seen.get(k)) {
            // Repeats carry the same usage; taking the larger count is safe either way.
            let record = &mut self.records[i];
            record.tokens = record.tokens.max(tokens);
            return;
        }
        if tokens.total() == 0 {
            return;
        }
        if let Some(key) = &key {
            self.seen.insert(key.clone(), self.records.len());
        }
        self.records.push(UsageRecord {
            key,
            timestamp,
            tokens,
        });
    }

    /// The first `cwd` read: where the session started.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Records so far, in file order, one per message.
    pub fn records(&self) -> &[UsageRecord] {
        &self.records
    }

    /// Drops records older than `cutoff`.
    pub fn prune(&mut self, cutoff: DateTime<Utc>) {
        let before = self.records.len();
        self.records.retain(|r| r.timestamp >= cutoff);
        if self.records.len() != before {
            self.seen = self
                .records
                .iter()
                .enumerate()
                .filter_map(|(i, r)| Some((r.key.clone()?, i)))
                .collect();
        }
    }
}

/// Parses a whole transcript held in memory.
pub fn parse_str(text: &str) -> UsageParser {
    let mut parser = UsageParser::default();
    for line in text.lines() {
        parser.push_line(line);
    }
    parser
}

/// Adds up records into windows ending at `now`. `files` holds each transcript's
/// starting cwd with its records; a message in several files counts once, for the
/// first file that has it. `today_start` is local midnight as an instant (see
/// [`local_midnight`]). A file's records go to the deepest repo in `repos` holding its
/// cwd, else to the cwd itself; files with no cwd count only in the totals. The result
/// is marked ready; a caller that has not read the transcripts yet clears it.
pub fn summarize<'a, I>(
    files: I,
    repos: &[String],
    now: DateTime<Utc>,
    today_start: DateTime<Utc>,
    live_agents: u32,
) -> UsageSummary
where
    I: IntoIterator<Item = (Option<&'a str>, &'a [UsageRecord])>,
{
    let week_start = now - SEVEN_DAYS;
    let five_start = now - FIVE_HOURS;
    let mut summary = UsageSummary {
        today: TokenTotals::default(),
        last_7_days: TokenTotals::default(),
        last_5_hours: TokenTotals::default(),
        live_agents,
        by_repo: Vec::new(),
        computed_at: now,
        ready: true,
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut owners: HashMap<&str, TokenTotals> = HashMap::new();
    for (cwd, records) in files {
        let mut file_total = TokenTotals::default();
        for record in records {
            if record.timestamp < week_start {
                continue;
            }
            if let Some(key) = &record.key {
                if !seen.insert(key) {
                    continue;
                }
            }
            file_total.add(record.tokens);
            if record.timestamp >= today_start {
                summary.today.add(record.tokens);
            }
            if record.timestamp >= five_start {
                summary.last_5_hours.add(record.tokens);
            }
        }
        summary.last_7_days.add(file_total);
        if let Some(cwd) = cwd.filter(|_| file_total.total() > 0) {
            let owner = owning_repo(cwd, repos).unwrap_or(cwd);
            owners.entry(owner).or_default().add(file_total);
        }
    }
    summary.by_repo = owners
        .into_iter()
        .map(|(path, totals)| RepoUsage {
            path: path.to_owned(),
            name: folder_name(path).to_owned(),
            last_7_days: totals,
        })
        .collect();
    summary.by_repo.sort_by(|a, b| {
        b.last_7_days
            .total()
            .cmp(&a.last_7_days.total())
            .then_with(|| a.path.cmp(&b.path))
    });
    summary
}

/// The deepest repo that holds `cwd`.
fn owning_repo<'a>(cwd: &str, repos: &'a [String]) -> Option<&'a str> {
    repos
        .iter()
        .filter(|repo| path_within(cwd, repo))
        .max_by_key(|repo| repo.len())
        .map(String::as_str)
}

fn folder_name(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

/// The start of the day holding `now`, at a fixed UTC offset.
pub fn local_midnight(now: DateTime<Utc>, offset: FixedOffset) -> DateTime<Utc> {
    let day = now.with_timezone(&offset).date_naive();
    let midnight = day.and_time(NaiveTime::MIN);
    match offset.from_local_datetime(&midnight).single() {
        Some(t) => t.with_timezone(&Utc),
        // A fixed offset maps every local time to exactly one instant.
        None => now,
    }
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, ts: &str, input: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/home/dev/projects/kelp-cli","requestId":"req_{id}","message":{{"id":"{id}","usage":{{"input_tokens":{input},"output_tokens":1}}}}}}"#
        )
    }

    #[test]
    fn repeats_of_a_message_count_once_across_pushes() {
        let mut parser = UsageParser::default();
        parser.push_line(&line("msg_1", "2026-03-14T10:00:00Z", 5));
        parser.push_line(&line("msg_1", "2026-03-14T10:00:01Z", 5));
        parser.push_line(&line("msg_2", "2026-03-14T10:00:02Z", 7));
        parser.push_line(&line("msg_1", "2026-03-14T10:00:03Z", 6));
        let records = parser.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tokens.input, 6);
        assert_eq!(
            records[0].timestamp,
            "2026-03-14T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(parser.cwd(), Some("/home/dev/projects/kelp-cli"));
    }

    #[test]
    fn prune_drops_old_records_and_their_keys() {
        let mut parser = UsageParser::default();
        parser.push_line(&line("msg_1", "2026-03-01T10:00:00Z", 5));
        parser.push_line(&line("msg_2", "2026-03-14T10:00:00Z", 7));
        parser.prune("2026-03-07T00:00:00Z".parse().unwrap());
        assert_eq!(parser.records().len(), 1);
        parser.push_line(&line("msg_2", "2026-03-14T10:00:01Z", 7));
        assert_eq!(parser.records().len(), 1);
    }

    #[test]
    fn midnight_at_fixed_offsets() {
        let now: DateTime<Utc> = "2026-03-14T18:00:00Z".parse().unwrap();
        let east = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            local_midnight(now, east),
            "2026-03-13T22:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        let early: DateTime<Utc> = "2026-03-14T03:00:00Z".parse().unwrap();
        let west = FixedOffset::west_opt(5 * 3600).unwrap();
        assert_eq!(
            local_midnight(early, west),
            "2026-03-13T05:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            local_midnight(now, FixedOffset::east_opt(0).unwrap()),
            "2026-03-14T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn deepest_repo_owns_a_cwd() {
        let repos = vec![
            "/home/dev/projects".to_owned(),
            "/home/dev/projects/tidepool-api".to_owned(),
        ];
        assert_eq!(
            owning_repo("/home/dev/projects/tidepool-api/src", &repos),
            Some("/home/dev/projects/tidepool-api")
        );
        assert_eq!(
            owning_repo("/home/dev/projects/kelp-cli", &repos),
            Some("/home/dev/projects")
        );
        assert_eq!(owning_repo("/tmp", &repos), None);
        assert_eq!(folder_name("/home/dev/scratch/"), "scratch");
        assert_eq!(folder_name("/"), "/");
    }
}

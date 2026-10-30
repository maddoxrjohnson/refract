//! Usage totals over invented transcripts whose totals were summed by hand (spec §9).
//!
//! The fixtures give each message counts of the form input 10^e, output 2·10^e, cache
//! read 3·10^e and cache write 4·10^e, with a different `e` per message, so each digit
//! of a total says whether one message was counted.
//!
//! `usage_windows.jsonl` (tidepool-api), with now = 2026-03-14T18:00:00Z at UTC+2, so
//! local midnight is 2026-03-13T22:00:00Z, the 5 h window starts at 13:00:00Z and the
//! 7 d window at 2026-03-07T18:00:00Z:
//!
//! | msg | e | timestamp            | notes                          | 5h | today | 7d |
//! |-----|---|----------------------|--------------------------------|----|-------|----|
//! | A   | 9 | 03-07 17:59:59.999   | just outside 7 d               |    |       |    |
//! | B   | 0 | 03-07 18:00:00.000   | 7 d start, inclusive           |    |       | x  |
//! | C   | 1 | 03-13 21:59:59.999   | just before local midnight     |    |       | x  |
//! | D   | 2 | 03-13 22:00:00.000   | local midnight; three lines    |    | x     | x  |
//! | E   | 3 | 03-14 12:59:59.999   | just outside 5 h               |    | x     | x  |
//! | F   | 4 | 03-14 13:00:00.000   | 5 h start; sidechain           | x  | x     | x  |
//! | -   | - | 03-14 15:00          | synthetic API error, zero      |    |       |    |
//! | H   | - | 03-14 16:00          | no usage                       |    |       |    |
//! | -   | - | 03-14 16:30          | truncated line                 |    |       |    |
//! | I   | 5 | 03-14 17:30:00       | requestId only; two lines      | x  | x     | x  |
//!
//! `usage_fork.jsonl` (cwd aurora-notes/web): J (e = 6, 03-14 17:00, all windows) and a
//! copy of I. `usage_scratch.jsonl` (cwd /home/dev/scratch, no repo): K (e = 7,
//! 03-10 09:00, 7 d only).

use chrono::{DateTime, FixedOffset, Utc};
use refract_core::transcript;
use refract_core::usage::{
    local_midnight, parse_str, summarize, RepoUsage, TokenTotals, UsageParser,
};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn now() -> DateTime<Utc> {
    "2026-03-14T18:00:00Z".parse().unwrap()
}

fn today_start() -> DateTime<Utc> {
    local_midnight(now(), FixedOffset::east_opt(2 * 3600).unwrap())
}

/// Input 10^e summed over `e` in `exps`, output twice that, and so on.
fn digits(exps: &[u32]) -> TokenTotals {
    let input: u64 = exps.iter().map(|e| 10u64.pow(*e)).sum();
    TokenTotals {
        input,
        output: 2 * input,
        cache_read: 3 * input,
        cache_write: 4 * input,
    }
}

fn repos() -> Vec<String> {
    vec![
        "/home/dev/projects/tidepool-api".into(),
        "/home/dev/projects/aurora-notes".into(),
        "/home/dev/projects/kelp-cli".into(),
    ]
}

#[test]
fn windows_file_records_are_deduplicated() {
    let parser = parse_str(&fixture("usage_windows.jsonl"));
    assert_eq!(parser.cwd(), Some("/home/dev/projects/tidepool-api"));
    let keys: Vec<&str> = parser
        .records()
        .iter()
        .map(|r| r.key.as_deref().unwrap())
        .collect();
    assert_eq!(
        keys,
        ["msg_A", "msg_B", "msg_C", "msg_D", "msg_E", "msg_F", "req_I"]
    );
    let d = &parser.records()[3];
    assert_eq!(d.tokens, digits(&[2]));
    assert_eq!(d.timestamp, today_start());
}

#[test]
fn one_file_sums_by_window() {
    let parser = parse_str(&fixture("usage_windows.jsonl"));
    let summary = summarize(
        [(parser.cwd(), parser.records())],
        &repos(),
        now(),
        today_start(),
        2,
    );
    assert_eq!(summary.last_7_days, digits(&[0, 1, 2, 3, 4, 5]));
    assert_eq!(
        summary.last_7_days,
        TokenTotals {
            input: 111_111,
            output: 222_222,
            cache_read: 333_333,
            cache_write: 444_444,
        }
    );
    assert_eq!(summary.today, digits(&[2, 3, 4, 5]));
    assert_eq!(summary.last_5_hours, digits(&[4, 5]));
    assert_eq!(summary.live_agents, 2);
    assert_eq!(summary.computed_at, now());
    assert_eq!(
        summary.by_repo,
        [RepoUsage {
            path: "/home/dev/projects/tidepool-api".into(),
            name: "tidepool-api".into(),
            last_7_days: digits(&[0, 1, 2, 3, 4, 5]),
        }]
    );
}

#[test]
fn several_files_sum_once_per_message_and_split_by_repo() {
    let files: Vec<UsageParser> = [
        "usage_windows.jsonl",
        "usage_fork.jsonl",
        "usage_scratch.jsonl",
    ]
    .iter()
    .map(|name| parse_str(&fixture(name)))
    .collect();
    let summary = summarize(
        files.iter().map(|p| (p.cwd(), p.records())),
        &repos(),
        now(),
        today_start(),
        0,
    );
    assert_eq!(
        summary.last_7_days,
        TokenTotals {
            input: 11_111_111,
            output: 22_222_222,
            cache_read: 33_333_333,
            cache_write: 44_444_444,
        }
    );
    assert_eq!(summary.today, digits(&[2, 3, 4, 5, 6]));
    assert_eq!(
        summary.last_5_hours,
        TokenTotals {
            input: 1_110_000,
            output: 2_220_000,
            cache_read: 3_330_000,
            cache_write: 4_440_000,
        }
    );
    // Heaviest first; I counts for the first file that has it (tidepool-api).
    let repos: Vec<(&str, &str, u64)> = summary
        .by_repo
        .iter()
        .map(|r| (r.path.as_str(), r.name.as_str(), r.last_7_days.total()))
        .collect();
    assert_eq!(
        repos,
        [
            ("/home/dev/scratch", "scratch", 100_000_000),
            (
                "/home/dev/projects/aurora-notes",
                "aurora-notes",
                10_000_000
            ),
            ("/home/dev/projects/tidepool-api", "tidepool-api", 1_111_110),
        ]
    );

    // Another order moves I between repos but never changes the totals.
    let reversed = summarize(
        files.iter().rev().map(|p| (p.cwd(), p.records())),
        &self::repos(),
        now(),
        today_start(),
        0,
    );
    assert_eq!(reversed.last_7_days, summary.last_7_days);
    assert_eq!(reversed.today, summary.today);
    assert_eq!(reversed.last_5_hours, summary.last_5_hours);
    assert_eq!(reversed.by_repo[1].last_7_days, digits(&[5, 6]));
}

#[test]
fn a_file_fed_in_pieces_gives_the_same_records() {
    let text = fixture("usage_windows.jsonl");
    let whole = parse_str(&text);
    let mut pieces = UsageParser::default();
    for line in text.lines() {
        pieces.push_line(line);
    }
    assert_eq!(pieces.records(), whole.records());
}

#[test]
fn a_moving_now_moves_the_windows() {
    let parser = parse_str(&fixture("usage_windows.jsonl"));
    // One millisecond later B leaves the 7 days; at UTC midnight C and D are not today.
    let later: DateTime<Utc> = "2026-03-14T18:00:00.001Z".parse().unwrap();
    let utc_midnight = local_midnight(later, FixedOffset::east_opt(0).unwrap());
    let summary = summarize(
        [(parser.cwd(), parser.records())],
        &[],
        later,
        utc_midnight,
        0,
    );
    assert_eq!(summary.last_7_days, digits(&[1, 2, 3, 4, 5]));
    assert_eq!(summary.today, digits(&[3, 4, 5]));
    // No repo matches, so the session's cwd stands in.
    assert_eq!(summary.by_repo[0].path, "/home/dev/projects/tidepool-api");
}

#[test]
fn usage_agrees_with_the_transcript_summary() {
    let text = fixture("usage_dedupe.jsonl");
    let parser = parse_str(&text);
    let mut sum = TokenTotals::default();
    for record in parser.records() {
        sum.input += record.tokens.input;
        sum.output += record.tokens.output;
        sum.cache_read += record.tokens.cache_read;
        sum.cache_write += record.tokens.cache_write;
    }
    let usage = transcript::parse_str(&text).usage;
    assert_eq!(
        (sum.input, sum.output, sum.cache_read, sum.cache_write),
        (
            usage.input,
            usage.output,
            usage.cache_read_input,
            usage.cache_creation_input
        )
    );
}

#[test]
fn nothing_to_count_gives_zeros() {
    let summary = summarize(
        std::iter::empty::<(Option<&str>, &[refract_core::usage::UsageRecord])>(),
        &repos(),
        now(),
        today_start(),
        0,
    );
    assert_eq!(summary.last_7_days, TokenTotals::default());
    assert!(summary.by_repo.is_empty());
    assert!(summary.ready);
    let value = serde_json::to_value(&summary).unwrap();
    for key in [
        "today",
        "last7Days",
        "last5Hours",
        "liveAgents",
        "byRepo",
        "computedAt",
        "ready",
    ] {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(
        value["today"],
        serde_json::json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0})
    );
}

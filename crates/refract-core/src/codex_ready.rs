//! Whether a repo is ready for Codex (spec `.plans/codex-spec.md` §4), how each agent's
//! New row stands against the other agent's live sessions (§5), and the prompt of the
//! Make Codex-ready button (§9).
//!
//! [`readiness`] is a pure function over the repo's instruction files, which the helper
//! reads (AGENTS.md, CLAUDE.md, CLAUDE.local.md, the other AGENTS.md files Codex loads
//! with it, and the notes files [`named_notes`] finds in AGENTS.md), and its live
//! sessions. The worst rule that applies sets the state: Not ready, then Busy, then
//! Warnings, then Ready. Only Not ready and Busy block a launch.

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::classify::LiveState;

/// The most bytes of AGENTS.md text Codex reads (its `project_doc_max_bytes` default).
pub const MAX_AGENTS_BYTES: u64 = 32 * 1024;

/// The longest line text a finding keeps, in characters.
pub const FINDING_MAX_CHARS: usize = 160;

/// Terms that mark a line as a Claude-only instruction, matched without regard to case.
/// A term that starts or ends with a letter or digit must not run on into another one
/// there, so `/clear` doesn't match `/clearance` and `plan mode` doesn't match
/// `airplane mode`. `model:` is matched with any spaces and quotes before the name.
pub const CLAUDE_TERMS: &[&str] = &[
    "AskUserQuestion",
    "ExitPlanMode",
    "plan mode",
    "subagent_type",
    "model: opus",
    "model: sonnet",
    "model: haiku",
    "~/.claude/",
    "/clear",
    "/compact",
    ".claude/skills",
    "Skill tool",
];

/// Words that name Codex or its own counterpart of a Claude step. A line with one of
/// these names both agents ("`/clear` in Claude, `/new` in Codex"), so it is a mention,
/// not a Claude-only instruction.
pub const CODEX_TERMS: &[&str] = &["codex", "/new"];

/// Words in a notes file's path that mark it as notes an agent keeps (a handoff or
/// progress file) rather than a spec or a doc. Only those are scanned; a spec that
/// describes `/clear` as a product feature would only give false warnings.
pub const NOTE_HINTS: &[&str] = &["handoff", "progress", "notes", "todo", "status"];

/// The repo's instruction files, as the helper read them. A missing file is `None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstructionFiles<'a> {
    /// `AGENTS.md` at the repo root.
    pub agents_md: Option<&'a str>,
    /// `CLAUDE.md` at the repo root.
    pub claude_md: Option<&'a str>,
    /// `CLAUDE.local.md` at the repo root.
    pub claude_local_md: Option<&'a str>,
    /// Sizes in bytes of the other AGENTS.md files Codex loads with the root one (nested
    /// ones on the way from the git root to the repo folder).
    pub other_agents_bytes: &'a [u64],
    /// The notes files [`named_notes`] found in AGENTS.md that exist, as (path relative
    /// to the repo, contents).
    pub notes: &'a [(&'a str, &'a str)],
}

/// How launching Codex stands. On the wire: `"ready"`, `"warnings"`, `"notReady"`,
/// `"busy"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReadyState {
    Ready,
    /// Ready, with Claude-only lines Codex will read.
    Warnings,
    /// Blocks the launch: the instructions don't reach Codex.
    NotReady,
    /// Blocks the launch: Claude is working or waiting on the person (see
    /// [`AgentGates::codex`]).
    Busy,
}

impl ReadyState {
    /// Codex can be launched (Ready or Warnings).
    pub fn launchable(self) -> bool {
        matches!(self, ReadyState::Ready | ReadyState::Warnings)
    }
}

/// What a finding is about. On the wire: `"noAgentsMd"`, `"claudeMdNotSynced"`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingKind {
    /// Not ready: no non-empty AGENTS.md at the repo root.
    NoAgentsMd,
    /// Not ready: CLAUDE.md or CLAUDE.local.md has no `@AGENTS.md` line and is not the
    /// same as AGENTS.md, so the two can drift apart.
    ClaudeMdNotSynced,
    /// Not ready: AGENTS.md relies on an `@path` import, which Codex does not follow.
    AtImport,
    /// Not ready: the AGENTS.md files are over [`MAX_AGENTS_BYTES`].
    TooLarge,
    /// Warning: a Claude-only instruction ([`CLAUDE_TERMS`]).
    ClaudeOnly,
}

impl FindingKind {
    /// The finding blocks the launch.
    pub fn blocks(self) -> bool {
        self != FindingKind::ClaudeOnly
    }
}

/// One finding, shown as `file:line text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// Path relative to the repo, such as `AGENTS.md` or `notes/HANDOFF.md`.
    pub file: String,
    /// 1-based; `None` for a finding about the whole file.
    pub line: Option<u32>,
    pub kind: FindingKind,
    /// What is wrong, or for [`FindingKind::ClaudeOnly`] the line itself, trimmed and
    /// cut to [`FINDING_MAX_CHARS`].
    pub text: String,
    /// The [`CLAUDE_TERMS`] entry that matched, for a Claude-only line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

/// A live session as the gates take it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveAgent {
    pub agent: AgentKind,
    pub session_id: String,
    pub state: LiveState,
}

impl LiveAgent {
    /// Working, waiting on the person, or open with no status saying it is idle (a
    /// Claude session without the status hook). Such a session could be changing the
    /// working tree, so the other agent is not started beside it.
    pub fn is_busy(&self) -> bool {
        self.state != LiveState::Idle
    }
}

/// How one agent's New row stands against the other agent's live sessions. On the wire:
/// `{"kind": "open"}`, `{"kind": "busy", "agent": "claude", "sessionId": "...", "state":
/// "working"}` or `{"kind": "stopIdle", "sessionIds": ["..."]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Gate {
    /// No live session of the other agent.
    #[default]
    Open,
    /// The other agent is busy (the newest busy session): the row is dimmed with
    /// "Busy: <agent> is working" (or waiting on you).
    Busy(LiveAgent),
    /// The other agent is open and idle: the row becomes "Stop the idle <other> agent
    /// and start <this>". The helper stops these sessions first (the Refactor stop
    /// path, which refuses while background work runs), then launches.
    StopIdle { session_ids: Vec<String> },
}

impl Gate {
    pub fn is_open(&self) -> bool {
        *self == Gate::Open
    }
}

/// Both New rows' gates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGates {
    /// New Claude agent, against live Codex sessions.
    #[serde(default, skip_serializing_if = "Gate::is_open")]
    pub claude: Gate,
    /// New Codex agent, against live Claude sessions.
    #[serde(default, skip_serializing_if = "Gate::is_open")]
    pub codex: Gate,
}

impl AgentGates {
    /// The gate of `agent`'s New row.
    pub fn of(&self, agent: AgentKind) -> &Gate {
        match agent {
            AgentKind::Claude => &self.claude,
            AgentKind::Codex => &self.codex,
        }
    }
}

/// The readiness the panel and the picker show. On the wire: `{"state": "warnings",
/// "findings": [...], "gates": {"codex": {"kind": "stopIdle", ...}}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexReadiness {
    pub state: ReadyState,
    /// Blocking findings first, then warnings, each in file and line order.
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub gates: AgentGates,
}

/// How `target`'s New row stands against the other agent's sessions in `live`: busy
/// when any of them is busy (the newest listed first wins, so pass them newest first),
/// otherwise stop-idle when any is open, otherwise open.
pub fn gate(target: AgentKind, live: &[LiveAgent]) -> Gate {
    let other: Vec<&LiveAgent> = live.iter().filter(|l| l.agent != target).collect();
    if let Some(busy) = other.iter().find(|l| l.is_busy()) {
        return Gate::Busy((*busy).clone());
    }
    if other.is_empty() {
        Gate::Open
    } else {
        Gate::StopIdle {
            session_ids: other.iter().map(|l| l.session_id.clone()).collect(),
        }
    }
}

/// The repo's Codex readiness from its instruction files and its live sessions (of
/// either agent, newest first).
pub fn readiness(files: &InstructionFiles<'_>, live: &[LiveAgent]) -> CodexReadiness {
    let mut findings = blocking_findings(files);
    if let Some(agents) = nonempty(files.agents_md) {
        findings.extend(claude_only_lines("AGENTS.md", agents));
        for (path, text) in files.notes {
            findings.extend(claude_only_lines(path, text));
        }
    }
    let gates = AgentGates {
        claude: gate(AgentKind::Claude, live),
        codex: gate(AgentKind::Codex, live),
    };
    let state = if findings.iter().any(|f| f.kind.blocks()) {
        ReadyState::NotReady
    } else if matches!(gates.codex, Gate::Busy(_)) {
        ReadyState::Busy
    } else if findings.is_empty() {
        ReadyState::Ready
    } else {
        ReadyState::Warnings
    };
    CodexReadiness {
        state,
        findings,
        gates,
    }
}

fn nonempty(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.trim().is_empty())
}

/// The Not ready rules, in the spec's order.
fn blocking_findings(files: &InstructionFiles<'_>) -> Vec<Finding> {
    let whole = |file: &str, kind, text: String| Finding {
        file: file.to_owned(),
        line: None,
        kind,
        text,
        term: None,
    };
    let Some(agents) = nonempty(files.agents_md) else {
        return vec![whole(
            "AGENTS.md",
            FindingKind::NoAgentsMd,
            "No AGENTS.md at the repo root".into(),
        )];
    };
    let mut findings = Vec::new();
    for (file, text) in [
        ("CLAUDE.md", files.claude_md),
        ("CLAUDE.local.md", files.claude_local_md),
    ] {
        if let Some(text) = text {
            if !imports_agents_md(text) && text != files.agents_md.unwrap_or_default() {
                findings.push(whole(
                    file,
                    FindingKind::ClaudeMdNotSynced,
                    format!("{file} has no @AGENTS.md line and differs from AGENTS.md"),
                ));
            }
        }
    }
    for (n, line) in unfenced_lines(agents) {
        if is_import(line) {
            findings.push(Finding {
                file: "AGENTS.md".into(),
                line: Some(n),
                kind: FindingKind::AtImport,
                text: format!("Codex does not follow imports: {}", cut(line.trim())),
                term: None,
            });
        }
    }
    let total = agents.len() as u64 + files.other_agents_bytes.iter().sum::<u64>();
    if total > MAX_AGENTS_BYTES {
        findings.push(whole(
            "AGENTS.md",
            FindingKind::TooLarge,
            format!(
                "AGENTS.md files add up to {} KiB, over Codex's {} KiB",
                total.div_ceil(1024),
                MAX_AGENTS_BYTES / 1024
            ),
        ));
    }
    findings
}

/// The file has an `@AGENTS.md` (or `@./AGENTS.md`) line.
fn imports_agents_md(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|l| l == "@AGENTS.md" || l == "@./AGENTS.md")
}

/// A line that is only an `@path` import, such as `@docs/rules.md` or
/// `@~/.claude/rules.md`. An `@name` inside prose is a mention, not an import.
fn is_import(line: &str) -> bool {
    let line = line.trim();
    line.len() > 1
        && line.starts_with('@')
        && !line.contains(char::is_whitespace)
        && line[1..].contains(['/', '.'])
}

/// The lines outside code fences, numbered from 1.
fn unfenced_lines(text: &str) -> impl Iterator<Item = (u32, &str)> {
    let mut fenced = false;
    text.lines().enumerate().filter_map(move |(i, line)| {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            return None;
        }
        (!fenced).then_some((i as u32 + 1, line))
    })
}

/// The Claude-only lines of one file. Lines under a heading that says Claude only (such
/// as `## Claude Code only`, and not naming Codex) are skipped, as is any line that names
/// Codex too.
fn claude_only_lines(file: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut claude_section: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        if let Some(level) = heading_level(line) {
            if claude_section.is_some_and(|l| level <= l) {
                claude_section = None;
            }
            if claude_section.is_none()
                && lower.contains("claude")
                && lower.contains("only")
                && !names_codex(&lower)
            {
                claude_section = Some(level);
                continue;
            }
        }
        if claude_section.is_some() || names_codex(&lower) {
            continue;
        }
        if let Some(term) = claude_term(&lower) {
            findings.push(Finding {
                file: file.to_owned(),
                line: Some(i as u32 + 1),
                kind: FindingKind::ClaudeOnly,
                text: cut(line.trim()),
                term: Some(term.to_owned()),
            });
        }
    }
    findings
}

/// `1` for `# Title`, `2` for `## Title`, ...; `None` for any other line.
fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

fn names_codex(lower: &str) -> bool {
    CODEX_TERMS.iter().any(|t| find_term(lower, t))
}

/// The first [`CLAUDE_TERMS`] entry in a lowercased line.
pub fn claude_term(lower: &str) -> Option<&'static str> {
    let line = normalize_model(lower);
    CLAUDE_TERMS
        .iter()
        .copied()
        .find(|t| find_term(&line, &t.to_lowercase()))
}

/// `model:` followed by any spaces, quotes or backticks becomes `model: `.
fn normalize_model(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("model:") {
        out.push_str(&rest[..at]);
        out.push_str("model: ");
        rest = rest[at + "model:".len()..].trim_start_matches([' ', '\t', '"', '\'', '`']);
    }
    out.push_str(rest);
    out
}

/// `term` occurs in `line` without running on into a word at an alphanumeric edge. A
/// term starting with `/` must not follow a letter or digit either, so a path such as
/// `src/clear.rs` is not the `/clear` command.
fn find_term(line: &str, term: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let first = term.chars().next().unwrap_or(' ');
    let last = term.chars().last().unwrap_or(' ');
    line.match_indices(term).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + term.len()..].chars().next();
        let start_ok = !(word(first) || first == '/') || !before.is_some_and(word);
        let end_ok = !word(last) || !after.is_some_and(|c| word(c) || c == '-');
        start_ok && end_ok
    })
}

fn cut(line: &str) -> String {
    if line.chars().count() <= FINDING_MAX_CHARS {
        return line.to_owned();
    }
    let mut text: String = line.chars().take(FINDING_MAX_CHARS - 1).collect();
    text.push('…');
    text
}

/// The notes files AGENTS.md names by relative path, in first-named order and without
/// repeats, for the helper to read and pass back in [`InstructionFiles::notes`]. A name
/// counts when it is a relative `.md` or `.txt` path inside the repo (no `/` or `~`
/// start, no `..`, no URL) whose path contains one of the [`NOTE_HINTS`], such as
/// `notes/HANDOFF.md` or `PROGRESS.md`. Backticks, quotes, brackets and trailing
/// punctuation around it are dropped, so `` `notes/HANDOFF.md`. `` and
/// `[handoff](notes/HANDOFF.md)` both count.
pub fn named_notes(agents_md: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let separators = |c: char| c.is_whitespace() || matches!(c, '`' | '(' | ')' | '[' | ']');
    for token in agents_md.split(separators) {
        let path = token
            .trim_start_matches(['"', '\'', '<', '*', '_'])
            .trim_end_matches(['"', '\'', '>', '*', '_', '.', ',', ':', ';', '!', '?']);
        if is_note_path(path) && !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

fn is_note_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let file = lower.rsplit('/').next().unwrap_or("");
    (lower.ends_with(".md") || lower.ends_with(".txt"))
        && !path.starts_with(['/', '~', '@'])
        && !path.contains("://")
        && !path.split('/').any(|s| s == ".." || s.is_empty())
        && !matches!(file, "agents.md" | "claude.md" | "claude.local.md")
        && NOTE_HINTS.iter().any(|h| lower.contains(h))
}

/// The terminal tab's name for a Make Codex-ready agent.
pub const MAKE_READY_TITLE: &str = "Make Codex-ready";

/// The opening prompt of the Make Codex-ready agent (spec §9). It runs as a plain New
/// launch with no plan mode, as Claude, or as Codex when Claude is over the threshold;
/// the repo watch re-checks readiness when the files change.
pub const MAKE_READY_PROMPT: &str = "Make this repository ready for Codex CLI as well as \
Claude Code, without changing what the instructions mean.
1. Move the rules that apply to any coding agent into AGENTS.md at the repo root (create it \
if missing). Codex reads AGENTS.md but does not follow `@path` imports, so inline anything \
AGENTS.md imports, and keep it under 32 KiB.
2. Leave CLAUDE.md (or CLAUDE.local.md, whichever exists) as a line `@AGENTS.md` followed by \
a \"Claude only\" section with the Claude-specific rules (AskUserQuestion, plan mode, \
subagent types and models, skills, ~/.claude paths).
3. In the handoff and progress notes AGENTS.md names, reword Claude-only steps neutrally, \
keeping their meaning (for example \"start a fresh session: `/clear` in Claude, `/new` in \
Codex\").
4. Give AGENTS.md the same git tracking as CLAUDE.md: tracked if CLAUDE.md is tracked, \
untracked and ignored the same way if it is not.
5. Do not edit .gitignore and do not commit.
6. Finish with a short list of what moved where.";

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &str = "# Rules\n\nRead `notes/HANDOFF.md` first.\nRun the tests.\n";

    fn files(agents: &str) -> InstructionFiles<'_> {
        InstructionFiles {
            agents_md: Some(agents),
            ..InstructionFiles::default()
        }
    }

    fn kinds(r: &CodexReadiness) -> Vec<FindingKind> {
        r.findings.iter().map(|f| f.kind).collect()
    }

    fn live(agent: AgentKind, id: &str, state: LiveState) -> LiveAgent {
        LiveAgent {
            agent,
            session_id: id.into(),
            state,
        }
    }

    #[test]
    fn a_plain_agents_md_is_ready() {
        let r = readiness(&files(AGENTS), &[]);
        assert_eq!(r.state, ReadyState::Ready);
        assert!(r.findings.is_empty());
        assert_eq!(r.gates, AgentGates::default());
    }

    #[test]
    fn a_missing_or_blank_agents_md_is_not_ready() {
        for agents in [None, Some(""), Some("  \n\n")] {
            let r = readiness(
                &InstructionFiles {
                    agents_md: agents,
                    claude_md: Some("# Old rules\n"),
                    ..InstructionFiles::default()
                },
                &[],
            );
            assert_eq!(r.state, ReadyState::NotReady, "{agents:?}");
            assert_eq!(kinds(&r), [FindingKind::NoAgentsMd]);
            assert_eq!(r.findings[0].file, "AGENTS.md");
            assert_eq!(r.findings[0].line, None);
        }
    }

    #[test]
    fn claude_md_must_import_or_equal_agents_md() {
        let imports = InstructionFiles {
            claude_md: Some("@AGENTS.md\n\n# Claude only\nUse AskUserQuestion.\n"),
            ..files(AGENTS)
        };
        assert_eq!(readiness(&imports, &[]).state, ReadyState::Ready);
        let local = InstructionFiles {
            claude_local_md: Some("  @./AGENTS.md\n"),
            ..files(AGENTS)
        };
        assert_eq!(readiness(&local, &[]).state, ReadyState::Ready);
        let copy = InstructionFiles {
            claude_md: Some(AGENTS),
            ..files(AGENTS)
        };
        assert_eq!(readiness(&copy, &[]).state, ReadyState::Ready);
        let drifted = InstructionFiles {
            claude_md: Some("# Rules\n\nRun the tests.\n"),
            claude_local_md: Some("See AGENTS.md\n"),
            ..files(AGENTS)
        };
        let r = readiness(&drifted, &[]);
        assert_eq!(r.state, ReadyState::NotReady);
        assert_eq!(
            kinds(&r),
            [
                FindingKind::ClaudeMdNotSynced,
                FindingKind::ClaudeMdNotSynced
            ]
        );
        assert_eq!(r.findings[0].file, "CLAUDE.md");
        assert_eq!(r.findings[1].file, "CLAUDE.local.md");
    }

    #[test]
    fn an_import_in_agents_md_is_not_ready() {
        let agents = "# Rules\n@docs/rules.md\nMention @maddox in prose.\n```\n@not/an/import.md\n```\n@~/.claude/shared.md\n";
        let r = readiness(&files(agents), &[]);
        assert_eq!(r.state, ReadyState::NotReady);
        let lines: Vec<_> = r
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::AtImport)
            .map(|f| f.line)
            .collect();
        assert_eq!(lines, [Some(2), Some(7)]);
    }

    #[test]
    fn agents_md_files_over_the_limit_are_not_ready() {
        let big = "x".repeat(MAX_AGENTS_BYTES as usize);
        assert_eq!(readiness(&files(&big), &[]).state, ReadyState::Ready);
        let r = readiness(
            &InstructionFiles {
                other_agents_bytes: &[1],
                ..files(&big)
            },
            &[],
        );
        assert_eq!(kinds(&r), [FindingKind::TooLarge]);
        assert_eq!(
            r.findings[0].text,
            "AGENTS.md files add up to 33 KiB, over Codex's 32 KiB"
        );
    }

    #[test]
    fn each_claude_term_warns_with_its_line() {
        let cases = [
            ("Ask with AskUserQuestion.", "AskUserQuestion"),
            ("Call ExitPlanMode when done.", "ExitPlanMode"),
            ("Use Plan Mode for designs.", "plan mode"),
            ("Pass subagent_type: scout.", "subagent_type"),
            ("Spawn it with model: opus.", "model: opus"),
            ("Spawn it with `model: \"sonnet\"`.", "model: sonnet"),
            ("Use model:haiku for searches.", "model: haiku"),
            ("Rules live in ~/.claude/CLAUDE.md.", "~/.claude/"),
            ("Then run /clear.", "/clear"),
            ("Suggest `/compact` when large.", "/compact"),
            ("Skills are in .claude/skills.", ".claude/skills"),
            ("Load it with the Skill tool.", "Skill tool"),
        ];
        assert_eq!(cases.len(), CLAUDE_TERMS.len());
        for (line, term) in cases {
            let agents = format!("# Rules\n\n{line}\n");
            let r = readiness(&files(&agents), &[]);
            assert_eq!(r.state, ReadyState::Warnings, "{line}");
            assert_eq!(r.findings.len(), 1, "{line}");
            let f = &r.findings[0];
            assert_eq!(
                (
                    f.file.as_str(),
                    f.line,
                    f.kind,
                    f.text.as_str(),
                    f.term.as_deref()
                ),
                (
                    "AGENTS.md",
                    Some(3),
                    FindingKind::ClaudeOnly,
                    line,
                    Some(term)
                ),
            );
        }
    }

    #[test]
    fn mentions_do_not_warn() {
        for line in [
            "Start a fresh session: `/clear` in Claude, `/new` in Codex.",
            "Codex has no plan mode; Claude does.",
            "Look in src/clear.rs and the /clearance page.",
            "Turn on airplane mode.",
            "The model: gpt-6-luna default.",
            "Compaction is automatic; see /compacting.",
        ] {
            let agents = format!("# Rules\n{line}\n");
            assert_eq!(
                readiness(&files(&agents), &[]).state,
                ReadyState::Ready,
                "{line}"
            );
        }
    }

    #[test]
    fn a_claude_only_section_does_not_warn() {
        let agents = "# Rules\nRun the tests.\n\n## Claude Code only\nUse AskUserQuestion.\n### Tiers\nUse model: opus.\n\n## Commits\nThen run /clear.\n";
        let r = readiness(&files(agents), &[]);
        assert_eq!(r.state, ReadyState::Warnings);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].line, Some(10));
    }

    #[test]
    fn notes_files_are_scanned_and_blockers_come_first() {
        let notes = [(
            "notes/HANDOFF.md",
            "# Handoff\n\nNext: wire it up, then /clear.\n",
        )];
        let r = readiness(
            &InstructionFiles {
                notes: &notes,
                claude_md: Some("old"),
                ..files("# Rules\nUse AskUserQuestion.\n")
            },
            &[],
        );
        assert_eq!(r.state, ReadyState::NotReady);
        let at: Vec<_> = r
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.kind))
            .collect();
        assert_eq!(
            at,
            [
                ("CLAUDE.md", None, FindingKind::ClaudeMdNotSynced),
                ("AGENTS.md", Some(2), FindingKind::ClaudeOnly),
                ("notes/HANDOFF.md", Some(3), FindingKind::ClaudeOnly),
            ]
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let line = format!("Run /clear {}", "x".repeat(300));
        let r = readiness(&files(&line), &[]);
        assert_eq!(r.findings[0].text.chars().count(), FINDING_MAX_CHARS);
        assert!(r.findings[0].text.ends_with('…'));
    }

    #[test]
    fn a_busy_claude_blocks_codex_and_an_idle_one_can_be_stopped() {
        let working = [live(AgentKind::Claude, "c1", LiveState::Working)];
        let r = readiness(&files(AGENTS), &working);
        assert_eq!(r.state, ReadyState::Busy);
        assert_eq!(r.gates.codex, Gate::Busy(working[0].clone()));
        assert_eq!(r.gates.claude, Gate::Open);
        // Waiting, or open without a status: busy too.
        for state in [LiveState::WaitingOnUser, LiveState::Unknown] {
            let l = [live(AgentKind::Claude, "c1", state)];
            assert_eq!(readiness(&files(AGENTS), &l).state, ReadyState::Busy);
        }
        // Idle: offered to stop, and Codex is launchable.
        let idle = [
            live(AgentKind::Claude, "c1", LiveState::Idle),
            live(AgentKind::Claude, "c2", LiveState::Idle),
        ];
        let r = readiness(&files(AGENTS), &idle);
        assert_eq!(r.state, ReadyState::Ready);
        assert_eq!(
            r.gates.codex,
            Gate::StopIdle {
                session_ids: vec!["c1".into(), "c2".into()]
            }
        );
        // One idle, one working: busy, naming the working one.
        let mixed = [
            idle[0].clone(),
            live(AgentKind::Claude, "c3", LiveState::Working),
        ];
        assert!(matches!(gate(AgentKind::Codex, &mixed), Gate::Busy(l) if l.session_id == "c3"));
        // Not ready wins over busy.
        assert_eq!(
            readiness(&InstructionFiles::default(), &working).state,
            ReadyState::NotReady
        );
    }

    #[test]
    fn the_claude_row_mirrors_live_codex_sessions() {
        let working = [live(AgentKind::Codex, "x1", LiveState::Working)];
        let r = readiness(&files(AGENTS), &working);
        // Codex's own sessions don't block Codex.
        assert_eq!(r.state, ReadyState::Ready);
        assert_eq!(r.gates.claude, Gate::Busy(working[0].clone()));
        assert_eq!(r.gates.of(AgentKind::Claude), &r.gates.claude);
        let idle = [live(AgentKind::Codex, "x1", LiveState::Idle)];
        assert_eq!(
            gate(AgentKind::Claude, &idle),
            Gate::StopIdle {
                session_ids: vec!["x1".into()]
            }
        );
    }

    #[test]
    fn readiness_goes_on_the_wire_in_camel_case() {
        let l = [live(AgentKind::Claude, "c1", LiveState::WaitingOnUser)];
        let r = readiness(&files("# Rules\nRun /clear.\n"), &l);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({
                "state": "busy",
                "findings": [{"file": "AGENTS.md", "line": 2, "kind": "claudeOnly",
                    "text": "Run /clear.", "term": "/clear"}],
                "gates": {"codex": {"kind": "busy", "agent": "claude", "sessionId": "c1",
                    "state": "waitingOnUser"}}
            })
        );
        let stop = Gate::StopIdle {
            session_ids: vec!["x1".into()],
        };
        assert_eq!(
            serde_json::to_value(&stop).unwrap(),
            serde_json::json!({"kind": "stopIdle", "sessionIds": ["x1"]})
        );
        let back: CodexReadiness =
            serde_json::from_value(serde_json::json!({"state": "ready", "findings": []})).unwrap();
        assert_eq!(back.gates, AgentGates::default());
    }

    #[test]
    fn notes_named_in_agents_md_are_found() {
        let agents = "Read `notes/HANDOFF.md` first, then [progress](docs/PROGRESS.md).\n\
            Keep notes/HANDOFF.md short. See .plans/tidepool-spec.md and README.md.\n\
            Never read /etc/notes.md, ~/notes.md, ../notes.md or https://example.com/notes.md.\n\
            CLAUDE.md and AGENTS.md are the rules; TODO.txt, \"status.md\".\n";
        assert_eq!(
            named_notes(agents),
            [
                "notes/HANDOFF.md",
                "docs/PROGRESS.md",
                "TODO.txt",
                "status.md"
            ]
        );
        assert!(named_notes("").is_empty());
    }

    #[test]
    fn the_make_ready_prompt_keeps_its_rules() {
        for needle in [
            "AGENTS.md",
            "@AGENTS.md",
            "Claude only",
            "`/new` in",
            ".gitignore",
            "do not commit",
        ] {
            assert!(MAKE_READY_PROMPT.contains(needle), "{needle}");
        }
    }
}

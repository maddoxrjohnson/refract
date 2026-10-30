# 12. Usage totals come from Claude Code's local transcripts

Status: accepted (M6)

**Context.** The usage widget needs token counts today, over a week and over the last five hours. Whether Claude Code exposes plan limits through an official local source is an open question, and Refract never calls a model or guesses.

**Decision.** The helper sums the `usage` fields of assistant entries in the session files, deduplicated by message id (the same response is written as several lines, and a session and its fork can hold the same message) and counting subagent transcripts. A thread with its own inotify instance keeps a byte offset per file, reads only new complete lines, and drops records older than 7 days, so a request only adds up what is in memory. Until the first scan finishes the answer is marked not ready. Plan-limit percentages and cost estimates are not shown.

**Consequences.** Collecting costs nothing in tokens, and a request only adds up what is in memory. The numbers are what Claude Code recorded, not what the provider billed. The first scan of a large history takes a moment, and the helper's memory grows with seven days of records.

# 7. Remote data is cached for fifteen minutes and a failure keeps the last good answer

Status: accepted (M5)

**Context.** GitHub's rate limits are per hour, the detail panel asks for details every time a gem is zoomed, and a flaky network or a missing token should not blank out data that was fine a minute ago.

**Decision.** Lists (per source) and details (per repo) are cached for 15 minutes. A failed fetch, or a source with no token, is tried again after 60 seconds, and a failure keeps the last good data and reports its error beside it. Refresh skips the cache. Each slot has its own lock, so concurrent requests for one repo share a single fetch. While the same gem stays zoomed and the window is shown, the interface fetches its details again 15 minutes after they were fetched.

**Consequences.** Data can be up to 15 minutes old, and the panel shows how old. The cache lives in memory, so a restart asks again. Changing a token drops what was cached with the old one.

# System widget: Windows and WSL split

Status: agreed with Maddox 2026-11-02; built 2026-11-02 (see §9 for where it differs).

## 1. Why

The system widget shows how much CPU, memory and GPU the whole machine uses, with Refract's share drawn inside each bar. When the machine runs out of memory you can't tell whether WSL or Windows is responsible. On 2026-11-02 Windows had 327 MB free out of 16 GB, and `code.cmd` calls timed out. The WSL VM (`vmmemWSL`) held 5.5 GB of that, and 4.0 GB of what Linux holds is file cache.

## 2. Decisions

| Question | Decision |
|---|---|
| How it looks | Each bar is split into a **WSL** segment and a **Windows** segment, with a small legend. No new bars. |
| WSL memory figure | What Windows sees: `vmmemWSL`'s working set. The part of it that is Linux file cache is drawn fainter and striped, because Linux can give it back. |
| Bars that are split | Memory and CPU. GPU is split only when the counters credit work to `vmmemWSL` (see §4.3). Otherwise the GPU bar stays whole, with no WSL segment. |
| The helper | `refract-agent` and its child processes count as Refract. Refract's bright mark sits in both segments: WebView2 and the app inside the Windows part, the helper inside the WSL part. The Refract line shows the combined number. |

## 3. Layout

```
System
CPU     [▓▓▓▓▓▒▒▒▒░░░░░░░░░░]  42%
Memory  [▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒░░] 15.7 GB
GPU     [▓▒▒▒░░░░░░░░░░░░░░░]  18%
▓ WSL  ▒ Windows  ■ Refract
WSL     12% CPU · 5.5 GB · 1% GPU
Refract  3% CPU · 410 MB · 2% GPU
```

- Order inside a bar: WSL first, then Windows. Refract's bright mark is drawn at the start of each segment, sized to its share there: the helper in WSL, the app and WebView2 in Windows.
- Memory's WSL segment has two parts: what Linux programs use (solid) and what is cache (fainter and striped). See §4.1 for how the cache part is worked out.
- The value text stays the machine total. The meter's `aria-valuetext` adds the WSL figure, for example "15.7 GB, WSL 5.5 GB".
- The WSL line leaves out GPU when there is no WSL GPU figure.
- When there are no WSL figures (helper not running, an older helper, the first reading), the bars and lines look exactly as they do today, with no legend entry for WSL.
- Colours are tokens in `ui.css`, readable in both themes. The two shades of WSL memory must still be told apart in Low quality and with reduced motion (nothing animates anyway).

## 4. Where the numbers come from

Sampling stays as it is: every 2 s, only while the window is visible ("Resource use while hidden" is unchanged).

### 4.1 Memory

- **Machine total:** `sysinfo`, as today.
- **WSL:** `vmmemWSL`'s working set, read on the Windows side. Check first whether `sysinfo` can read it: `Get-Process` gives its working set without admin, but not its CPU time. If `sysinfo` can't read it, use the process memory API directly with `PROCESS_QUERY_LIMITED_INFORMATION`. If that fails too, use Linux `MemTotal - MemFree` from the helper and say so in the doc.
- **Cache shade:** Linux file cache `c` (`Buffers + Cached + SReclaimable - Shmem` from `/proc/meminfo`) and program use `u` (`MemTotal - MemFree - c`). Some of the VM's memory can be paged out on the Windows side, so not all of `u + c` is resident. Programs are assumed to be resident first: programs (solid) = `min(u, ws)`, and the rest of `ws` (cache, fainter and striped) after it, where `ws` is the working set. See §9.
- **Refract in WSL:** the resident memory of the helper and its live descendants (`git` and the like), from `/proc/<pid>/status` `VmRSS`.

### 4.2 CPU

- **Machine total:** `sysinfo`, as today. It already includes the VM's work.
- **WSL:** from the helper. The busy share of `/proc/stat`'s `cpu` line between two readings, times `linux_cpus / windows_logical_cpus`, so a `.wslconfig` with `processors=` scales correctly. The segment is limited to the machine total.
- **Refract in WSL:** CPU time of the helper and its live descendants between two readings (`/proc/<pid>/stat` utime+stime), as a share of all Windows CPUs. A child that exited between readings is left out (a small undercount, same as on Windows).

### 4.3 GPU

- `\GPU Engine(*)` instances whose pid is `vmmemWSL`'s are WSL's, added up per engine the same way as Refract's own share. On 2026-11-02 there were none, since nothing in WSL was using the GPU. So: no `vmmemWSL` instance in a reading means no WSL GPU figure (`null`). An instance at 0 % means `0`.
- Refract has no GPU share inside WSL.

## 5. Protocol

- New helper method `systemStats`, no params. It returns `{cpuPercent, cpuCount, memTotalBytes, memUsedBytes, memCacheBytes, own: {cpuPercent, memBytes}}` from Linux's point of view. CPU figures are `null` on the first call, which only primes the counters. The helper keeps the previous reading for its own process tree and `/proc/stat`.
- The app calls it from the sampler thread on each tick, through the existing client. It does not wait past the tick: if the reply is late, that tick has no WSL figures.
- An older helper answers "unknown method": no WSL figures, no error shown, no retry storm (try again only after the helper restarts).
- `PROTOCOL_VERSION` stays 1, since nothing existing changes. The app copes with the method being missing.

## 6. App side

- `SystemStats` gains `wsl: { cpuPercent, memBytes, memProgramBytes, memCacheBytes, gpuPercent: number | null } | null`. `own` gains `wslCpuPercent` and `wslMemBytes`, so the bar can place the helper inside the WSL segment, while `own.cpuPercent` / `own.memBytes` become the combined figures shown in the Refract line.
- `stats.rs` keeps `wsl.exe` in Refract's Windows processes. Its process tree walk is unchanged.
- The web demo (`DemoBackend.ts`) gets plausible WSL figures, and a mode with none, so e2e can check both.

## 7. Tests

- Rust (helper): parsing `/proc/stat`, `/proc/meminfo` and `/proc/<pid>/stat|status` from fixture text; the delta and scaling maths; the first call returning `null` CPU; process tree walk with a stale parent.
- Rust (app): splitting the shading (§4.1) including `ws < u`, `ws < u + c`, zero cache; GPU instances for `vmmemWSL` → figure, none → `null`; WSL CPU limited to the total.
- Vitest: the widget with and without `wsl`, with `gpuPercent: null`, `aria-valuetext`, the WSL line, the Refract line's combined numbers.
- e2e: the demo shows the legend and WSL line; with the no-WSL mode it looks as before.
- Hand-check after `install-local.sh`: on Maddox's machine the WSL memory segment is near what Task Manager shows for `vmmemWSL`. `stress`-style load in WSL grows the WSL CPU segment. Optional: something using the GPU in WSL shows a WSL GPU figure.

## 8. Docs to update

`docs/architecture.md` "System stats" (including "What is Refract", which currently says the helper's work isn't attributed) and the methods table; `.plans/refract-spec.md` §3.5; README if it describes the widget.

## 9. As built

- **Working set:** `sysinfo` reads 0 for `vmmemWSL` and `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` is refused without admin, so the app reads the working set from the system's process list, `NtQuerySystemInformation(SystemProcessInformation)`, as `Get-Process` does. The list is walked by a safe function over its bytes, and the buffer is capped at 64 MiB. Linux's `MemTotal - MemFree` is used only if that fails.
- **The striped part:** everything in the working set beyond what programs use, so memory Linux has freed but not yet handed back is in it along with the cache: the part Windows can get back. The legend has a striped "cache" entry, and the Memory meter's `aria-valuetext` gives it, for example "15.7 GB, WSL 5.5 GB (4 GB cache)". A 1 px gap separates the WSL and Windows segments.
- **Missed answers:** the app waits 400 ms for `systemStats`. An answer after a tick without one only primes the counters, so the last good reading is shown through a missed tick and the priming tick after it while it is under 6 s old; after that there are no WSL figures.
- **The helper's minimum interval:** a call within 1 s of the helper's last reading, or one where `/proc/stat` or the `/proc` listing can't be read, gets `null` CPU figures and leaves the last reading in place.
- **Older helper:** answered "unknown method" once per helper start; the marker is kept across window shows.
- **Height:** with WSL figures the widget is a fixed 52 px taller (labels on their own lines, so the height doesn't change with the digits). The usage widget's 7-day list leaves 52 px more room for it. Below 540 px of window height the legend and WSL line are hidden, and the Refract line shows its swatch.

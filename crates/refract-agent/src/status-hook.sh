#!/bin/sh
# Refract status hook, installed by refract-agent (spec §6.2). Claude Code runs it as
# `sh status-hook.sh <status> <status dir>` with the hook's JSON on stdin. It writes
# <status dir>/<session_id>.json (without an absolute status dir,
# ${XDG_STATE_HOME:-~/.local/state}/refract/sessions/<session_id>.json) as
# {"status":"working","event":"UserPromptSubmit","at":<unix seconds>,"pids":[...],
# "starts":[...],"reason":"","boot":"<boot id>","init":<ticks>}, through a temporary
# file so readers never see half of it. `pids` are the script's ancestors from its parent up, at most 3 and above pid 1,
# and `starts` when each started, in clock ticks after boot, both read from
# /proc/<pid>/stat: Claude Code runs the hook command through a shell, so the claude
# process that ran it is among them, which tells which session a process runs after
# /clear or /resume. `reason`, for `ended` only, is the input's own (SessionEnd gives
# one, such as `clear`), letters and `_` only, else empty. `boot` is the kernel's boot
# id (/proc/sys/kernel/random/boot_id, hex and `-` only, else empty) and `init` when
# PID 1 started, in clock ticks after boot (0 when unknown): a report from another
# boot, or from another PID 1 (a WSL distro restarted in the same VM keeps the boot
# id), was written before a restart. It prints nothing and always exits 0, so Claude
# never sees it and a failure never blocks a session.
{
    # Read at most 1 MiB (session_id comes first) and drain the rest, so Claude never
    # writes into a closed pipe.
    input=$({ head -c 1048576; cat >/dev/null; } | tr -d '\n\r')
    status=$1
    case $status in
    working | waitingOnUser | finished | ended) ;;
    *) exit 0 ;;
    esac
    id=$(printf '%s' "$input" |
        grep -o '"session_id"[[:space:]]*:[[:space:]]*"[0-9A-Fa-f-]*"' | head -n 1 |
        sed 's/.*"\([0-9A-Fa-f-]*\)"$/\1/')
    case $id in
    ????????-????-????-????-????????????) ;;
    *) exit 0 ;;
    esac
    case $id in
    *[!0-9A-Fa-f-]*) exit 0 ;;
    esac
    event=$(printf '%s' "$input" |
        grep -o '"hook_event_name"[[:space:]]*:[[:space:]]*"[A-Za-z]*"' | head -n 1 |
        sed 's/.*"\([A-Za-z]*\)"$/\1/')
    # Only an ending has a reason worth reading; other events' tool input could hold a
    # `reason` key of its own, and is not worth searching again.
    reason=
    if [ "$status" = ended ]; then
        reason=$(printf '%s' "$input" |
            grep -o '"reason"[[:space:]]*:[[:space:]]*"[A-Za-z_]*"' | head -n 1 |
            sed 's/.*"\([A-Za-z_]*\)"$/\1/')
    fi
    # The helper passes the directory it watches. Without one, a relative
    # XDG_STATE_HOME is ignored, as the XDG spec and refract-agent do.
    case ${2:-} in
    /*) dir=$2 ;;
    *)
        case ${XDG_STATE_HOME:-} in
        /*) dir=$XDG_STATE_HOME/refract/sessions ;;
        *) dir=${HOME:-/nonexistent}/.local/state/refract/sessions ;;
        esac
        ;;
    esac
    # Fields of /proc/<pid>/stat after the last `)` (the command name before it may
    # hold spaces and parentheses), from field 3: field 4 is the parent and field 22
    # the start. A function has its own positional parameters, so $1 and $2 stay.
    stat_fields() {
        ppid=${2:-}
        start=${20:-}
    }
    pids=
    starts=
    pid=${PPID:-}
    n=0
    while [ "$n" -lt 3 ]; do
        case $pid in
        '' | *[!0-9]*) break ;;
        esac
        [ "$pid" -gt 1 ] || break
        read -r stat <"/proc/$pid/stat" || break
        stat_fields ${stat##*)}
        case $start in
        '' | *[!0-9]*) break ;;
        esac
        pids=${pids:+$pids,}$pid
        starts=${starts:+$starts,}$start
        n=$((n + 1))
        pid=$ppid
    done
    boot=
    read -r boot </proc/sys/kernel/random/boot_id || boot=
    case $boot in
    *[!0-9A-Fa-f-]*) boot= ;;
    esac
    init=0
    if read -r stat </proc/1/stat; then
        stat_fields ${stat##*)}
        case $start in
        '' | *[!0-9]*) ;;
        *) init=$start ;;
        esac
    fi
    mkdir -p "$dir" || exit 0
    tmp=$dir/.$id.$$.tmp
    if printf '{"status":"%s","event":"%s","at":%s,"pids":[%s],"starts":[%s],"reason":"%s","boot":"%s","init":%s}\n' "$status" "$event" "$(date +%s)" "$pids" "$starts" "$reason" "$boot" "$init" >"$tmp"; then
        mv -f "$tmp" "$dir/$id.json" || rm -f "$tmp"
    else
        rm -f "$tmp"
    fi
} >/dev/null 2>&1
exit 0

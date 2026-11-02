#!/bin/sh
# Refract status hook, installed by refract-agent (spec §6.2). Claude Code runs it as
# `sh status-hook.sh <status> <status dir>` with the hook's JSON on stdin. It writes
# <status dir>/<session_id>.json (without an absolute status dir,
# ${XDG_STATE_HOME:-~/.local/state}/refract/sessions/<session_id>.json) as
# {"status":"working","event":"UserPromptSubmit","at":<unix seconds>,"pids":[...]},
# through a temporary file so readers never see half of it. `pids` are the script's
# ancestors from its parent up, at most 4 and above pid 1, read from /proc/<pid>/stat:
# Claude Code runs the hook command through a shell, so the claude process that ran it
# is among them, which tells which session a process runs after /clear or /resume. It
# prints nothing and always exits 0, so Claude never sees it and a failure never
# blocks a session.
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
    # Field 4 (ppid) of /proc/<pid>/stat, counted after the last `)`: the command name
    # before it may hold spaces and parentheses.
    pids=
    pid=${PPID:-}
    n=0
    while [ "$n" -lt 4 ]; do
        case $pid in
        '' | *[!0-9]*) break ;;
        esac
        [ "$pid" -gt 1 ] || break
        read -r stat <"/proc/$pid/stat" || break
        pids=${pids:+$pids,}$pid
        n=$((n + 1))
        rest=${stat##*)}
        rest=${rest# }
        rest=${rest#* }
        pid=${rest%% *}
    done
    mkdir -p "$dir" || exit 0
    tmp=$dir/.$id.$$.tmp
    if printf '{"status":"%s","event":"%s","at":%s,"pids":[%s]}\n' "$status" "$event" "$(date +%s)" "$pids" >"$tmp"; then
        mv -f "$tmp" "$dir/$id.json" || rm -f "$tmp"
    else
        rm -f "$tmp"
    fi
} >/dev/null 2>&1
exit 0

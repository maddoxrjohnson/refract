#!/bin/sh
# Refract status hook, installed by refract-agent (spec §6.2). Claude Code runs it as
# `sh status-hook.sh <status> <status dir>` with the hook's JSON on stdin. It writes
# <status dir>/<session_id>.json (without an absolute status dir,
# ${XDG_STATE_HOME:-~/.local/state}/refract/sessions/<session_id>.json) as
# {"status":"working","event":"UserPromptSubmit","at":<unix seconds>}, through a
# temporary file so readers never see half of it. It prints nothing and always exits
# 0, so Claude never sees it and a failure never blocks a session.
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
    mkdir -p "$dir" || exit 0
    tmp=$dir/.$id.$$.tmp
    if printf '{"status":"%s","event":"%s","at":%s}\n' "$status" "$event" "$(date +%s)" >"$tmp"; then
        mv -f "$tmp" "$dir/$id.json" || rm -f "$tmp"
    else
        rm -f "$tmp"
    fi
} >/dev/null 2>&1
exit 0

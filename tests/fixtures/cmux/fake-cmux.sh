#!/bin/sh
# A stand-in for the cmux CLI used by tests/cmux_probe.rs. It records
# every call and answers from the fixture files in FAKE_CMUX_FIXTURES.
#
# Log format, appended to FAKE_CMUX_LOG per call:
#   #call
#   #env CMUX_SOCKET_PATH=<value or unset>
#   #env CMUX_QUIET=<value or unset>
#   <tab><argv[0]>
#   <tab><argv[1]>
#   ...
#
# FAKE_CMUX_FAIL names a command (after any --socket pair) that must
# fail: it prints FAKE_CMUX_FAIL_WITH (default err_not_found.txt) to
# stderr and exits 1. FAKE_CMUX_SESSIONS picks the file `sessions`
# prints (default sessions.json).
log="${FAKE_CMUX_LOG:?FAKE_CMUX_LOG is required}"
fx="${FAKE_CMUX_FIXTURES:?FAKE_CMUX_FIXTURES is required}"

{
    echo '#call'
    echo "#env CMUX_SOCKET_PATH=${CMUX_SOCKET_PATH-unset}"
    echo "#env CMUX_QUIET=${CMUX_QUIET-unset}"
    for a; do printf '\t%s\n' "$a"; done
} >> "$log"

if [ "$1" = "--socket" ]; then shift 2; fi
cmd="$1"

if [ -n "${FAKE_CMUX_FAIL:-}" ] && [ "$cmd" = "$FAKE_CMUX_FAIL" ]; then
    cat "$fx/${FAKE_CMUX_FAIL_WITH:-err_not_found.txt}" >&2
    exit 1
fi

case "$cmd" in
    tree)
        if [ "$2" = "--all" ]; then cat "$fx/tree_all.json"; else cat "$fx/tree_workspace.json"; fi ;;
    new-workspace) cat "$fx/new_workspace.txt" ;;
    new-split) cat "$fx/new_split.txt" ;;
    identify) cat "$fx/identify.json" ;;
    read-screen) cat "$fx/read_screen.json" ;;
    rpc) cat "$fx/terminal_paste.json" ;;
    rename-tab) cat "$fx/rename_tab.txt" ;;
    send-key) cat "$fx/send_key.txt" ;;
    close-surface) cat "$fx/close_surface.json" ;;
    close-workspace) cat "$fx/close_workspace.json" ;;
    sessions) cat "$fx/${FAKE_CMUX_SESSIONS:-sessions.json}" ;;
    ping) echo PONG ;;
    notify) printf '{\n  "surface_id" : "%s",\n  "workspace_id" : "%s"\n}\n' 9368A7E3-DCC1-4510-8DD6-DB8A819770F8 3543ECC9-B2A1-4751-9961-1695A3EC1222 ;;
    *)
        printf "Error: Unknown command '%s'. Run 'cmux --help' for the full command list.\n" "$cmd" >&2
        exit 2 ;;
esac

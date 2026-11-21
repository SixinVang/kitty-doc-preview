#!/usr/bin/env bash
# docp-pane.sh — open/close/list docp preview panes inside Rex, idempotently.
#
#   docp-pane.sh open  FILE [--page N] [--ratio R] [--theme NAME] [--reopen]
#   docp-pane.sh close FILE
#   docp-pane.sh close-block BLOCK_ID
#   docp-pane.sh list
#
# Exit codes: 0 ok, 2 not inside Rex, 3 bad input, 4 rex/docp failure.
set -uo pipefail

DOCP_BIN=${DOCP_BIN:-$(command -v docp || echo "$HOME/.local/bin/docp")}
SESSION=${REX_SESSION:-}
BLOCK=${REX_BLOCK:-}
LABEL_PREFIX="docp: "

die() { printf 'docp-pane: %s\n' "$*" >&2; exit "${2:-3}"; }

need_rex() {
  [ -n "$SESSION" ] || die "REX_SESSION is unset: not running inside a Rex pane" 2
  command -v rex >/dev/null 2>&1 || die "rex CLI not found on PATH" 2
}

abs() { (cd "$(dirname "$1")" 2>/dev/null && printf '%s/%s' "$PWD" "$(basename "$1")"); }

label_for() { printf '%s%s' "$LABEL_PREFIX" "$(basename "$1")"; }

# Print the block ids whose label matches the file's preview pane.
find_panes() {
  local label=$1
  rex block ls -s "$SESSION" --json 2>/dev/null |
    jq -r --arg l "$label" '.blocks[] | select(.label == $l) | .block_id' 2>/dev/null
}

is_alive() {
  rex block inspect -s "$SESSION" "$1" --json >/dev/null 2>&1
}

cmd_open() {
  local file=${1:-}; shift || true
  [ -n "$file" ] || die "usage: docp-pane.sh open FILE [--page N] [--ratio R] [--theme NAME] [--reopen]"
  need_rex
  [ -e "$file" ] || die "no such file: $file" 3
  local path; path=$(abs "$file")
  [ -n "$path" ] || die "cannot resolve path: $file" 3

  local ratio=40 page="" theme="" reopen=0
  while [ $# -gt 0 ]; do
    case $1 in
      --page) page=${2:-}; shift 2 ;;
      --ratio) ratio=${2:-}; shift 2 ;;
      --theme) theme=${2:-}; shift 2 ;;
      --reopen) reopen=1; shift ;;
      *) die "unknown option: $1" 3 ;;
    esac
  done

  # docp only needs graphics for the interactive view; PDF has no text mode yet.
  case $path in
    *.pdf|*.PDF) [ -n "$theme" ] || theme=dark ;;
  esac

  local label; label=$(label_for "$path")
  local existing; existing=$(find_panes "$label")

  if [ -n "$existing" ] && [ "$reopen" -eq 0 ]; then
    local id; id=$(printf '%s\n' "$existing" | head -1)
    if is_alive "$id"; then
      echo "$id"
      printf 'reused existing preview pane (docp reloads the file on save)\n' >&2
      return 0
    fi
  fi
  # Stale or forced: close every pane holding this file, then reopen.
  local id
  for id in $existing; do
    rex block close -s "$SESSION" "$id" >/dev/null 2>&1
  done

  local args=()
  [ -n "$page" ] && args+=(--page "$page")
  [ -n "$theme" ] && args+=(--theme "$theme")

  local out
  out=$(rex split -s "$SESSION" -b "$BLOCK" --split=right --ratio "$ratio" \
        --label "$label" --focus=false --json -- \
        "$DOCP_BIN" ${args[@]+"${args[@]}"} "$path" 2>&1) || {
    printf '%s\n' "$out" >&2
    die "rex split failed" 4
  }
  local newid
  newid=$(printf '%s' "$out" | jq -r '.block_ids[0] // empty')
  [ -n "$newid" ] || die "rex split returned no block id: $out" 4

  # docp prints its errors and exits when the pane cannot do graphics.
  local screen=""
  local i
  for i in 1 2 3 4 5 6; do
    sleep 0.4
    screen=$(rex capture -s "$SESSION" -b "$newid" --trim 2>/dev/null | head -20)
    case $screen in
      *"does not support the kitty graphics protocol"*|*"no such file"*|*"docp: "*)
        printf '%s\n' "$screen" >&2
        rex block close -s "$SESSION" "$newid" >/dev/null 2>&1
        die "docp failed to render; see the message above" 4 ;;
    esac
    [ -n "$screen" ] && break
  done

  echo "$newid"
  printf 'opened docp preview: %s\n' "$label" >&2
}

cmd_close() {
  local file=${1:-}
  [ -n "$file" ] || die "usage: docp-pane.sh close FILE" 3
  need_rex
  local label; label=$(label_for "$(abs "$file")")
  local ids; ids=$(find_panes "$label")
  [ -n "$ids" ] || { printf 'no preview pane for %s\n' "$label" >&2; return 0; }
  local id
  for id in $ids; do rex block close -s "$SESSION" "$id" >/dev/null 2>&1 && echo "$id"; done
}

cmd_close_block() {
  local id=${1:-}
  [ -n "$id" ] || die "usage: docp-pane.sh close-block BLOCK_ID" 3
  need_rex
  rex block close -s "$SESSION" "$id" >/dev/null 2>&1 && echo "$id"
}

cmd_list() {
  need_rex
  rex block ls -s "$SESSION" --json |
    jq -r --arg p "$LABEL_PREFIX" \
      '.blocks[] | select(.label | startswith($p)) | "\(.block_id)\t\(.label)"'
}

sub=${1:-}; shift || true
case $sub in
  open) cmd_open "$@" ;;
  close) cmd_close "$@" ;;
  close-block) cmd_close_block "$@" ;;
  list) cmd_list ;;
  *) die "usage: docp-pane.sh {open FILE [opts] | close FILE | close-block ID | list}" 3 ;;
esac

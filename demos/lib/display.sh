#!/usr/bin/env bash
# Lightweight display helpers safe to source before demo build setup.

demo_header() {
  local title=$1
  local width=42
  local title_width=$((width - 8))
  local padding
  local left
  local right

  if [ "${#title}" -gt "$title_width" ]; then
    printf 'demo title is too wide for the %d-column header: %s\n' \
      "$width" "$title" >&2
    return 1
  fi

  padding=$((title_width - ${#title}))
  left=$((padding / 2))
  right=$((padding - left))

  printf '\n%*s\n' "$width" '' | tr ' ' '='
  printf '=== %*s%s%*s ===\n' "$left" '' "$title" "$right" ''
  printf '%*s\n\n' "$width" '' | tr ' ' '='
}

#!/usr/bin/env bash
# Resolve the shared QEMU demo asset directory without hiding it behind Hermit's
# private guest /tmp. This file may be sourced or executed with one ROOT argument.

qemu_default_assets() {
  local root digest
  root="$(cd "$1" && pwd -P)"
  case "$root/" in
    /tmp/*)
      digest="$(printf '%s' "$root" | sha256sum | cut -c1-12)"
      printf '/var/tmp/hermit-demo-qemu-%s-%s\n' "$UID" "$digest"
      ;;
    *)
      printf '%s/ignored/qemu-linux\n' "$root"
      ;;
  esac
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  if [[ $# -ne 1 ]]; then
    echo "usage: $0 ROOT" >&2
    exit 2
  fi
  qemu_default_assets "$1"
fi

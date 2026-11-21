#!/bin/sh
set -eu
usage() {
  echo "usage: $0 [-v] file" >&2
  exit 2
}
while getopts v opt; do
  case $opt in
    v) verbose=1 ;;
    *) usage ;;
  esac
done
[ -n "${verbose:-}" ] && echo verbose

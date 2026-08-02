#!/bin/sh
set -eu
case "${1-}" in
  generate|smoke|full|report|test) exec python3 "$(dirname "$0")/bench.py" "$@" ;;
  *) printf '%s\n' 'usage: bench/run.sh {generate|smoke|full|report|test} ...' >&2; exit 2 ;;
esac

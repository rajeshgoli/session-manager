#!/bin/sh
# Stable entry point for the harness-independent production sandbox generator.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$script_dir/wall_profile.py" "$@"

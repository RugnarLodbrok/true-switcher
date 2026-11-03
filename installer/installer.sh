#!/usr/bin/env bash
set -eu
for python in python3 python3.14 python3.13 python3.12 python3.11 python3.10 python3.9; do
    if command -v "$python" >/dev/null 2>&1 && "$python" -c 'import sys; sys.exit(sys.version_info < (3, 9))'; then
        exec "$python" "$(dirname "$0")/installer.py" "$@"
    fi
done
printf '%s\n' 'Python 3.9 or newer is required.' >&2
exit 1

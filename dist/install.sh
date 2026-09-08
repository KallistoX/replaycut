#!/bin/sh
# Installs replaycut for this user: files, desktop entry, optional autostart.
# Everything happens in `replaycut install`; this only finds the executable
# next to itself. Nothing needs root.
set -e
cd "$(dirname "$0")"
chmod +x ./replaycut
exec ./replaycut install "$@"

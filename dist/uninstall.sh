#!/bin/sh
# Removes the installation. Settings, clips and credentials stay;
# `replaycut uninstall --purge` removes them too.
set -e
cd "$(dirname "$0")"
exec ./replaycut uninstall "$@"

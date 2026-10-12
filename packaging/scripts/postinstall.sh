#!/bin/sh
# The agent powers the machine off when it exits, so the package never
# enables or starts the unit. Runner images enable it themselves with
# `systemctl enable hatchrun-agent`.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

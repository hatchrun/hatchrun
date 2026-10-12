#!/bin/sh

# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: Apache-2.0

# The agent powers the machine off when it exits, so the package never
# enables or starts the unit. Runner images enable it themselves with
# `systemctl enable hatchrun-agent`.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

# Writes connect.yaml with the AWS account that owns the hatchrun images:
# AWS Marketplace's, which owns the copies of every Marketplace image.
# The control plane role may launch only images of that account.
#
#   render.sh IMAGE_OWNER > connect.yaml

owner=$1
if ! [[ ${owner} =~ ^[0-9]{12}$ ]]; then
  echo "render.sh: IMAGE_OWNER must be a 12-digit AWS account ID" >&2
  exit 1
fi

OWNER="${owner}" awk '
  /ec2:Owner: "[0-9]+" # image-owner$/ {
    sub(/"[0-9]+"/, "\"" ENVIRON["OWNER"] "\"")
  }
  { print }
' "$(dirname "$0")/connect.yaml"

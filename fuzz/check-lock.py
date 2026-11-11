#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Fails when fuzz/Cargo.lock and Cargo.lock disagree on a shared crate.

The fuzzers must link the same versions the shipped binaries do, or a
crash in one version goes unseen and a crash in the other does not
reproduce.
"""

import sys
import tomllib
from pathlib import Path

root = Path(__file__).resolve().parent.parent


def versions(lock):
    with open(lock, "rb") as f:
        packages = tomllib.load(f)["package"]
    found = {}
    for p in packages:
        found.setdefault(p["name"], set()).add(p["version"])
    return found


shipped = versions(root / "Cargo.lock")
fuzz = versions(root / "fuzz" / "Cargo.lock")
drift = [
    f"{name}: fuzz {', '.join(sorted(fuzz[name]))}, root {', '.join(sorted(shipped[name]))}"
    for name in sorted(fuzz.keys() & shipped.keys())
    if not fuzz[name] <= shipped[name]
]
if drift:
    print("fuzz/Cargo.lock differs from Cargo.lock:", *drift, sep="\n  ", file=sys.stderr)
    print("See fuzz/README.md to sync it.", file=sys.stderr)
    sys.exit(1)

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

/** The hatchrun mark, micro cut for 16 to 40 px. Source: hatchrun/brand. */
export function Logo({ size = 28 }: { size?: number }) {
  return <img className="logo" src="/brand/hatchrun-mark-micro.svg" width={size} height={size} alt="" />;
}

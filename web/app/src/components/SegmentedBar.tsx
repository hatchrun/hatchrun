// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { cn } from "@/lib/utils";
import { STATUS, type Status } from "./StatusPill";

export interface Segment {
  status: Status;
  count: number;
}

/**
 * One bar that splits by share, for aggregates too large for a grid of
 * slots (past about 40). Segments of zero are dropped. The bar is
 * labelled with the counts for assistive tech; show the numbers beside
 * it for sighted users.
 */
export function SegmentedBar({
  segments,
  height = 8,
  className,
}: {
  segments: Segment[];
  height?: number;
  className?: string;
}) {
  const used = segments.filter((segment) => segment.count > 0);
  const total = used.reduce((sum, segment) => sum + segment.count, 0);
  const label = used.map((segment) => `${segment.count} ${STATUS[segment.status].label}`).join(", ") || "none";
  return (
    <div
      role="img"
      aria-label={label}
      style={{ height }}
      className={cn("flex w-full gap-px overflow-hidden rounded-xs bg-rule", className)}
    >
      {used.map((segment) => (
        <span
          key={segment.status}
          className={STATUS[segment.status].fill}
          style={{ width: `${(segment.count / total) * 100}%`, minWidth: 2 }}
        />
      ))}
    </div>
  );
}

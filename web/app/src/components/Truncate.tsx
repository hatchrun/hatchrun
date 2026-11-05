// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { cn } from "@/lib/utils";
import { CopyButton } from "./CopyButton";

/**
 * One line cut with an ellipsis; the full value shows on hover (title).
 * Use for repo, branch, job and secret names. The parent must let it
 * shrink (`min-w-0` on a flex child, or a width on the cell). With
 * `copy`, a copy button follows; branches and SHAs copy in one click.
 */
export function Truncate({
  value,
  children,
  copy = false,
  mono = false,
  className,
}: {
  /** The full text: the tooltip and what copy takes. */
  value: string;
  /** What to show instead of `value`, for example a short SHA. */
  children?: ReactNode;
  copy?: boolean;
  mono?: boolean;
  className?: string;
}) {
  const text = (
    <span title={value} className={cn("min-w-0 truncate", mono && "font-mono text-[13px]", className)}>
      {children ?? value}
    </span>
  );
  if (!copy) {
    return text;
  }
  return (
    <span className="inline-flex min-w-0 max-w-full items-center gap-1">
      {text}
      <CopyButton text={value} label={`Copy ${value}`} size={13} />
    </span>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronLeft, ChevronRight } from "lucide-react";
import { useState } from "react";

import type { Page } from "@/api";
import { abbreviate } from "@/lib/format";
import { cn } from "@/lib/utils";

/**
 * The cursor stack of a paged list. The API only goes forward, so the
 * hook remembers the cursors of the pages already seen to go back.
 * `reset` (call it when a filter changes) returns to the first page.
 */
export function useCursors() {
  const [stack, setStack] = useState<(string | null)[]>([null]);
  const cursor = stack[stack.length - 1] ?? null;
  return {
    cursor,
    /** 0 for the first page. */
    index: stack.length - 1,
    hasPrev: stack.length > 1,
    next: (next: string | null) => next && setStack((current) => [...current, next]),
    prev: () => setStack((current) => (current.length > 1 ? current.slice(0, -1) : current)),
    reset: () => setStack([null]),
    /** A key that changes with the page, for `useLoad`. */
    key: stack.join("/"),
  };
}

/**
 * "1-50 of 12.4k" with newer and older buttons. Pass `range` ("last 7
 * days", "newest first") to say what the total covers. The list never
 * grows past one page; this replaces "load more".
 */
export function CursorPager({
  page,
  index,
  pageSize = 50,
  hasPrev,
  onPrev,
  onNext,
  range,
  noun = "items",
  prevLabel = "Newer",
  nextLabel = "Older",
  className,
}: {
  page: Page<unknown>;
  /** Page index from `useCursors`, 0-based. */
  index: number;
  pageSize?: number;
  hasPrev: boolean;
  onPrev: () => void;
  onNext: (cursor: string | null) => void;
  range?: string;
  noun?: string;
  /** Button words; "Newer" and "Older" for lists that go back in time. */
  prevLabel?: string;
  nextLabel?: string;
  className?: string;
}) {
  const first = index * pageSize + 1;
  const last = index * pageSize + page.items.length;
  const button =
    "inline-flex h-8 cursor-pointer items-center gap-1.5 rounded-sm border border-control bg-raised px-2.5 font-mono text-xs tracking-[0.04em] text-fg uppercase hover:border-fg-3 disabled:cursor-default disabled:opacity-40 disabled:hover:border-control";
  return (
    <div className={cn("flex flex-wrap items-center justify-between gap-3", className)}>
      <span className="font-mono text-[13px] text-fg-2" aria-live="polite">
        {page.items.length === 0 ? `0 ${noun}` : `${first.toLocaleString()}-${last.toLocaleString()} of ${abbreviate(page.total)} ${noun}`}
        {range && <span className="text-fg-3"> · {range}</span>}
      </span>
      <span className="flex gap-2">
        <button type="button" className={button} disabled={!hasPrev} onClick={onPrev}>
          <ChevronLeft size={14} aria-hidden="true" />
          {prevLabel}
        </button>
        <button type="button" className={button} disabled={page.next === null} onClick={() => onNext(page.next)}>
          {nextLabel}
          <ChevronRight size={14} aria-hidden="true" />
        </button>
      </span>
    </div>
  );
}

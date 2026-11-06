// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/**
 * The frame of a screen inside the Shell: the 24px page title with its
 * meta line on the left, `actions` (buttons) on the right, then the
 * content with a 16px rhythm. `narrow` caps the width for form pages.
 */
export function Page({
  title,
  meta,
  actions,
  narrow = false,
  nav,
  children,
}: {
  title: string;
  /** One line under or beside the title: counts, live state. */
  meta?: ReactNode;
  actions?: ReactNode;
  narrow?: boolean;
  /** A sub-navigation above the title (Settings). */
  nav?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className={cn("mx-auto flex w-full flex-col gap-4", narrow ? "max-w-[880px]" : "max-w-[1400px]")}>
      {/* Settings pages keep the sub-nav and title in the same 880px column, wide content below stays wide. */}
      <div className={cn("flex flex-col gap-4", nav && !narrow && "mx-auto w-full max-w-[880px]")}>
      {nav}
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1">
          <h1 className="page-title">{title}</h1>
          {meta && <span className="font-mono text-[13px] text-fg-2">{meta}</span>}
        </div>
        {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
      </div>
      </div>
      {children}
    </div>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Class names of the run screen's buttons and fields: mono 12px labels,
// as in the design. Pressed buttons take the accent.

const BASE =
  "inline-flex cursor-pointer items-center justify-center gap-1.5 rounded-sm border font-mono text-xs tracking-[0.04em] whitespace-nowrap uppercase transition-colors focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-brand disabled:cursor-not-allowed disabled:opacity-50";

export const BTN = `${BASE} h-8 border-control bg-raised px-3 text-fg hover:border-fg-3 aria-pressed:border-brand aria-pressed:text-brand`;
export const BTN_SM = `${BASE} h-8 border-control bg-raised px-2 text-fg hover:border-fg-3 aria-pressed:border-brand aria-pressed:text-brand`;
export const BTN_PRIMARY = `${BASE} h-8 border-brand bg-brand px-3 font-bold text-[#141517] hover:bg-brand-hover`;
export const FIELD =
  "h-8 rounded-sm border border-control bg-background px-2 font-mono text-[13px] text-fg placeholder:text-fg-3 focus:border-brand focus:outline-none";

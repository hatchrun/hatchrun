// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/** Runner time: "12.5 min" under an hour, "3h 07m" from an hour on. */
export function runnerTime(seconds: number): string {
  if (seconds < 3600) {
    return `${(seconds / 60).toFixed(1)} min`;
  }
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return `${hours.toLocaleString()}h ${String(minutes).padStart(2, "0")}m`;
}

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

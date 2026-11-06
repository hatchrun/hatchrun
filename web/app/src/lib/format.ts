// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Number and time formatting shared by the screens.

/** 12400 -> "12.4k", 1_250_000 -> "1.3M", 999 -> "999". Counts only; keep exact figures for money. */
export function abbreviate(count: number): string {
  const unit = (value: number, suffix: string) => {
    const text = value >= 100 ? Math.round(value).toString() : value.toFixed(1).replace(/\.0$/, "");
    return `${text}${suffix}`;
  };
  if (count < 1000) {
    return String(count);
  }
  if (count < 1_000_000) {
    return unit(count / 1000, "k");
  }
  return unit(count / 1_000_000, "M");
}

/** 2m 14s, 48s, 1h 07m. For a duration in milliseconds; "-" for null. */
export function duration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) {
    return "-";
  }
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ${String(seconds % 60).padStart(2, "0")}s`;
  }
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/** "just now", "4 min ago", "3 h ago", else the date. */
export function ago(ms: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - ms) / 1000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 48) {
    return `${hours} h ago`;
  }
  return new Date(ms).toLocaleDateString();
}

/** A long span in words: "2 min 14 s". */
export function span(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) {
    return `${seconds} s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ${seconds % 60} s`;
  }
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

export function bytes(value: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

export const shortSha = (sha: string) => sha.slice(0, 7);

/** refs/heads/main -> main. */
export const shortRef = (ref: string) => ref.replace(/^refs\/(heads|tags)\//, "");

const pad = (n: number) => String(n).padStart(2, "0");

/** 2026-11-03 for a Unix ms time, in the viewer's time zone. */
export function isoDate(ms: number): string {
  const date = new Date(ms);
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** 2026-11-03 14:05, the absolute form for a hover title. */
export function isoDateTime(ms: number): string {
  const date = new Date(ms);
  return `${isoDate(ms)} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

const WEEK = 7 * 86_400_000;

/**
 * The one date format of the tables: relative ("4 min ago", "3 d ago")
 * under 7 days, else the ISO-like date "2026-11-03". Put `isoDateTime(ms)`
 * in the title for the absolute time on hover.
 */
export function tableDate(ms: number, now = Date.now()): string {
  const age = now - ms;
  if (age >= WEEK) {
    return isoDate(ms);
  }
  const seconds = Math.max(0, Math.round(age / 1000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours} h ago`;
  }
  return `${Math.floor(hours / 24)} d ago`;
}

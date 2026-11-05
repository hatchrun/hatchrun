// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useSyncExternalStore } from "react";

import { getItem, setItem } from "./storage";

export type Theme = "dark" | "light" | "system";

export const THEMES: { id: Theme; label: string; swatch: string }[] = [
  { id: "dark", label: "Dark", swatch: "#141517" },
  { id: "light", label: "Light", swatch: "#e3ded3" },
  { id: "system", label: "Match system", swatch: "linear-gradient(135deg, #141517 50%, #e3ded3 50%)" },
];

const listeners = new Set<() => void>();

export function getTheme(): Theme {
  const stored = getItem("theme");
  return stored === "light" || stored === "system" ? stored : "dark";
}

/** Sets `data-theme` on the document: tokens in index.css switch on it. */
export function applyTheme(theme: Theme = getTheme()) {
  const light = theme === "light" || (theme === "system" && window.matchMedia("(prefers-color-scheme: light)").matches);
  document.documentElement.dataset.theme = light ? "light" : "dark";
}

export function setTheme(theme: Theme) {
  setItem("theme", theme);
  applyTheme(theme);
  for (const listener of listeners) {
    listener();
  }
}

export function useTheme(): Theme {
  return useSyncExternalStore(
    (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    getTheme,
  );
}

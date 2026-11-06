// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

/** Whether the browser runs on an Apple platform, where the shortcut key is Command. */
export const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent);

/** The shortcut modifier as shown to the user: ⌘ on a Mac, Ctrl elsewhere. */
export const MOD = IS_MAC ? "⌘" : "Ctrl";

/** "⌘K" on a Mac, "Ctrl K" elsewhere. */
export const shortcut = (key: string) => (IS_MAC ? `⌘${key}` : `Ctrl ${key}`);

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Dialog } from "radix-ui";

const SHORTCUTS: [string, string][] = [
  ["⌘K or Ctrl K", "Search: jump to a repo, run or page"],
  ["⌘O or Ctrl O", "Switch organization"],
  ["?", "Keyboard shortcuts"],
  ["↑ ↓ Enter Esc", "Move, pick and close in every list and menu"],
];

export function ShortcutsDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/50" />
        <Dialog.Content
          aria-describedby={undefined}
          className="fixed top-[18vh] left-1/2 z-50 w-[440px] max-w-[calc(100vw-16px)] -translate-x-1/2 rounded-md border border-control bg-panel p-4 shadow-[0_10px_28px_rgba(0,0,0,0.55)] outline-none"
        >
          <Dialog.Title className="section-title mb-3">Keyboard shortcuts</Dialog.Title>
          <dl className="flex flex-col gap-2">
            {SHORTCUTS.map(([keys, what]) => (
              <div key={keys} className="flex items-baseline justify-between gap-4">
                <dd className="text-sm text-fg-2">{what}</dd>
                <dt className="font-mono text-[13px] whitespace-nowrap text-fg">{keys}</dt>
              </div>
            ))}
          </dl>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

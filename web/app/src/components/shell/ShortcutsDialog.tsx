// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { X } from "lucide-react";
import { Dialog } from "radix-ui";

import { shortcut } from "@/lib/platform";

const SHORTCUTS: [string, string][] = [
  [shortcut("K"), "Search: jump to a repo, run or page"],
  [shortcut("O"), "Switch organization"],
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
          <div className="mb-3 flex items-center justify-between gap-3">
            <Dialog.Title className="section-title">Keyboard shortcuts</Dialog.Title>
            <Dialog.Close aria-label="Close" className="flex size-7 cursor-pointer items-center justify-center rounded-sm border-0 bg-transparent text-fg-3 hover:bg-raised hover:text-fg">
              <X size={16} aria-hidden="true" />
            </Dialog.Close>
          </div>
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

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Dialog } from "radix-ui";
import { useSyncExternalStore, type ReactNode } from "react";

/** Below this width an editor panel is a drawer over the page, not a column beside the list. */
export const PANEL_MIN_WIDTH = 1100;

const query = `(min-width: ${PANEL_MIN_WIDTH}px)`;

function subscribe(listener: () => void) {
  const media = window.matchMedia(query);
  media.addEventListener("change", listener);
  return () => media.removeEventListener("change", listener);
}

/** Whether the window is wide enough for the panel to sit beside the list. */
export function useRoomForPanel(): boolean {
  return useSyncExternalStore(subscribe, () => window.matchMedia(query).matches);
}

/**
 * An editor panel. With room it renders in place (the caller puts it in
 * a grid column). Without, it is a full-height drawer from the right over
 * a dimmed page: focus stays inside, Esc and a click on the dim close it.
 * `children` is the panel; it must have a heading with the id `labelledBy`.
 */
export function SidePanel({ inline, labelledBy, onClose, children }: { inline: boolean; labelledBy: string; onClose: () => void; children: ReactNode }) {
  if (inline) {
    return <>{children}</>;
  }
  return (
    <Dialog.Root open onOpenChange={(open) => !open && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/60" />
        <Dialog.Content
          aria-labelledby={labelledBy}
          aria-describedby={undefined}
          className="fixed inset-y-0 right-0 z-50 w-[420px] max-w-full overflow-y-auto border-l border-control bg-panel p-4 shadow-[-12px_0_32px_rgba(0,0,0,0.5)] outline-none"
        >
          <Dialog.Title className="sr-only">Editor</Dialog.Title>
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Popover as Radix } from "radix-ui";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/**
 * A popover on the raised surface: Esc and outside click close it, focus
 * returns to the trigger. `trigger` is the button; pass `open` and
 * `onOpenChange` to control it.
 */
export function Popover({
  trigger,
  children,
  open,
  onOpenChange,
  width = 260,
  side = "bottom",
  align = "start",
  className,
}: {
  trigger: ReactNode;
  children: ReactNode;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  width?: number | string;
  side?: "top" | "bottom";
  align?: "start" | "end";
  className?: string;
}) {
  return (
    <Radix.Root open={open} onOpenChange={onOpenChange}>
      <Radix.Trigger asChild>{trigger}</Radix.Trigger>
      <Radix.Portal>
        <Radix.Content
          side={side}
          align={align}
          sideOffset={4}
          collisionPadding={8}
          style={{ width, maxWidth: "calc(100vw - 16px)" }}
          className={cn(
            "z-50 rounded-md border border-control bg-raised text-left shadow-[0_10px_28px_rgba(0,0,0,0.55)] outline-none",
            className,
          )}
        >
          {children}
        </Radix.Content>
      </Radix.Portal>
    </Radix.Root>
  );
}

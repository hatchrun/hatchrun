// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";

import { cn } from "@/lib/utils";

export interface PickItem {
  key: string;
  label: ReactNode;
  /** Right-aligned mono note, such as "recent" or "current". */
  note?: ReactNode;
  /** The row is shown in a sticky group heading when it differs from the row before. */
  group?: string;
  /** Selected rows get the accent bar and tint. */
  selected?: boolean;
  /** Mono label, for repo and branch names. */
  mono?: boolean;
  /** Full text for the title tooltip when the label is cut off. */
  title?: string;
}

/**
 * A search box over a listbox, the body of every picker popover (org
 * switcher, scope picker, filter chips, command palette). Focus stays in
 * the input; Arrow keys move the active row, Enter picks it. The parent
 * owns the query and filters `items`; this only renders and navigates.
 */
export function PickList({
  items,
  query,
  onQuery,
  onPick,
  placeholder,
  label,
  empty = "No matches.",
  footer,
  header,
  autoFocus = true,
  maxHeight = 320,
  onKeyDown,
}: {
  items: PickItem[];
  query: string;
  onQuery: (query: string) => void;
  onPick: (item: PickItem) => void;
  placeholder: string;
  /** Accessible name of the input and the list. */
  label: string;
  empty?: ReactNode;
  footer?: ReactNode;
  /** Extra controls between the input and the list. */
  header?: ReactNode;
  autoFocus?: boolean;
  maxHeight?: number;
  /** Sees keys the list did not handle. */
  onKeyDown?: (event: KeyboardEvent<HTMLInputElement>) => void;
}) {
  const id = useId();
  // The active row resets to the first when the query or the list changes.
  const resetKey = `${query}\u0000${items.length}`;
  const [moved, setMoved] = useState({ key: resetKey, index: 0 });
  const active = moved.key === resetKey ? moved.index : 0;
  const setActive = (update: number | ((index: number) => number)) =>
    setMoved({ key: resetKey, index: typeof update === "number" ? update : update(active) });
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const keys = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((index) => Math.min(index + 1, items.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((index) => Math.max(index - 1, 0));
    } else if (event.key === "Enter" && items[active]) {
      event.preventDefault();
      onPick(items[active]);
    } else {
      onKeyDown?.(event);
    }
  };

  return (
    <div className="flex flex-col">
      <div className="p-2.5">
        <input
          autoFocus={autoFocus}
          role="combobox"
          aria-expanded="true"
          aria-controls={`${id}-list`}
          aria-activedescendant={items[active] ? `${id}-${active}` : undefined}
          aria-label={label}
          placeholder={placeholder}
          value={query}
          onChange={(event) => onQuery(event.target.value)}
          onKeyDown={keys}
          className="h-8 w-full rounded-sm border border-control bg-background px-2 font-mono text-[13px] text-fg outline-none placeholder:text-fg-3 focus:border-brand"
        />
      </div>
      {header}
      <div
        ref={list}
        id={`${id}-list`}
        role="listbox"
        aria-label={label}
        style={{ maxHeight }}
        className="overflow-y-auto border-t border-rule"
      >
        {items.length === 0 && <p className="meta px-3 py-3">{empty}</p>}
        {items.map((item, index) => (
          <div key={item.key}>
            {item.group && item.group !== items[index - 1]?.group && (
              <div className="cap sticky top-0 bg-raised px-3 pt-2 pb-1">{item.group}</div>
            )}
            <div
              id={`${id}-${index}`}
              data-index={index}
              role="option"
              aria-selected={item.selected ?? false}
              title={item.title}
              onMouseMove={() => setActive(index)}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onPick(item)}
              className={cn(
                "flex w-full cursor-pointer items-center gap-2.5 px-3 py-1.5 text-left text-sm text-fg",
                index === active && "bg-rule",
                item.selected && "selected-item",
              )}
            >
              <span className={cn("min-w-0 flex-1 truncate", item.mono ? "font-mono text-[13px]" : "font-semibold")}>
                {item.label}
              </span>
              {item.note && <span className="shrink-0 font-mono text-xs text-fg-3">{item.note}</span>}
            </div>
          </div>
        ))}
      </div>
      {footer && <div className="flex items-center justify-between gap-2 border-t border-rule px-3 py-2 font-mono text-xs text-fg-3">{footer}</div>}
    </div>
  );
}

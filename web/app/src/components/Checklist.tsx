// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState, type ReactNode } from "react";

import { cn } from "@/lib/utils";

export interface Step {
  key: string;
  title: string;
  done: boolean;
  /** The line under the title: what the step does, or what is done. */
  note?: ReactNode;
  /** A link or button at the right of the title row. */
  aside?: ReactNode;
  /** The step's controls. */
  children?: ReactNode;
}

/** "2 / 4 done" for the title row. */
export function ChecklistCount({ steps }: { steps: Step[] }) {
  return (
    <span className="font-mono text-xs text-fg-3">
      <span className="text-lg font-bold text-fg">{steps.filter((step) => step.done).length}</span> / {steps.length} done
    </span>
  );
}

/**
 * Setup steps in one panel with a segmented progress bar above. The
 * first step that is not done is the current one: neutral edge and bright glyph.
 * Done steps get a green check, later ones three dots.
 */
export function Checklist({ steps, label, collapseDone = false }: { steps: Step[]; label: string; collapseDone?: boolean }) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const current = steps.findIndex((step) => !step.done);
  const done = steps.filter((step) => step.done).length;
  return (
    <>
      <div
        role="progressbar"
        aria-label={`${label} progress`}
        aria-valuemin={0}
        aria-valuemax={steps.length}
        aria-valuenow={done}
        className="grid gap-1"
        style={{ gridTemplateColumns: `repeat(${steps.length}, 1fr)` }}
      >
        {steps.map((step, index) => (
          <span key={step.key} className={cn("h-1 rounded-xs", step.done ? "bg-fg-3" : index === current ? "bg-fg-2" : "bg-control")} />
        ))}
      </div>
      <section aria-label={label} className="rounded-md border border-control bg-panel">
        {steps.map((step, index) => {
          const state = step.done ? "done" : index === current ? "current" : "later";
          const folded = collapseDone && step.done && !expanded.has(step.key);
          const foldable = collapseDone && step.done && Boolean(step.children);
          return (
            <div
              key={step.key}
              aria-current={state === "current" ? "step" : undefined}
              className={cn(
                "flex flex-wrap gap-x-3.5 gap-y-2 px-4 py-3.5",
                index > 0 && "border-t border-rule",
                state === "current" && "shadow-[inset_2px_0_0_var(--fg-3)]",
              )}
            >
              <span
                aria-hidden="true"
                className={cn("w-[22px] text-center font-mono font-bold", state === "done" ? "text-fg-2" : state === "current" ? "text-fg" : "text-fg-3")}
              >
                {state === "done" ? "✓" : state === "current" ? "▶" : "•••"}
              </span>
              <div className="flex min-w-0 flex-[1_1_420px] flex-col gap-2">
                <h2 className={cn("m-0 text-[15px] font-bold", state === "later" ? "text-fg-2" : "text-fg")}>
                  {step.title}
                  <span className="sr-only">{step.done ? " (done)" : ""}</span>
                </h2>
                {step.note && <div className="font-mono text-xs text-fg-3">{step.note}</div>}
                {!folded && step.children}
              </div>
              {foldable && (
                <button
                  type="button"
                  aria-expanded={!folded}
                  aria-label={`${folded ? "Change" : "Hide"} ${step.title}`}
                  onClick={() =>
                    setExpanded((prev) => {
                      const next = new Set(prev);
                      if (!next.delete(step.key)) next.add(step.key);
                      return next;
                    })
                  }
                  className="cursor-pointer self-start font-mono text-xs text-fg-2 quiet-underline hover:text-fg"
                >
                  {folded ? "Change" : "Hide"}
                </button>
              )}
              {step.aside}
            </div>
          );
        })}
      </section>
    </>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import type { ReactNode } from "react";

export interface Step {
  key: string;
  title: string;
  done: boolean;
  /** Not open yet; says so instead of its body. */
  soon?: string;
  /** What the step says when done; its body is shown only while open. */
  summary?: ReactNode;
  body: ReactNode;
}

/** The setup steps, one open at a time: the first that is not done. */
export function Checklist({ steps }: { steps: Step[] }) {
  const open = steps.findIndex((step) => !step.done && !step.soon);
  const done = steps.filter((step) => step.done).length;
  return (
    <div className="card checklist">
      <div className="checklist-head">
        <span>
          {done} of {steps.length} done
        </span>
        <span className="progress" aria-hidden="true">
          <span style={{ width: `${(done / steps.length) * 100}%` }} />
        </span>
      </div>
      <ol>
        {steps.map((step, index) => {
          const state = step.done ? "done" : index === open ? "open" : "waiting";
          return (
            <li key={step.key} className={`step step-${state}`} aria-current={state === "open" ? "step" : undefined}>
              <span className="step-mark" aria-hidden="true">
                {step.done ? <Check /> : index + 1}
              </span>
              <div className="step-content">
                <h2 className="step-title">
                  {step.title}
                  <span className="visually-hidden">{step.done ? " (done)" : ""}</span>
                </h2>
                {step.done ? step.summary && <div className="step-summary">{step.summary}</div> : null}
                {!step.done && state === "open" && <div className="step-body">{step.body}</div>}
                {!step.done && step.soon && <p className="step-summary muted">{step.soon}</p>}
              </div>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function Check() {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="2.2">
      <path d="M3 8.5 6.5 12 13 4.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

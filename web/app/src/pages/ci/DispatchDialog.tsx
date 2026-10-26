// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState, type FormEvent, type RefObject } from "react";
import { Dialog } from "radix-ui";

import { ApiError, dispatchWorkflow, type DispatchRequest } from "@/api";

type Kind = "string" | "boolean" | "integer";

interface InputRow {
  id: number;
  name: string;
  value: string;
  kind: Kind;
}

/** Where a message of the API belongs: an input row, the workflow field, the ref field, or the form. */
type Blame = { row: number } | "workflow" | "ref" | "form";

interface Problem {
  message: string;
  blame: Blame;
}

/** The inputs of the request, or the first row that does not fit its type. */
function inputsOf(rows: InputRow[]): { inputs: DispatchRequest["inputs"] } | { problem: Problem } {
  const inputs: DispatchRequest["inputs"] = {};
  for (const row of rows) {
    const name = row.name.trim();
    if (name === "") {
      continue;
    }
    if (row.kind === "integer") {
      if (!/^-?\d+$/.test(row.value.trim())) {
        return { problem: { message: `Input \`${name}\` must be a whole number`, blame: { row: row.id } } };
      }
      inputs[name] = Number(row.value.trim());
    } else if (row.kind === "boolean") {
      inputs[name] = row.value === "true";
    } else {
      inputs[name] = row.value;
    }
  }
  return { inputs };
}

/**
 * Which field a 422 is about. The API names a workflow as
 * "workflow `name`", an input in backticks, and otherwise speaks of the
 * ref or the config at that ref, which the ref field answers for.
 */
function blame(message: string, rows: InputRow[]): Blame {
  if (/workflow `/.test(message)) {
    return "workflow";
  }
  const named = /`([^`]+)`/.exec(message)?.[1];
  const row = rows.find((candidate) => candidate.name.trim() === named);
  return row ? { row: row.id } : "ref";
}

function problemOf(err: unknown, rows: InputRow[]): Problem {
  if (!(err instanceof ApiError)) {
    return { message: err instanceof Error ? err.message : String(err), blame: "form" };
  }
  switch (err.status) {
    case 422:
      return { message: err.message, blame: blame(err.message, rows) };
    case 401:
      return { message: "You are signed out. Reload the page and sign in again.", blame: "form" };
    case 403:
      return {
        message: "You need push access to this repository to run workflows, or the page's session has expired. Reload and try again.",
        blame: "form",
      };
    case 404:
      return { message: "Not found, or you cannot read this repository.", blame: "form" };
    case 503:
      return {
        message: `GitHub is rate limiting. Try again${err.retryAfter ? ` in ${err.retryAfter} seconds` : " shortly"}.`,
        blame: "form",
      };
    default:
      return { message: err.message, blame: "form" };
  }
}

export function DispatchDialog({
  owner,
  repo,
  open,
  onOpenChange,
  onStarted,
  returnFocus,
}: {
  owner: string;
  repo: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the id of the first run started. */
  onStarted: (run: number) => void;
  /** Focused when the dialog closes; the dialog has no trigger of its own. */
  returnFocus?: RefObject<HTMLElement | null>;
}) {
  const [workflow, setWorkflow] = useState("");
  const [ref, setRef] = useState("");
  const [rows, setRows] = useState<InputRow[]>([]);
  const [nextRow, setNextRow] = useState(1);
  const [problem, setProblem] = useState<Problem | null>(null);
  const [busy, setBusy] = useState(false);

  const change = (id: number, patch: Partial<InputRow>) => {
    setRows((current) => current.map((row) => (row.id === id ? { ...row, ...patch } : row)));
    setProblem(null);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (workflow.trim() === "") {
      setProblem({ message: "Name the workflow to run.", blame: "workflow" });
      return;
    }
    const checked = inputsOf(rows);
    if ("problem" in checked) {
      setProblem(checked.problem);
      return;
    }
    setBusy(true);
    setProblem(null);
    try {
      const started = await dispatchWorkflow(owner, repo, {
        workflow: workflow.trim(),
        ref: ref.trim(),
        inputs: checked.inputs,
      });
      onOpenChange(false);
      const first = started.runs[0];
      if (first !== undefined) {
        onStarted(first);
      }
    } catch (err) {
      setProblem(problemOf(err, rows));
    } finally {
      setBusy(false);
    }
  };

  const at = (where: Blame) =>
    problem && JSON.stringify(problem.blame) === JSON.stringify(where) ? (
      <p className="field-error" role="alert">
        {problem.message}
      </p>
    ) : null;

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/60" />
        <Dialog.Content
          onCloseAutoFocus={(event) => {
            if (returnFocus?.current) {
              event.preventDefault();
              returnFocus.current.focus();
            }
          }}
          className="fixed top-1/2 left-1/2 z-50 max-h-[90vh] w-[min(520px,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2 overflow-y-auto rounded-md border border-rivet bg-graphite p-5">
          <Dialog.Title className="display text-xl">Run workflow</Dialog.Title>
          <Dialog.Description className="muted mb-4">
            Starts a workflow of {owner}/{repo} that has a <code>workflow_dispatch</code> trigger.
          </Dialog.Description>
          <form onSubmit={(event) => void submit(event)} noValidate>
            <div className="field">
              <label className="label" htmlFor="dispatch-workflow">
                Workflow
              </label>
              <input
                id="dispatch-workflow"
                className="input input-mono"
                value={workflow}
                placeholder="ci"
                aria-invalid={problem?.blame === "workflow"}
                onChange={(event) => {
                  setWorkflow(event.target.value);
                  setProblem(null);
                }}
              />
              {at("workflow")}
            </div>
            <div className="field">
              <label className="label" htmlFor="dispatch-ref">
                Branch or tag
              </label>
              <input
                id="dispatch-ref"
                className="input input-mono"
                value={ref}
                placeholder="the default branch"
                aria-invalid={problem?.blame === "ref"}
                onChange={(event) => {
                  setRef(event.target.value);
                  setProblem(null);
                }}
              />
              {at("ref")}
            </div>
            <fieldset className="field">
              <legend className="label">Inputs</legend>
              {rows.map((row) => (
                <div key={row.id} className="mb-2">
                  <div className="flex gap-2">
                    <input
                      className="input input-mono"
                      aria-label="Input name"
                      placeholder="name"
                      value={row.name}
                      onChange={(event) => change(row.id, { name: event.target.value })}
                    />
                    {row.kind === "boolean" ? (
                      <select
                        className="input input-mono"
                        aria-label={`Value of ${row.name || "input"}`}
                        value={row.value === "true" ? "true" : "false"}
                        onChange={(event) => change(row.id, { value: event.target.value })}
                      >
                        <option value="true">true</option>
                        <option value="false">false</option>
                      </select>
                    ) : (
                      <input
                        className="input input-mono"
                        aria-label={`Value of ${row.name || "input"}`}
                        aria-invalid={typeof problem?.blame === "object" && problem.blame.row === row.id}
                        placeholder="value"
                        value={row.value}
                        onChange={(event) => change(row.id, { value: event.target.value })}
                      />
                    )}
                    <select
                      className="input"
                      aria-label="Type"
                      value={row.kind}
                      onChange={(event) =>
                        change(row.id, {
                          kind: event.target.value as Kind,
                          value: event.target.value === "boolean" ? "false" : "",
                        })
                      }
                    >
                      <option value="string">text</option>
                      <option value="boolean">yes or no</option>
                      <option value="integer">number</option>
                    </select>
                    <button
                      type="button"
                      className="button button-small"
                      aria-label={`Remove ${row.name || "input"}`}
                      onClick={() => setRows((current) => current.filter((other) => other.id !== row.id))}
                    >
                      Remove
                    </button>
                  </div>
                  {at({ row: row.id })}
                </div>
              ))}
              <button
                type="button"
                className="button button-small"
                onClick={() => {
                  setRows((current) => [...current, { id: nextRow, name: "", value: "", kind: "string" }]);
                  setNextRow(nextRow + 1);
                }}
              >
                Add input
              </button>
              <p className="field-help">Inputs must match the ones the workflow declares.</p>
            </fieldset>
            {at("form")}
            <div className="inline-actions mt-4 flex justify-end gap-2">
              <Dialog.Close className="button" disabled={busy}>
                Cancel
              </Dialog.Close>
              <button type="submit" className="button button-primary" disabled={busy}>
                {busy ? "Starting" : "Run workflow"}
              </button>
            </div>
          </form>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A repository's deployment environments: which branches may deploy
// to one, whether an organization admin must approve each job, and the
// secrets and variables only that environment's jobs get.

import { useState, type FormEvent } from "react";

import { deleteCiEnvironment, listCiEnvironments, putCiEnvironment, type CiEnvironment } from "@/api";
import { Await } from "@/components/Status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useLoad } from "@/load";
import { SecretsTable } from "./SecretsTable";

const patterns = (text: string) =>
  text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");

export function Environments({ owner, repo }: { owner: string; repo: string }) {
  const [rows, reload] = useLoad(() => listCiEnvironments(owner, repo), `${owner}/${repo}/environments`);
  const [name, setName] = useState("");
  const [editing, setEditing] = useState(false);
  const [branches, setBranches] = useState("");
  const [approval, setApproval] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const fail = (err: unknown) => setError(err instanceof Error ? err.message : String(err));

  const reset = () => {
    setName("");
    setEditing(false);
    setBranches("");
    setApproval(false);
  };

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setSaved(null);
    const given = name.trim();
    try {
      const stored = await putCiEnvironment(owner, repo, given, { branches: patterns(branches), approval });
      reset();
      setSaved(stored.name);
      reload();
    } catch (err) {
      fail(err);
    } finally {
      setBusy(false);
    }
  };

  const edit = (row: CiEnvironment) => {
    setName(row.name);
    setEditing(true);
    setBranches(row.branches.join("\n"));
    setApproval(row.approval);
    setError(null);
    setSaved(null);
  };

  const remove = async (row: CiEnvironment) => {
    if (
      !window.confirm(
        `Delete the environment ${row.name}? Its secrets and variables are deleted too, and jobs that use it fail.`,
      )
    ) {
      return;
    }
    setError(null);
    setSaved(null);
    try {
      await deleteCiEnvironment(owner, repo, row.name);
      if (open === row.name) {
        setOpen(null);
      }
      if (editing && name === row.name) {
        reset();
      }
      reload();
    } catch (err) {
      fail(err);
    }
  };

  const id = `env-${owner}/${repo}`;
  return (
    <section aria-label="Environments" className="flex flex-col gap-3">
      <h2 className="display text-xl">Environments</h2>
      <Await loaded={rows} retry={reload}>
        {(list) =>
          list.length === 0 ? (
            <p className="muted">No environments yet.</p>
          ) : (
            <div className="overflow-hidden rounded-md border border-rivet">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Name</TableHead>
                    <TableHead>Branches</TableHead>
                    <TableHead>Approval</TableHead>
                    <TableHead className="text-right">
                      <span className="sr-only">Actions</span>
                    </TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {list.map((row) => (
                    <TableRow key={row.name}>
                      <TableCell className="font-mono text-sm">{row.name}</TableCell>
                      <TableCell className="font-mono text-sm break-all">
                        {row.branches.length === 0 ? "Any branch" : row.branches.join(", ")}
                      </TableCell>
                      <TableCell className="text-sm">{row.approval ? "Required" : "Not required"}</TableCell>
                      <TableCell className="text-right whitespace-nowrap">
                        <button type="button" className="button button-small" onClick={() => edit(row)}>
                          Edit<span className="sr-only"> {row.name}</span>
                        </button>{" "}
                        <button
                          type="button"
                          className="button button-small"
                          aria-expanded={open === row.name}
                          onClick={() => setOpen(open === row.name ? null : row.name)}
                        >
                          Secrets<span className="sr-only"> of {row.name}</span>
                        </button>{" "}
                        <button type="button" className="button button-small" onClick={() => void remove(row)}>
                          Delete<span className="sr-only"> {row.name}</span>
                        </button>
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          )
        }
      </Await>
      {open !== null && (
        // Keyed by the environment so a half-typed secret does not carry over to another one.
        <div key={open} className="flex flex-col gap-6 rounded-md border border-rivet p-4">
          <h3 className="display text-lg">
            <span className="font-mono">{open}</span>
          </h3>
          <SecretsTable scope={{ owner, repo, environment: open }} kind="secrets" />
          <SecretsTable scope={{ owner, repo, environment: open }} kind="vars" />
        </div>
      )}
      <form onSubmit={save} className="flex flex-col gap-3" aria-label="Add or change an environment">
        <div className="field">
          <label className="label" htmlFor={`${id}-name`}>
            Name
          </label>
          <input
            id={`${id}-name`}
            className="input input-mono"
            value={name}
            onChange={(event) => setName(event.target.value)}
            readOnly={editing}
            autoComplete="off"
            spellCheck={false}
            required
          />
        </div>
        <div className="field">
          <label className="label" htmlFor={`${id}-branches`}>
            Branches
          </label>
          <textarea
            id={`${id}-branches`}
            className="input input-mono"
            rows={3}
            value={branches}
            onChange={(event) => setBranches(event.target.value)}
            spellCheck={false}
            aria-describedby={`${id}-branches-help`}
          />
          <p id={`${id}-branches-help`} className="field-help">
            One pattern per line. Leave empty to allow any branch.
          </p>
        </div>
        <label className="check">
          <input type="checkbox" checked={approval} onChange={(event) => setApproval(event.target.checked)} />
          Require approval from an organization admin
        </label>
        {error !== null && (
          <p className="notice notice-error" role="alert">
            {error}
          </p>
        )}
        {saved !== null && (
          <p className="notice notice-success" role="status">
            Saved {saved}.
          </p>
        )}
        <div className="flex gap-2">
          <button type="submit" className="button button-primary" disabled={busy}>
            {busy ? "Saving" : "Save environment"}
          </button>
          {editing && (
            <button type="button" className="button" onClick={reset}>
              Cancel
            </button>
          )}
        </div>
      </form>
    </section>
  );
}

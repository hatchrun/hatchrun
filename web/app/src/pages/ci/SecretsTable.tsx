// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The secrets or the variables of an organization or a repository.
// A secret's value is write-only: the field is never filled from the
// API and the list holds names and update times. A variable shows its
// value.

import { useRef, useState, type FormEvent } from "react";

import {
  ApiError,
  deleteCiItem,
  listCiSecrets,
  listCiVars,
  putCiItem,
  type CiItemKind,
  type CiScope,
  type CiSecret,
  type CiVar,
} from "@/api";
import { RepoAccess } from "./RepoAccess";
import { Await } from "@/components/Status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useLoad } from "@/load";

type Row = CiSecret | CiVar;

const scopeKey = (scope: CiScope) =>
  "org" in scope ? scope.org : "environment" in scope ? `${scope.owner}/${scope.repo}/env/${scope.environment}` : `${scope.owner}/${scope.repo}`;

/** Which field a 400 is about: the API's name messages all speak of names. */
const aboutName = (message: string) => /\bnames?\b/i.test(message);

interface Problem {
  field: "name" | "value" | "form";
  message: string;
}

export function SecretsTable({ scope, kind }: { scope: CiScope; kind: CiItemKind }) {
  const secret = kind === "secrets";
  const noun = secret ? "secret" : "variable";
  const [rows, reload] = useLoad<Row[]>(
    () => (secret ? listCiSecrets(scope) : listCiVars(scope)),
    `${scopeKey(scope)}/${kind}`,
  );
  const orgScope = "org" in scope ? scope.org : null;
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [repositories, setRepositories] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<Problem | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const valueField = useRef<HTMLInputElement>(null);

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setProblem(null);
    setSaved(null);
    const given = name.trim();
    try {
      await putCiItem(scope, kind, given, value, repositories);
      // The value leaves the page as soon as it is stored.
      setValue("");
      setName("");
      setRepositories(null);
      setSaved(given.toUpperCase());
      reload();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const status = err instanceof ApiError ? err.status : 0;
      setProblem({
        field: status === 400 && aboutName(message) ? "name" : status === 400 || status === 413 || status === 422 ? "value" : "form",
        message,
      });
    } finally {
      setBusy(false);
    }
  };

  const remove = async (row: Row) => {
    if (!window.confirm(`Delete the ${noun} ${row.name}? Workflows that use it will stop getting it.`)) {
      return;
    }
    setProblem(null);
    setSaved(null);
    try {
      await deleteCiItem(scope, kind, row.name);
      reload();
    } catch (err) {
      setProblem({ field: "form", message: err instanceof Error ? err.message : String(err) });
    }
  };

  const replace = (row: Row) => {
    setName(row.name);
    setValue("value" in row ? row.value : "");
    setRepositories(row.repositories);
    setProblem(null);
    setSaved(null);
    valueField.current?.focus();
  };

  const id = `${kind}-${scopeKey(scope)}`;
  return (
    <section aria-label={secret ? "Secrets" : "Variables"} className="flex flex-col gap-3">
      <h2 className="display text-xl">{secret ? "Secrets" : "Variables"}</h2>
      <Await loaded={rows} retry={reload}>
        {(list) =>
          list.length === 0 ? (
            <p className="muted">No {secret ? "secrets" : "variables"} yet.</p>
          ) : (
            <div className="overflow-hidden rounded-md border border-rivet">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Name</TableHead>
                    {!secret && <TableHead>Value</TableHead>}
                    {orgScope !== null && <TableHead>Repositories</TableHead>}
                    <TableHead>Updated</TableHead>
                    <TableHead className="text-right">
                      <span className="sr-only">Actions</span>
                    </TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {list.map((row) => (
                    <TableRow key={row.name}>
                      <TableCell className="font-mono text-sm">{row.name}</TableCell>
                      {!secret && "value" in row && <TableCell className="font-mono text-sm break-all">{row.value}</TableCell>}
                      {orgScope !== null && (
                        <TableCell className="text-sm" title={row.repositories?.join(", ")}>
                          {row.repositories === null ? "All" : `${row.repositories.length} selected`}
                        </TableCell>
                      )}
                      <TableCell className="font-mono text-xs text-fog" title={new Date(row.updatedAt).toLocaleString()}>
                        {new Date(row.updatedAt).toLocaleDateString()}
                      </TableCell>
                      <TableCell className="text-right whitespace-nowrap">
                        <button type="button" className="button button-small" onClick={() => replace(row)}>
                          {secret ? "Replace" : "Edit"}
                          <span className="sr-only"> {row.name}</span>
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
      <form onSubmit={save} className="flex flex-col gap-3" aria-label={`Add or replace a ${noun}`}>
        <div className="field-pair">
          <div className="field">
            <label className="label" htmlFor={`${id}-name`}>
              Name
            </label>
            <input
              id={`${id}-name`}
              className="input input-mono"
              value={name}
              onChange={(event) => setName(event.target.value)}
              autoComplete="off"
              spellCheck={false}
              required
              aria-invalid={problem?.field === "name"}
              aria-describedby={problem?.field === "name" ? `${id}-name-error` : undefined}
            />
            {problem?.field === "name" && (
              <p id={`${id}-name-error`} className="field-error" role="alert">
                {problem.message}
              </p>
            )}
          </div>
          <div className="field">
            <label className="label" htmlFor={`${id}-value`}>
              Value
            </label>
            <input
              id={`${id}-value`}
              ref={valueField}
              className="input input-mono"
              type={secret ? "password" : "text"}
              value={value}
              onChange={(event) => setValue(event.target.value)}
              autoComplete={secret ? "new-password" : "off"}
              spellCheck={false}
              required
              aria-invalid={problem?.field === "value"}
              aria-describedby={problem?.field === "value" ? `${id}-value-error` : undefined}
            />
            {problem?.field === "value" && (
              <p id={`${id}-value-error`} className="field-error" role="alert">
                {problem.message}
              </p>
            )}
          </div>
        </div>
        {orgScope !== null && <RepoAccess org={orgScope} value={repositories} onChange={setRepositories} id={id} />}
        {secret && <p className="field-help">Values are write-only. Saving a name that exists replaces its value.</p>}
        {problem?.field === "form" && (
          <p className="notice notice-error" role="alert">
            {problem.message}
          </p>
        )}
        {saved && (
          <p className="notice notice-success" role="status">
            Saved {saved}.
          </p>
        )}
        <div>
          <button type="submit" className="button button-primary" disabled={busy}>
            {busy ? "Saving" : `Save ${noun}`}
          </button>
        </div>
      </form>
    </section>
  );
}

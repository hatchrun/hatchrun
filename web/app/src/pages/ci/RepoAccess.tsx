// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Which repositories of an organization may use a secret or variable:
// all of them (null) or a chosen list of `owner/repo` names.

import { listCiOrgRepos } from "@/api";
import { Await } from "@/components/Status";
import { useLoad } from "@/load";

export function RepoAccess({
  org,
  value,
  onChange,
  id = "repo-access",
}: {
  org: string;
  value: string[] | null;
  onChange: (value: string[] | null) => void;
  /** Makes the radio names and input ids unique when several are on a page. */
  id?: string;
}) {
  const selected = value !== null;
  const [repos, reload] = useLoad(() => listCiOrgRepos(org), `${org}/repositories`);
  const toggle = (name: string) =>
    onChange(value === null ? [name] : value.includes(name) ? value.filter((seen) => seen !== name) : [...value, name]);

  return (
    <fieldset className="field">
      <legend className="label">Repository access</legend>
      <label className="check">
        <input type="radio" name={`${id}-scope`} checked={!selected} onChange={() => onChange(null)} />
        All repositories
      </label>
      <label className="check">
        <input type="radio" name={`${id}-scope`} checked={selected} onChange={() => onChange(value ?? [])} />
        Selected repositories
      </label>
      {selected && (
        <Await loaded={repos} retry={reload}>
          {(list) =>
            list.length === 0 ? (
              <p className="muted">No repositories have run CI yet.</p>
            ) : (
              <div className="check-grid">
                {list.map((repo) => (
                  <label key={repo.name} className="check">
                    <input type="checkbox" checked={value.includes(repo.name)} onChange={() => toggle(repo.name)} />
                    <span className="font-mono text-sm">{repo.name}</span>
                  </label>
                ))}
              </div>
            )
          }
        </Await>
      )}
    </fieldset>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Which repositories of an organization may use a secret or variable:
// all of them (null) or a chosen list of `owner/repo` names.

import { listCiOrgRepos } from "@/api";
import { Await } from "@/components/Status";
import { useLoad } from "@/load";

/** The org's repositories as checkboxes. Mounted only while a list is chosen, so the read waits for that. */
function RepoChecks({ org, value, onChange }: { org: string; value: string[]; onChange: (value: string[]) => void }) {
  const [repos, reload] = useLoad(() => listCiOrgRepos(org), `${org}/repositories`);
  const toggle = (name: string) =>
    onChange(value.includes(name) ? value.filter((seen) => seen !== name) : [...value, name]);

  return (
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
  );
}

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
      {value !== null && <RepoChecks org={org} value={value} onChange={onChange} />}
      {value !== null && value.length === 0 && (
        <p className="field-help">No repository can use this until you select one.</p>
      )}
    </fieldset>
  );
}

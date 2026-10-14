// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useState } from "react";

import { ApiError, createTenant, listOrgs, type OrgChoice } from "../api";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { navigate } from "../router";

export function NewOrg() {
  const [orgs, reload] = useLoad(listOrgs, "orgs");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const choose = async (org: OrgChoice) => {
    setBusy(org.login);
    setError(null);
    try {
      const tenant = await createTenant(org.login);
      navigate(`/o/${tenant.org.login}`);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String(err));
      setBusy(null);
    }
  };

  return (
    <section className="narrow">
      <p className="eyebrow">Step 1 of 4</p>
      <h1>Choose a GitHub organization</h1>
      <p className="muted">
        hatchrun runs the jobs of one GitHub organization in one AWS account. You can set up
        organizations you own.
      </p>
      {error && (
        <p className="notice notice-error" role="alert">
          {error}
        </p>
      )}
      <Await loaded={orgs} retry={reload}>
        {(list) =>
          list.length === 0 ? (
            <div className="card empty">
              <h2>No organizations you own</h2>
              <p className="muted">
                Only an owner of a GitHub organization can set it up. Ask an owner to sign in, or
                create an organization on GitHub first.
              </p>
              <a className="button" href="https://github.com/account/organizations/new">
                Create an organization on GitHub
              </a>
            </div>
          ) : (
            <ul className="list card">
              {list.map((org) => (
                <li key={org.id} className="list-row">
                  <img className="avatar avatar-square" src={org.avatarUrl} alt="" width={32} height={32} />
                  <span className="list-title">
                    {org.login}
                    {org.tenant && <span className="badge">On hatchrun</span>}
                  </span>
                  <button
                    type="button"
                    className={org.tenant ? "button button-small" : "button button-small button-primary"}
                    disabled={busy !== null}
                    onClick={() => choose(org)}
                  >
                    {busy === org.login ? "Setting up" : org.tenant ? "Open" : "Set up"}
                  </button>
                </li>
              ))}
            </ul>
          )
        }
      </Await>
      <p className="fine">
        Missing an organization? You must be an owner, not a member, and your membership must be
        accepted.
      </p>
    </section>
  );
}

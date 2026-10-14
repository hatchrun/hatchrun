// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect } from "react";

import { listTenants } from "../api";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { Link, navigate, useQuery } from "../router";

const errors: Record<string, string> = {
  "install-state":
    "That App installation was started from another browser session. Open your organization below and install from there.",
};

export function Home() {
  const [tenants, reload] = useLoad(listTenants, "tenants");
  const error = useQuery("error");

  useEffect(() => {
    if (tenants.state !== "ready" || error) {
      return;
    }
    if (tenants.data.length === 0) {
      navigate("/new", true);
    } else if (tenants.data.length === 1 && tenants.data[0]) {
      navigate(`/o/${tenants.data[0].login}`, true);
    }
  }, [tenants, error]);

  return (
    <section className="narrow">
      {error && (
        <p className="notice notice-error" role="alert">
          {errors[error] ?? "Something went wrong."}
        </p>
      )}
      <div className="page-head">
        <h1>Organizations</h1>
        <Link href="/new" className="button button-small">
          Set up another
        </Link>
      </div>
      <Await loaded={tenants} retry={reload}>
        {(list) => (
          <ul className="list card">
            {list.map((tenant) => (
              <li key={tenant.id}>
                <Link href={`/o/${tenant.login}`} className="list-row">
                  <img className="avatar avatar-square" src={tenant.avatarUrl} alt="" width={32} height={32} />
                  <span className="list-title">{tenant.login}</span>
                  <span className="chevron" aria-hidden="true">
                    ›
                  </span>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Await>
    </section>
  );
}

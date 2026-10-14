// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";

import { getTenant, type Tenant } from "../api";
import { Checklist, type Step } from "../components/Checklist";
import { navigate, useQuery } from "../router";
import { ChoosePlan, PlanSummary } from "./Billing";
import { AwsSummary, ConnectAws } from "./ConnectAws";

const errors: Record<string, string> = {
  "install-account":
    "That installation is on a different account. Install the App on this organization.",
};

export function TenantPage({ tenant: loaded }: { tenant: Tenant }) {
  const [tenant, setTenant] = useState(loaded);
  const installed = useQuery("installed");
  const error = useQuery("error");
  const checkout = useQuery("checkout");
  const org = tenant.org.login;
  const confirming = checkout !== null && !tenant.billing.licensed;

  // Back from Polar's checkout: the webhook may land a moment after the
  // browser does, so look again for half a minute.
  useEffect(() => {
    if (!confirming) {
      return;
    }
    let tries = 0;
    const timer = window.setInterval(() => {
      tries += 1;
      getTenant(org).then(setTenant, () => undefined);
      if (tries >= 10) {
        window.clearInterval(timer);
        navigate(`/o/${org}`, true);
      }
    }, 3000);
    return () => window.clearInterval(timer);
  }, [confirming, org]);

  const refresh = (next?: Tenant) => {
    if (next) {
      setTenant(next);
    } else {
      getTenant(org).then(setTenant, () => undefined);
    }
  };

  const steps: Step[] = [
    {
      key: "organization",
      title: "Choose a GitHub organization",
      done: tenant.steps.organization,
      summary: (
        <>
          Linked to <a href={`https://github.com/${org}`}>github.com/{org}</a>.
        </>
      ),
      body: null,
    },
    {
      key: "plan",
      title: "Choose a plan",
      done: tenant.steps.billing,
      summary: <PlanSummary tenant={tenant} />,
      body: <ChoosePlan tenant={tenant} onChange={refresh} confirming={confirming} />,
    },
    {
      key: "aws",
      title: "Connect your AWS account",
      done: tenant.steps.aws,
      summary: <AwsSummary tenant={tenant} />,
      body: <ConnectAws tenant={tenant} onConnected={setTenant} />,
    },
    {
      key: "app",
      title: "Install the GitHub App",
      done: tenant.steps.app,
      summary: tenant.installationId && (
        <>
          Installed on {org}.{" "}
          <a href={`https://github.com/organizations/${org}/settings/installations/${tenant.installationId}`}>
            Configure on GitHub
          </a>
        </>
      ),
      body: <InstallApp tenant={tenant} />,
    },
  ];

  return (
    <section className="narrow">
      {checkout && tenant.billing.licensed && (
        <p className="notice notice-success" role="status">
          Payment received. {org} has a hatchrun licence.
        </p>
      )}
      {installed && (
        <p className="notice notice-success" role="status">
          The hatchrun App is installed on {org}.
        </p>
      )}
      {error && (
        <p className="notice notice-error" role="alert">
          {errors[error] ?? "Something went wrong."}
        </p>
      )}
      <div className="page-head">
        <div>
          <p className="eyebrow">Setup</p>
          <h1>Get {org} running on hatchrun</h1>
        </div>
      </div>
      <Checklist steps={steps} />
    </section>
  );
}

export function InstallApp({ tenant }: { tenant: Tenant }) {
  return (
    <>
      <p>
        The App tells hatchrun when a job asks for a hatchrun runner and registers a runner for it.
        Install it on <strong>{tenant.org.login}</strong>, for all repositories or the ones that
        should use hatchrun.
      </p>
      <ul className="perm-list">
        <li>
          <strong>Self-hosted runners</strong>: read and write, to register one runner per job
        </li>
        <li>
          <strong>Actions</strong>: read, to see queued jobs
        </li>
        <li>
          <strong>Metadata</strong>: read, required by GitHub
        </li>
      </ul>
      <a className="button button-primary" href={tenant.installUrl}>
        Install on GitHub
      </a>
    </>
  );
}

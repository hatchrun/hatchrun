// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import type { Tenant } from "../api";
import { Checklist, type Step } from "../components/Checklist";
import { useQuery } from "../router";

const errors: Record<string, string> = {
  "install-account":
    "That installation is on a different account. Install the App on this organization.",
};

export function TenantPage({ tenant }: { tenant: Tenant }) {
  const installed = useQuery("installed");
  const error = useQuery("error");
  const org = tenant.org.login;

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
      done: false,
      soon: "Billing opens here soon.",
      body: null,
    },
    {
      key: "aws",
      title: "Connect your AWS account",
      done: tenant.steps.aws,
      soon: "AWS setup opens here soon.",
      body: null,
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

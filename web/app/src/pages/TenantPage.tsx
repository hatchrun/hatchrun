// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";

import { getTenant, type Aws, type Tenant, type User } from "../api";
import { Checklist, type Step } from "../components/Checklist";
import { OrgSwitcher } from "../components/OrgSwitcher";
import { PageTabs, Shell } from "../components/Shell";
import { Link, navigate, useQuery } from "../router";
import { ChoosePlan, PlanSummary } from "./Billing";
import { AwsSummary, ConnectAws } from "./ConnectAws";
import { Dashboard } from "./Dashboard";
import { JobPage } from "./JobPage";
import { RunnersPage } from "./Runners";
import { TenantSettings } from "./TenantSettings";

/** Whether every setup step is done. */
export const setUp = (tenant: Tenant) => Object.values(tenant.steps).every(Boolean);

/** A tenant's pages as tabs in the top bar: setup until it is done, then the jobs; runner profiles and settings. */
export function TenantPage({
  user,
  tenant: loaded,
  section,
  item,
}: {
  user: User;
  tenant: Tenant;
  section: string | undefined;
  item?: string;
}) {
  const [tenant, setTenant] = useState(loaded);
  const org = tenant.org.login;
  const ready = setUp(tenant);
  const tabs = [
    { href: `/o/${org}`, label: ready ? "Jobs" : "Setup", current: section === undefined || section === "jobs" },
    { href: `/o/${org}/runners`, label: "Runners", current: section === "runners" },
    { href: `/o/${org}/settings`, label: "Settings", current: section === "settings" },
  ];
  return (
    <Shell
      user={user}
      nav={
        <>
          <OrgSwitcher current={tenant.org} />
          <PageTabs label={`${org} pages`} tabs={tabs} />
        </>
      }
      status={tenant.aws && <CloudAccount org={org} aws={tenant.aws} />}
    >
      {section === "jobs" && item ? (
        <JobPage org={org} id={item} />
      ) : section === "runners" ? (
        <Narrow>
          <RunnersPage tenant={tenant} item={item} />
        </Narrow>
      ) : section === "settings" ? (
        <Narrow>
          <TenantSettings tenant={tenant} onChange={setTenant} />
        </Narrow>
      ) : ready && !new URLSearchParams(window.location.search).has("checkout") ? (
        <Dashboard tenant={tenant} />
      ) : (
        <Narrow>
          <Setup tenant={tenant} setTenant={setTenant} />
        </Narrow>
      )}
    </Shell>
  );
}

/** The column the pages not yet on the new layout were drawn for. */
function Narrow({ children }: { children: React.ReactNode }) {
  return <div className="mx-auto w-full max-w-[1080px] pt-3">{children}</div>;
}

/** The connected cloud account and region; opens Settings. */
function CloudAccount({ org, aws }: { org: string; aws: Aws }) {
  const account = /^arn:aws[a-z-]*:iam::(\d{12}):/.exec(aws.roleArn)?.[1];
  return (
    <Link
      href={`/o/${org}/settings`}
      title="Connected AWS account, opens Settings"
      className="hidden items-center gap-2 font-mono text-xs text-fog hover:text-bone md:flex"
    >
      <span aria-hidden="true" className="size-[7px] rounded-full bg-passed shadow-[0_0_0_3px_var(--passed-halo)]" />
      aws
      {account && <span className="hidden text-bone lg:inline">{account}</span>}
      <span>{aws.region}</span>
    </Link>
  );
}

function Setup({ tenant, setTenant }: { tenant: Tenant; setTenant: (tenant: Tenant) => void }) {
  const installed = useQuery("installed");
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
  }, [confirming, org, setTenant]);

  const refresh = (next?: Tenant) => {
    if (next) {
      setTenant(next);
    } else {
      getTenant(org).then(setTenant, () => undefined);
    }
  };

  const steps: Step[] = [
    {
      key: "app",
      title: "Install hatchrun on GitHub",
      done: tenant.steps.app,
      summary: tenant.installationId && (
        <>
          Installed on <a href={`https://github.com/${org}`}>{org}</a>.{" "}
          <a href={`https://github.com/organizations/${org}/settings/installations/${tenant.installationId}`}>
            Change repositories
          </a>
        </>
      ),
      body: <InstallApp tenant={tenant} />,
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
      key: "runs-on",
      title: "Name a runner profile in runs-on",
      done: false,
      soon: "Once the steps above are done, set runs-on: hatchrun-4cpu-arm64 (or any profile under Runners) and push.",
      body: null,
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
          hatchrun is installed on {org}. Two more steps and your jobs can run.
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

/** Shown when the App is gone from the organization: uninstalled or suspended. */
export function InstallApp({ tenant }: { tenant: Tenant }) {
  const org = tenant.org.login;
  return (
    <>
      <p>
        hatchrun can't reach <strong>{org}</strong> right now. Someone uninstalled the app or
        suspended it, so no runners will start. Install it again, or unsuspend it in the
        organization's settings.
      </p>
      <p className="inline-actions">
        <a className="button button-primary" href={tenant.installUrl}>
          Install on GitHub
        </a>
        <a className="button" href={`https://github.com/organizations/${org}/settings/installations`}>
          Open GitHub settings
        </a>
      </p>
    </>
  );
}

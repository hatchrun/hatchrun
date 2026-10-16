// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState, type ReactNode } from "react";

import { getTenant, type Tenant } from "../api";
import { Checklist, type Step } from "../components/Checklist";
import { Link, navigate, useQuery } from "../router";
import { ChoosePlan, PlanSummary } from "./Billing";
import { AwsSummary, ConnectAws } from "./ConnectAws";
import { Dashboard } from "./Dashboard";
import { JobPage } from "./JobPage";
import { RunnersPage } from "./Runners";
import { TenantSettings } from "./TenantSettings";

/** Whether every setup step is done. */
export const setUp = (tenant: Tenant) => Object.values(tenant.steps).every(Boolean);

/** A tenant's pages behind a sidebar: setup until it is done, then the dashboard and its jobs; runner profiles and settings. */
export function TenantPage({
  tenant: loaded,
  section,
  item,
}: {
  tenant: Tenant;
  section: string | undefined;
  item?: string;
}) {
  const [tenant, setTenant] = useState(loaded);
  const org = tenant.org.login;
  const ready = setUp(tenant);
  const links = [
    {
      href: `/o/${org}`,
      label: ready ? "Jobs" : "Setup",
      icon: ready ? <JobsIcon /> : <SetupIcon />,
      current: section === undefined || section === "jobs",
    },
    { href: `/o/${org}/runners`, label: "Runners", icon: <RunnersIcon />, current: section === "runners" },
    { href: `/o/${org}/settings`, label: "Settings", icon: <SettingsIcon />, current: section === "settings" },
  ];
  return (
    <div className="org">
      <nav className="sidenav" aria-label={`${org} pages`}>
        <p className="sidenav-label">Organization</p>
        <ul>
          {links.map((link) => (
            <li key={link.href}>
              <Link href={link.href} className="sidenav-link" aria-current={link.current ? "page" : undefined}>
                {link.icon}
                <span>{link.label}</span>
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      <div className="org-main">
        {section === "jobs" && item ? (
          <JobPage org={org} id={item} />
        ) : section === "runners" ? (
          <RunnersPage tenant={tenant} item={item} />
        ) : section === "settings" ? (
          <TenantSettings tenant={tenant} onChange={setTenant} />
        ) : ready && !new URLSearchParams(window.location.search).has("checkout") ? (
          <Dashboard tenant={tenant} />
        ) : (
          <Setup tenant={tenant} setTenant={setTenant} />
        )}
      </div>
    </div>
  );
}

function Icon({ children }: { children: ReactNode }) {
  return (
    <svg
      className="sidenav-icon"
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

function JobsIcon() {
  return (
    <Icon>
      <path d="M5.5 4 12 8l-6.5 4z" />
    </Icon>
  );
}

function SetupIcon() {
  return (
    <Icon>
      <path d="m2.5 4.5 1.5 1.5 2.5-2.5M2.5 10.5 4 12l2.5-2.5M9 5h4.5M9 11h4.5" />
    </Icon>
  );
}

function RunnersIcon() {
  return (
    <Icon>
      <rect x="2" y="2.5" width="12" height="4.5" rx="1.2" />
      <rect x="2" y="9" width="12" height="4.5" rx="1.2" />
      <path d="M4.75 4.75h.01M4.75 11.25h.01" />
    </Icon>
  );
}

function SettingsIcon() {
  return (
    <Icon>
      <path d="M2.5 4.5h6M11.5 4.5h2M2.5 11.5h2M7.5 11.5h6" />
      <circle cx="10" cy="4.5" r="1.5" />
      <circle cx="6" cy="11.5" r="1.5" />
    </Icon>
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

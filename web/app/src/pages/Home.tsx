// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState, type ReactNode } from "react";

import { ApiError, createTenant, listInstallations, listTenants, type InstallationChoice } from "../api";
import { Logo } from "../components/Logo";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { Link, navigate, useQuery } from "../router";
import { GitHubMark } from "./SignIn";

const settingsUrl = (login: string, id: number) =>
  `https://github.com/organizations/${login}/settings/installations/${id}`;

/** What the setup URL sends back in `?error=`, for the account in `?account=`. */
function errorText(code: string, account: string): ReactNode {
  const who = account || "that account";
  switch (code) {
    case "install-state":
      return "That install was started in another browser or session, so we didn't connect it. If it's yours, pick it from the list below.";
    case "install-unseen":
      return "We couldn't find that installation on your GitHub account. Try installing again.";
    case "install-personal":
      return (
        <>
          {who} is a personal account, and hatchrun works with organizations. Install it on an
          organization instead. You can remove it from {who} in your{" "}
          <a href="https://github.com/settings/installations">GitHub settings</a>.
        </>
      );
    case "install-suspended":
      return `hatchrun is suspended on ${who}. An owner can unsuspend it in the organization's GitHub settings, then come back here.`;
    case "install-not-owner":
      return `Only owners of ${who} can set it up. Ask an owner to sign in here and finish the setup.`;
    case "install-owner-unknown":
      return (
        <>
          GitHub won't tell hatchrun who owns {who} yet. hatchrun asks for one more permission
          (reading members) to check that. An owner can accept it in the{" "}
          {account ? (
            <a href={`https://github.com/organizations/${account}/settings/installations`}>
              organization's settings
            </a>
          ) : (
            "organization's settings"
          )}
          . Then pick {who} from the list below.
        </>
      );
    case "install-failed":
      return "GitHub didn't answer the way we expected while connecting your install. Pick your organization from the list below to try again.";
    default:
      return "Something went wrong. Try again.";
  }
}

/** One organization on the start page: an installation, a tenant, or both. */
interface Row {
  login: string;
  avatarUrl: string;
  installation: InstallationChoice | null;
  /** Set when the user is already in the organization's tenant. */
  tenant: string | null;
}

async function loadRows() {
  const [found, tenants] = await Promise.all([listInstallations(), listTenants()]);
  const rows: Row[] = found.installations.map((installation) => ({
    login: installation.login,
    avatarUrl: installation.avatarUrl,
    installation,
    tenant: installation.tenant,
  }));
  // Tenants whose App was uninstalled still open, to install it again.
  for (const tenant of tenants) {
    if (!rows.some((row) => row.login.toLowerCase() === tenant.login.toLowerCase())) {
      rows.push({ login: tenant.login, avatarUrl: tenant.avatarUrl, installation: null, tenant: tenant.login });
    }
  }
  rows.sort((a, b) => a.login.localeCompare(b.login));
  return { installUrl: found.installUrl, rows };
}

/**
 * The start page. Installing the App comes first: GitHub's own page
 * picks the organization and sends the user back to finish setup.
 * `stay` keeps a user with one organization here instead of opening it.
 */
export function Home({ stay = false }: { stay?: boolean }) {
  const [loaded, reload] = useLoad(loadRows, "start");
  const error = useQuery("error");
  const account = useQuery("account") ?? "";
  const requested = useQuery("install") === "requested";

  useEffect(() => {
    if (stay || error || requested || loaded.state !== "ready") {
      return;
    }
    const [only, ...rest] = loaded.data.rows;
    if (only?.tenant && rest.length === 0) {
      navigate(`/o/${only.tenant}`, true);
    }
  }, [loaded, stay, error, requested]);

  return (
    <section className="narrow start">
      {requested && (
        <p className="notice notice-accent" role="status">
          Request sent. An owner of the organization has to approve it on GitHub. Once they do,
          the organization shows up here.
        </p>
      )}
      {error && (
        <p className="notice notice-error" role="alert">
          {errorText(error, account)}
        </p>
      )}
      <Await loaded={loaded} retry={reload}>
        {({ installUrl, rows }) =>
          rows.length === 0 ? <Install installUrl={installUrl} /> : <Organizations installUrl={installUrl} rows={rows} />
        }
      </Await>
    </section>
  );
}

function Install({ installUrl }: { installUrl: string }) {
  return (
    <div className="start-hero">
      <div className="start-marks" aria-hidden="true">
        <span className="start-mark">
          <Logo size={28} />
        </span>
        <span className="start-link" />
        <span className="start-mark">
          <GitHubMark />
        </span>
      </div>
      <h1>Add hatchrun to your GitHub organization</h1>
      <p className="muted">
        Start by installing the hatchrun app. GitHub asks which organization, then brings you back
        here to pick a plan and connect AWS.
      </p>
      <ul className="card start-points">
        <li>
          <CheckIcon />
          <span>Jobs that ask for a hatchrun runner get a fresh EC2 instance in your AWS account.</span>
        </li>
        <li>
          <CheckIcon />
          <span>
            hatchrun registers runners and watches job events. It <strong>can't read your code</strong>.
          </span>
        </li>
        <li>
          <CheckIcon />
          <span>Choose all repositories or just a few. You can change that later on GitHub.</span>
        </li>
      </ul>
      <a className="button button-primary button-wide" href={installUrl}>
        <GitHubMark />
        Install on GitHub
      </a>
      <p className="fine">
        You need to be an owner of the organization. If you're a member, GitHub lets you send the
        request to an owner instead.
      </p>
    </div>
  );
}

function Organizations({ installUrl, rows }: { installUrl: string; rows: Row[] }) {
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const setUp = async (installation: InstallationChoice) => {
    setBusy(installation.id);
    setError(null);
    try {
      const tenant = await createTenant(installation.id);
      navigate(`/o/${tenant.org.login}`);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String(err));
      setBusy(null);
    }
  };

  return (
    <>
      <div className="page-head">
        <div>
          <h1>Your organizations</h1>
          <p className="muted">Pick one to carry on, or add another.</p>
        </div>
      </div>
      {error && (
        <p className="notice notice-error" role="alert">
          {error}
        </p>
      )}
      <ul className="list card">
        {rows.map((row) => (
          <li key={row.login}>
            <OrganizationRow row={row} busy={busy} onSetUp={setUp} />
          </li>
        ))}
        <li>
          <a className="list-row start-add" href={installUrl}>
            <span className="start-plus" aria-hidden="true">
              +
            </span>
            <span className="list-title">Add another organization</span>
          </a>
        </li>
      </ul>
      <p className="fine">
        An organization shows up here once hatchrun is installed on it. Only its owners can finish
        the setup.
      </p>
    </>
  );
}

function OrganizationRow({
  row,
  busy,
  onSetUp,
}: {
  row: Row;
  busy: number | null;
  onSetUp: (installation: InstallationChoice) => void;
}) {
  const avatar = <img className="avatar avatar-square" src={row.avatarUrl} alt="" width={32} height={32} />;
  const installation = row.installation;
  if (installation?.suspended) {
    return (
      <div className="list-row">
        {avatar}
        <span className="list-title">
          {row.login}
          <span className="badge badge-danger">Suspended</span>
        </span>
        <a className="button button-small" href={settingsUrl(row.login, installation.id)}>
          Unsuspend on GitHub
        </a>
      </div>
    );
  }
  if (row.tenant) {
    return (
      <Link href={`/o/${row.tenant}`} className="list-row">
        {avatar}
        <span className="list-title">
          {row.login}
          {installation === null && <span className="badge">App removed</span>}
        </span>
        <span className="chevron" aria-hidden="true">
          ›
        </span>
      </Link>
    );
  }
  if (installation === null) {
    return null;
  }
  return (
    <div className="list-row">
      {avatar}
      <span className="list-title">{row.login}</span>
      <button
        type="button"
        className="button button-small button-primary"
        disabled={busy !== null}
        onClick={() => onSetUp(installation)}
      >
        {busy === installation.id ? "Setting up" : "Continue setup"}
      </button>
    </div>
  );
}

function CheckIcon() {
  return (
    <svg className="start-check" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
      <path d="M3 8.5 6.5 12 13 4.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

import {
  approveCiJob,
  rejectCiJob,
  type CiJob,
  type CiRun,
  type Session,
  type Tenant,
} from "@/api";
import { Truncate } from "@/components/Truncate";
import { orgUrl } from "@/lib/urls";
import { Link } from "@/router";
import { BTN, BTN_PRIMARY } from "./runUi";

/** The jobs of a run held for an organization admin's approval, each with Approve and Reject. */
export function ApprovalCard({
  run,
  jobs,
  session,
  role,
  reload,
}: {
  run: CiRun;
  jobs: CiJob[];
  session: Session | null;
  role: Tenant["role"] | null;
  reload: () => void;
}) {
  const held = jobs.filter((job) => job.held === "approval");
  if (held.length === 0) {
    return null;
  }
  const [owner = ""] = run.repository.split("/");
  return (
    <section
      aria-label="Approval required"
      className="flex flex-col gap-3 rounded-md border border-waiting/50 bg-waiting/10 p-4"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <span className="cap text-waiting">Approval required</span>
        <Link
          href={orgUrl(owner, "environments")}
          className="font-mono text-[13px] text-fg-2 underline decoration-control underline-offset-4 hover:text-fg"
        >
          Edit environment rules
        </Link>
      </div>
      {held.map((job) => (
        <Held
          key={job.id}
          job={job}
          branch={run.branch}
          session={session}
          role={role}
          reload={reload}
        />
      ))}
    </section>
  );
}

function Held({
  job,
  branch,
  session,
  role,
  reload,
}: {
  job: CiJob;
  branch: string | null;
  session: Session | null;
  role: Tenant["role"] | null;
  reload: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const decide = async (approve: boolean) => {
    if (!approve && !window.confirm(`Reject ${job.name}? The job fails.`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await (approve ? approveCiJob(job.id) : rejectCiJob(job.id));
      reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 flex-col gap-1 font-mono text-[13px]">
        <div className="flex flex-wrap items-center gap-x-5 gap-y-1">
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">environment</span>
            <Truncate value={job.environment ?? "none"} className="text-fg" />
          </span>
          {branch && (
            <span className="flex min-w-0 items-center gap-2">
              <span className="cap">branch</span>
              <Truncate value={branch} className="text-fg" />
            </span>
          )}
        </div>
        <span className="text-fg-2">
          Job <span className="text-fg">{job.name}</span> does not start until
          an org admin approves.
        </span>
        {error !== null && (
          <span role="alert" className="text-failed">
            {error}
          </span>
        )}
      </div>
      {session && role === "admin" ? (
        <div className="flex items-center gap-2">
          <button
            type="button"
            className={BTN_PRIMARY}
            disabled={busy}
            aria-label={`Approve ${job.name}`}
            onClick={() => void decide(true)}
          >
            Approve
          </button>
          <button
            type="button"
            className={BTN}
            disabled={busy}
            aria-label={`Reject ${job.name}`}
            onClick={() => void decide(false)}
          >
            Reject
          </button>
        </div>
      ) : (
        <span className="font-mono text-[13px] text-fg-3">
          {session ? "Only org admins can approve." : "Sign in to approve."}
        </span>
      )}
    </div>
  );
}

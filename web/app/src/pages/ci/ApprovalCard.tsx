// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronRight } from "lucide-react";
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
import { cn } from "@/lib/utils";
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
  const decided = jobs.filter((job) => job.held === null && job.approval !== null);
  // Decided cards start folded to one line; pending ones start open.
  const [pick, setPick] = useState<boolean | null>(null);
  const open = pick ?? held.length > 0;
  if (held.length === 0 && decided.length === 0) {
    return null;
  }
  const [owner = ""] = run.repository.split("/");
  const pending = held.length > 0;
  const shown = pending ? held : decided;
  const summary = pending
    ? `${held.length} ${held.length === 1 ? "job waits" : "jobs wait"} for approval`
    : decided.map((job) => `${job.name} ${job.approval?.decision} by ${job.approval?.by}`).join(" · ");
  return (
    <section
      aria-label={pending ? "Approval required" : "Approval decided"}
      className={cn(
        "flex flex-col gap-2 rounded-md border px-3 py-2",
        pending ? "border-rule border-l-2 border-l-waiting/60 bg-panel" : "border-rule bg-panel",
      )}
    >
      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1">
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setPick(!open)}
          className="flex min-w-0 cursor-pointer items-center gap-2 border-0 bg-transparent p-0 text-left"
        >
          <ChevronRight size={12} aria-hidden="true" className={cn("shrink-0 text-fg-3 transition-transform", open && "rotate-90")} />
          <span className={cn("cap shrink-0", pending && "text-waiting")}>{pending ? "Approval required" : "Approval decided"}</span>
          {!open && <span className="min-w-0 truncate font-mono text-[13px] text-fg-2">{summary}</span>}
        </button>
        <Link
          href={orgUrl(owner, "environments")}
          className="font-mono text-[13px] text-fg-2 underline decoration-control underline-offset-4 hover:text-fg"
        >
          Edit environment rules
        </Link>
      </div>
      {open &&
        shown.map((job) => (
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
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
      <div className="flex min-w-0 flex-wrap items-center gap-x-5 gap-y-1 font-mono text-[13px]">
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
          {job.approval ? (
            <>
              Job <span className="text-fg">{job.name}</span> was {job.approval.decision} by{" "}
              <span className="text-fg">{job.approval.by}</span> on {new Date(job.approval.at).toLocaleString()}.
            </>
          ) : (
            <>
              Job <span className="text-fg">{job.name}</span> does not start until an org admin approves.
            </>
          )}
        </span>
        {error !== null && (
          <span role="alert" className="text-failed">
            {error}
          </span>
        )}
      </div>
      {job.approval ? null : session && role === "admin" ? (
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

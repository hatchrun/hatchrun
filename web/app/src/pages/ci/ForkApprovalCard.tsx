// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

import { approveForkRun, rejectForkRun, type CiRun, type Session } from "@/api";
import { Truncate } from "@/components/Truncate";
import { cn } from "@/lib/utils";
import { BTN, BTN_PRIMARY } from "./runUi";

/** What GitHub's author association says about who wrote the pull request. */
const WHO: Record<string, string> = {
  FIRST_TIMER: "new to GitHub",
  FIRST_TIME_CONTRIBUTOR: "first contribution to this repository",
  CONTRIBUTOR: "has contributed before",
  NONE: "no association with the repository",
  MANNEQUIN: "an imported account",
};

/**
 * A fork pull request's run that waits for a maintainer: who wrote it,
 * where the head is and what the run does not get, with Approve and run
 * and Reject for anyone who can write to the repository (the server
 * decides). Once decided it folds to one line with who decided.
 */
export function ForkApprovalCard({ run, session, reload }: { run: CiRun; session: Session | null; reload: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { fork, gate } = run;
  if (fork === null || gate === null) {
    return null;
  }
  const pending = gate.status === "pending" || gate.status === "deferred";

  const decide = async (approve: boolean) => {
    if (!approve && !window.confirm("Reject this run? It is cancelled.")) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await (approve ? approveForkRun(run.id, gate.check_run_id) : rejectForkRun(run.id, gate.check_run_id));
      reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section
      aria-label={pending ? "Fork approval required" : "Fork approval decided"}
      className={cn("flex flex-col gap-2 rounded-md border px-3 py-2", pending ? "border-rule border-l-2 border-l-waiting/60 bg-panel" : "border-rule bg-panel")}
    >
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-5 gap-y-1 font-mono text-[13px]">
          <span className={cn("cap shrink-0", pending && "text-waiting")}>
            {gate.status === "pending" ? "Approval required" : gate.status === "deferred" ? "Waiting for a free slot" : `Approval ${gate.status}`}
          </span>
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">author</span>
            <Truncate value={fork.author} className="text-fg" />
            <span className="text-fg-3">{WHO[fork.author_association] ?? fork.author_association.toLowerCase()}</span>
          </span>
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">head</span>
            <Truncate value={fork.head_repo ?? "a deleted fork"} className="text-fg" />
          </span>
          {gate.status === "approved" && gate.by && <span className="text-fg-2">Approved by {gate.by}</span>}
          {error !== null && (
            <span role="alert" className="text-failed">
              {error}
            </span>
          )}
        </div>
        {pending &&
          (session ? (
            <div className="flex items-center gap-2">
              <button type="button" className={BTN_PRIMARY} disabled={busy} aria-label="Approve and run" onClick={() => void decide(true)}>
                Approve and run
              </button>
              <button type="button" className={BTN} disabled={busy} aria-label="Reject run" onClick={() => void decide(false)}>
                Reject
              </button>
            </div>
          ) : (
            <span className="font-mono text-[13px] text-fg-3">Sign in to approve.</span>
          ))}
      </div>
      {pending && (
        <p className="m-0 font-mono text-[13px] text-fg-3">
          The run gets no secrets, no write token, no OIDC token and no environment. Check the pull request for changes to the config before you approve: its own workflow is what runs.
        </p>
      )}
    </section>
  );
}

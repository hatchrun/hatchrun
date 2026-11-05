// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ChevronLeft, ExternalLink } from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import {
  ApiError,
  cancelCiRun,
  ciArtifactUrl,
  getCiRun,
  listCiArtifacts,
  type CiJob,
  type CiRun as Run,
  type CiRunDetail,
  type Session,
} from "@/api";
import { CopyButton } from "@/components/CopyButton";
import { Await, LoadError } from "@/components/Status";
import { ciStatus, StatusPill } from "@/components/StatusPill";
import { Truncate } from "@/components/Truncate";
import { useLoad, useNow } from "@/load";
import { bytes, duration, shortRef, shortSha } from "@/lib/format";
import { orgUrl, repoRunsUrl, runUrl } from "@/lib/urls";
import { Link, useLocation, type CiRoute } from "@/router";
import { ApprovalCard } from "./ci/ApprovalCard";
import { JobLog } from "./ci/JobLog";
import { JobsPanel } from "./ci/JobsPanel";
import { BTN } from "./ci/runUi";
import { SignIn } from "./SignIn";
import type { RunPageProps } from "./types";

/** How often a run with unfinished work reads again, in milliseconds. */
const REFRESH_MS = 5000;

/** The viewer the API turned away: signed out and not allowed to see this. */
const turnedAway = (error: Error, session: Session | null) =>
  session === null && error instanceof ApiError && (error.status === 401 || error.status === 404);

/** Reads again every few seconds while `active`. */
function useRefresh(active: boolean, reload: () => void) {
  useEffect(() => {
    if (!active) {
      return;
    }
    const timer = window.setInterval(reload, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [active, reload]);
}

/**
 * The run page (`/o/:org/ci/:repo/runs/:n`) and the job page
 * (`.../jobs/:job`): the approval card, the jobs of the run on the
 * left, one job's log on the right.
 */
export function CiRun({ session, route }: RunPageProps) {
  const location = useLocation();
  const [detail, reload] = useLoad(() => getCiRun(route.run), route.run, true);
  const quiet = useCallback(() => reload(), [reload]);
  useRefresh(detail.state === "ready" && detail.data.jobs.some((job) => job.status !== "completed"), quiet);

  if (detail.state === "error" && turnedAway(detail.error, session)) {
    return <SignIn returnTo={location} />;
  }
  if (detail.state === "error" && detail.error instanceof ApiError && detail.error.status === 404) {
    return <LoadError error={new Error("Not found, or you cannot read this repository.")} />;
  }
  return (
    <div className="mx-auto flex w-full max-w-[1400px] flex-col gap-4">
      <Await loaded={detail} retry={reload}>
        {(data) => <RunScreen route={route} data={data} stale={detail.state === "ready" ? detail.stale : undefined} session={session} reload={reload} />}
      </Await>
    </div>
  );
}

/** The job the page shows: the one in the URL, else the first failure, then a running one, then the first. */
function chosen(jobs: CiJob[], id: string | null): CiJob | undefined {
  if (id !== null) {
    return jobs.find((job) => String(job.id) === id);
  }
  const by = (status: ReturnType<typeof ciStatus>) => jobs.find((job) => ciStatus(job.status, job.conclusion, job.held) === status);
  return by("failed") ?? by("running") ?? jobs.find((job) => job.has_log) ?? jobs[0];
}

function RunScreen({
  route,
  data,
  stale,
  session,
  reload,
}: {
  route: CiRoute;
  data: CiRunDetail;
  stale: Error | undefined;
  session: Session | null;
  reload: () => void;
}) {
  const { run, jobs } = data;
  const now = useNow(1000);
  const job = chosen(jobs, route.job);
  return (
    <>
      <Crumbs route={route} run={run} job={job} />
      <Header run={run} session={session} now={now} reload={reload} />
      {stale && (
        <p className="font-mono text-[13px] text-fg-3" role="status">
          Could not refresh: {stale.message}. Showing the last result.
        </p>
      )}
      <ApprovalCard run={run} jobs={jobs} session={session} reload={reload} />
      <div className="flex h-[calc(100dvh-9rem)] min-h-[520px] gap-4 max-lg:h-auto max-lg:flex-col">
        <JobsPanel run={run} jobs={jobs} selected={job?.id ?? null} now={now} />
        <div className="flex min-h-0 min-w-0 flex-1 flex-col max-lg:h-[70vh]">
          {job === undefined ? (
            <p className="muted">This run has no such job.</p>
          ) : job.has_log || job.status !== "queued" ? (
            <JobLog key={job.id} job={job} onCheck={reload} />
          ) : (
            <p className="font-mono text-[13px] text-fg-3">{job.held ? "This job waits for approval." : "This job has not started."}</p>
          )}
        </div>
      </div>
      <Artifacts run={run} />
    </>
  );
}

function Crumbs({ route, run, job }: { route: CiRoute; run: Run; job: CiJob | undefined }) {
  const sep = (
    <span aria-hidden="true" className="text-control">
      /
    </span>
  );
  return (
    <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-2 font-mono text-[13px] text-fg-3">
      <Link href={orgUrl(route.org)} className="inline-flex items-center gap-1 text-fg hover:text-brand">
        <ChevronLeft size={12} aria-hidden="true" className="text-brand" />
        Runs
      </Link>
      {sep}
      <Link href={repoRunsUrl(run.repository)} title={run.repository} className="min-w-0 truncate underline decoration-control underline-offset-4 hover:text-fg">
        {run.repository}
      </Link>
      {sep}
      <Link href={runUrl(run.repository, run.id)} className="underline decoration-control underline-offset-4 hover:text-fg">
        #{run.run_number}
      </Link>
      {job && (
        <>
          {sep}
          <span aria-current="page" title={job.name} className="min-w-0 truncate text-fg">
            {job.name}
          </span>
        </>
      )}
    </nav>
  );
}

function Header({ run, session, now, reload }: { run: Run; session: Session | null; now: number; reload: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const finished = run.status === "completed";
  const took = duration((run.completed_at ?? now) - run.created_at);
  const branch = run.branch ?? shortRef(run.ref);

  const cancel = async () => {
    if (!window.confirm(`Cancel run #${run.run_number}? Jobs that are running stop.`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await cancelCiRun(run.id);
      reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-wrap items-start justify-between gap-4">
      <div className="flex min-w-0 flex-col gap-2">
        <StatusPill status={ciStatus(run.status, run.conclusion)} className="self-start">
          <span className="font-normal text-fg-2">{finished ? `took ${took}` : `for ${took}`}</span>
        </StatusPill>
        <h1 className="page-title break-words" title={run.workflow}>
          {run.workflow} <span className="font-mono text-xl font-medium tracking-normal text-fg-3 normal-case [font-stretch:100%]">#{run.run_number}</span>
        </h1>
        <div className="flex flex-wrap items-center gap-x-5 gap-y-1 font-mono text-[13px]">
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">branch</span>
            <Truncate value={branch} copy className="max-w-[260px] text-fg" />
          </span>
          <span className="flex items-center gap-2">
            <span className="cap">sha</span>
            <a
              href={`https://github.com/${run.repository}/commit/${run.sha}`}
              aria-label={`Commit ${shortSha(run.sha)} on GitHub, opens github.com`}
              className="inline-flex items-center gap-1.5 text-fg underline decoration-control underline-offset-4 hover:text-brand"
            >
              {shortSha(run.sha)}
              <ExternalLink size={10} aria-hidden="true" className="text-fg-3" />
            </a>
          </span>
          <span className="flex items-center gap-2">
            <span className="cap">actor</span>
            <span className="text-fg">{run.actor}</span>
          </span>
          <span className="flex items-center gap-2">
            <span className="cap">event</span>
            <span className="text-fg">{run.event}</span>
          </span>
          {run.run_attempt > 1 && (
            <span className="flex items-center gap-2">
              <span className="cap">attempt</span>
              <span className="text-fg">{run.run_attempt}</span>
            </span>
          )}
        </div>
        {error !== null && (
          <p role="alert" className="font-mono text-[13px] text-failed">
            {error}
          </p>
        )}
      </div>
      <div className="flex items-center gap-2">
        {session && !finished && (
          <button type="button" className={BTN} disabled={busy} onClick={() => void cancel()}>
            Cancel run
          </button>
        )}
        <span className="inline-flex items-center gap-1 font-mono text-[13px] text-fg-3">
          <CopyButton text={window.location.href.split("#")[0] ?? ""} label="Copy the link to this run" />
          Copy link
        </span>
      </div>
    </div>
  );
}

/** The artifacts the run's jobs uploaded, each a download. Nothing shows while there are none. */
function Artifacts({ run }: { run: Run }) {
  const [loaded, reload] = useLoad(() => listCiArtifacts(run.id), `${run.id}:${run.status}`, true);
  // Jobs upload while the run goes; read again until it ends.
  useRefresh(run.status !== "completed", reload);
  if (loaded.state === "loading") {
    return null;
  }
  if (loaded.state !== "ready" || loaded.data.artifacts.length === 0) {
    return null;
  }
  return (
    <section aria-label="Artifacts" className="flex flex-col gap-2">
      <h2 className="text-base font-semibold">Artifacts</h2>
      <ul className="divide-y divide-rule overflow-hidden rounded-md border border-rule bg-panel">
        {loaded.data.artifacts.map((artifact) => (
          <li key={artifact.id} className="flex items-center justify-between gap-3 px-3 py-2 font-mono text-[13px]">
            <a href={ciArtifactUrl(run.id, artifact.id)} className="min-w-0 truncate font-bold text-fg hover:text-brand">
              {artifact.name}
            </a>
            <span className="shrink-0 text-fg-3">
              {bytes(artifact.size)} · expires {new Date(artifact.expires_at).toLocaleDateString()}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

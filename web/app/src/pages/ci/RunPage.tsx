// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The orchestrator's run pages: one run with its jobs, and one job with
// its log. The screen agents replace these (see pages/CiRun.tsx). Public repositories read without signing
// in; a viewer the API turns away (401, or 404 while signed out) gets
// the sign-in prompt.

import { Fragment, useCallback, useEffect, useState, type ReactNode } from "react";

import {
  ApiError,
  approveCiJob,
  ciArtifactUrl,
  getCiRun,
  listCiArtifacts,
  rejectCiJob,
  type CiJob,
  type CiRun,
  type CiRunDetail,
  type Session,
} from "@/api";
import { ChevronLeftIcon } from "@/components/icons";
import { ciStatus, StatusPill } from "@/components/StatusPill";
import { Await, LoadError, Spinner } from "@/components/Status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useLoad, useNow } from "@/load";
import { Link, useLocation, type CiRoute } from "@/router";
import { bytes, span } from "@/lib/format";
import { jobUrl, repoRunsUrl, runUrl } from "@/lib/urls";
import { SignIn } from "../SignIn";
import { JobLog } from "./JobLog";

/** How often a page with unfinished work reads again, in milliseconds. */
const REFRESH_MS = 5000;

/** The viewer the API turned away: signed out and not allowed to see this. */
const turnedAway = (error: Error, session: Session | null) =>
  session === null && error instanceof ApiError && (error.status === 401 || error.status === 404);

const shortRef = (ref: string) => ref.replace(/^refs\/(heads|tags)\//, "");

function Frame({ children }: { session: Session | null; route: CiRoute; children: ReactNode }) {
  return <div className="flex w-full flex-col gap-4">{children}</div>;
}

/** What a failed read shows: the sign-in prompt for a viewer turned away, else the error. */
function Denied({ error, session, retry }: { error: Error; session: Session | null; retry: () => void }) {
  const location = useLocation();
  if (turnedAway(error, session)) {
    return <SignIn returnTo={location} />;
  }
  if (error instanceof ApiError && error.status === 404) {
    return <LoadError error={new Error("Not found, or you cannot read this repository.")} />;
  }
  return <LoadError error={error} retry={retry} />;
}

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

export function CiPages({ session, route }: { session: Session | null; route: CiRoute }) {
  if (route.job !== null) {
    return <JobView session={session} route={route} run={route.run} job={route.job} />;
  }
  return <RunView session={session} route={route} run={route.run} />;
}

const took = (run: Pick<CiRun, "status" | "created_at" | "completed_at">, now: number) =>
  span((run.completed_at ?? now) - run.created_at);

function Crumbs({ route, children }: { route: CiRoute; children: ReactNode }) {
  return (
    <nav aria-label="Breadcrumb" className="flex items-center gap-2 font-mono text-xs text-fg-3">
      <Link href={repoRunsUrl(route)} className="inline-flex items-center gap-1.5 text-fg hover:text-brand">
        <ChevronLeftIcon className="text-brand" />
        Runs
      </Link>
      {children}
    </nav>
  );
}

/** The run and its jobs, read again while any of them is unfinished. */
function useRun(id: string): [ReturnType<typeof useLoad<CiRunDetail>>[0], () => void] {
  const [detail, reload] = useLoad(() => getCiRun(id), id, true);
  const quiet = useCallback(() => reload(), [reload]);
  useRefresh(detail.state === "ready" && detail.data.jobs.some((job) => job.status !== "completed"), quiet);
  return [detail, reload];
}

function RunView({ session, route, run }: { session: Session | null; route: CiRoute; run: string }) {
  const [detail, reload] = useRun(run);
  if (detail.state === "error" && turnedAway(detail.error, session)) {
    return <SignIn returnTo={runUrl(route, run)} />;
  }
  return (
    <Frame session={session} route={route}>
      <Crumbs route={route}>
        <span className="text-control">/</span>
        <span>Run {run}</span>
      </Crumbs>
      {detail.state === "error" ? (
        <Denied error={detail.error} session={session} retry={reload} />
      ) : (
        <Await loaded={detail} retry={reload}>
          {(data) => <RunDetailView route={route} data={data} session={session} reload={reload} />}
        </Await>
      )}
    </Frame>
  );
}

function RunDetailView({
  route,
  data,
  session,
  reload,
}: {
  route: CiRoute;
  data: CiRunDetail;
  session: Session | null;
  reload: () => void;
}) {
  const { run, jobs } = data;
  const now = useNow(1000);
  return (
    <>
      <div className="flex flex-col gap-2">
        <StatusPill status={ciStatus(run.status, run.conclusion)}>
          <span className="font-normal text-fg-3">{run.completed_at ? `took ${took(run, now)}` : `for ${took(run, now)}`}</span>
        </StatusPill>
        <h1 className="page-title break-words">
          {run.workflow}{" "}
          <span className="font-mono text-xl font-medium tracking-normal text-fg-3 normal-case [font-stretch:100%]">
            #{run.run_number}
          </span>
        </h1>
        <span className="font-mono text-xs text-fg-3">
          {run.repository} · {shortRef(run.ref)} · {run.sha.slice(0, 7)} · {run.event} by {run.actor}
          {run.run_attempt > 1 && <> · attempt {run.run_attempt}</>}
        </span>
      </div>
      <section aria-label="Jobs" className="overflow-hidden rounded-md border border-control">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="cap">Status</TableHead>
              <TableHead className="cap">Job</TableHead>
              <TableHead className="cap text-right">Started</TableHead>
              <TableHead className="cap text-right">Took</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {jobs.map((job) => (
              <Fragment key={job.id}>
                <TableRow>
                  <TableCell>
                    <StatusPill status={ciStatus(job.status, job.conclusion)} />
                  </TableCell>
                  <TableCell>
                    <Link
                      href={jobUrl(route, run.id, job.id)}
                      className="font-semibold text-fg hover:text-brand"
                    >
                      {job.name}
                    </Link>
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {job.started_at === null ? "not yet" : new Date(job.started_at).toLocaleTimeString()}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">{jobTook(job, now)}</TableCell>
                </TableRow>
                {(job.held === "approval" || job.approval?.decision === "approved") && (
                  <TableRow>
                    <TableCell colSpan={4}>
                      <Approval job={job} session={session} reload={reload} />
                    </TableCell>
                  </TableRow>
                )}
              </Fragment>
            ))}
          </TableBody>
        </Table>
      </section>
      <Artifacts run={run} />
    </>
  );
}

/** The artifacts the run's jobs uploaded, each a download. Nothing shows while there are none. */
function Artifacts({ run }: { run: CiRun }) {
  const [loaded, reload] = useLoad(() => listCiArtifacts(run.id), `${run.id}:${run.status}`, true);
  // Jobs upload while the run goes; read again until it ends.
  useRefresh(run.status !== "completed", reload);
  if (loaded.state !== "ready" || loaded.data.artifacts.length === 0) {
    return null;
  }
  return (
    <section aria-label="Artifacts" className="flex flex-col gap-2">
      <h2 className="cap">Artifacts</h2>
      <div className="overflow-hidden rounded-md border border-control">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="cap">Name</TableHead>
              <TableHead className="cap text-right">Size</TableHead>
              <TableHead className="cap text-right">Expires</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {loaded.data.artifacts.map((artifact) => (
              <TableRow key={artifact.id}>
                <TableCell>
                  <a href={ciArtifactUrl(run.id, artifact.id)} className="font-semibold text-fg hover:text-brand">
                    {artifact.name}
                  </a>
                </TableCell>
                <TableCell className="text-right font-mono text-xs">{bytes(artifact.size)}</TableCell>
                <TableCell className="text-right font-mono text-xs">
                  {new Date(artifact.expires_at).toLocaleDateString()}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
    </section>
  );
}

/** A job held for approval, with Approve and Reject for a signed-in viewer; or who approved it. */
function Approval({ job, session, reload }: { job: CiJob; session: Session | null; reload: () => void }) {
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

  if (job.held !== "approval") {
    return <p className="text-sm text-fg-3">Approved by {job.approval?.by}</p>;
  }
  return (
    <div className="flex flex-wrap items-center gap-3 text-sm">
      <span>Waiting for approval to use {job.environment}</span>
      {session && (
        <>
          <button type="button" className="button button-primary" disabled={busy} onClick={() => void decide(true)}>
            Approve
          </button>
          <button type="button" className="button" disabled={busy} onClick={() => void decide(false)}>
            Reject
          </button>
        </>
      )}
      {error !== null && (
        <span role="alert" className="text-[var(--app-danger)]">
          {error}
        </span>
      )}
    </div>
  );
}

const jobTook = (job: CiJob, now: number) =>
  job.started_at === null ? "" : span((job.completed_at ?? now) - job.started_at);

function JobView({
  session,
  route,
  run,
  job,
}: {
  session: Session | null;
  route: CiRoute;
  run: string;
  job: string;
}) {
  const [detail, reload] = useRun(run);
  const found = detail.state === "ready" ? detail.data.jobs.find((candidate) => String(candidate.id) === job) : undefined;
  if (detail.state === "error" && turnedAway(detail.error, session)) {
    return <SignIn returnTo={jobUrl(route, run, job)} />;
  }
  return (
    <Frame session={session} route={route}>
      <Crumbs route={route}>
        <span className="text-control">/</span>
        <Link href={runUrl(route, run)} className="text-fg hover:text-brand">
          Run {run}
        </Link>
        <span className="text-control">/</span>
        <span>{found?.name ?? `Job ${job}`}</span>
      </Crumbs>
      {detail.state === "loading" && <Spinner />}
      {detail.state === "error" && <Denied error={detail.error} session={session} retry={reload} />}
      {detail.state === "ready" && found === undefined && <p className="muted">This run has no such job.</p>}
      {detail.state === "ready" && detail.stale && (
        <p className="font-mono text-xs text-fg-3" role="status">
          Could not refresh: {detail.stale.message}. Showing the last result.
        </p>
      )}
      {found && <JobHeader job={found} />}
      {found && <JobLog key={found.id} job={found} onCheck={reload} />}
    </Frame>
  );
}

function JobHeader({ job }: { job: CiJob }) {
  const now = useNow(1000);
  return (
    <div className="flex flex-col gap-2">
      <StatusPill status={ciStatus(job.status, job.conclusion)}>
        {job.started_at !== null && <span className="font-normal text-fg-3">{jobTook(job, now)}</span>}
      </StatusPill>
      <h1 className="page-title break-words">{job.name}</h1>
    </div>
  );
}

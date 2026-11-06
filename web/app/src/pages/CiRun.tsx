// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { ExternalLink } from "lucide-react";
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
import { orgUrl, repoUrl, runUrl, workflowUrl } from "@/lib/urls";
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
  session === null &&
  error instanceof ApiError &&
  (error.status === 401 || error.status === 404);

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
export function CiRun({ session, role, route }: RunPageProps) {
  const location = useLocation();
  const [detail, reload] = useLoad(() => getCiRun(route.run), route.run, true);
  const quiet = useCallback(() => reload(), [reload]);
  useRefresh(
    detail.state === "ready" &&
      (detail.data.run.status !== "completed" ||
        detail.data.jobs.some((job) => job.status !== "completed")),
    quiet,
  );

  if (detail.state === "error" && turnedAway(detail.error, session)) {
    return <SignIn returnTo={location} />;
  }
  if (
    detail.state === "error" &&
    detail.error instanceof ApiError &&
    detail.error.status === 404
  ) {
    return (
      <LoadError
        error={new Error("Not found, or you cannot read this repository.")}
      />
    );
  }
  return (
    <div className="mx-auto flex w-full max-w-[1400px] flex-col gap-3">
      <Await loaded={detail} retry={reload}>
        {(data) =>
          // The URL names the repository; a run of another one is not shown under it.
          data.run.repository.toLowerCase() !== `${route.org}/${route.repo}`.toLowerCase() ? (
            <LoadError error={new Error("Not found, or you cannot read this repository.")} />
          ) : (
            <RunScreen
              route={route}
              data={data}
              stale={detail.state === "ready" ? detail.stale : undefined}
              session={session}
              role={role}
              reload={reload}
            />
          )
        }
      </Await>
    </div>
  );
}

/** The job the page shows: the one in the URL, else the first failure, then a running one, then the first. */
function chosen(jobs: CiJob[], id: string | null): CiJob | undefined {
  if (id !== null) {
    return jobs.find((job) => String(job.id) === id);
  }
  const by = (status: ReturnType<typeof ciStatus>) =>
    jobs.find(
      (job) => ciStatus(job.status, job.conclusion, job.held) === status,
    );
  return (
    by("failed") ?? by("running") ?? jobs.find((job) => job.has_log) ?? jobs[0]
  );
}

function RunScreen({
  route,
  data,
  stale,
  session,
  role,
  reload,
}: {
  route: CiRoute;
  data: CiRunDetail;
  stale: Error | undefined;
  session: Session | null;
  role: RunPageProps["role"];
  reload: () => void;
}) {
  const { run, jobs } = data;
  const now = useNow(1000);
  const job = chosen(jobs, route.job);
  return (
    <>
      <Crumbs route={route} run={run} job={job} />
      <Header run={run} jobs={jobs} access={data.access} now={now} reload={reload} />
      {stale && (
        <p className="font-mono text-[13px] text-fg-3" role="status">
          Could not refresh: {stale.message}. Showing the last result.
        </p>
      )}
      <ApprovalCard
        run={run}
        jobs={jobs}
        session={session}
        role={role}
        reload={reload}
      />
      <div className="flex h-[calc(100dvh-9.5rem)] min-h-[520px] gap-4 max-lg:h-auto max-lg:flex-col">
        <JobsPanel
          key={run.id}
          run={run}
          jobs={jobs}
          selected={job?.id ?? null}
          now={now}
        />
        <div className="flex min-h-0 min-w-0 flex-1 flex-col max-lg:h-[75vh] max-lg:flex-none">
          {job === undefined ? (
            <p className="muted">This run has no such job.</p>
          ) : job.has_log || job.status !== "queued" ? (
            <JobLog key={job.id} job={job} onCheck={reload} />
          ) : (
            <p className="font-mono text-[13px] text-fg-3">
              {job.held
                ? "This job waits for approval."
                : "This job has not started."}
            </p>
          )}
        </div>
      </div>
      <Artifacts run={run} />
    </>
  );
}

function Crumbs({
  route,
  run,
  job,
}: {
  route: CiRoute;
  run: Run;
  job: CiJob | undefined;
}) {
  const sep = (
    <span aria-hidden="true" className="text-control">
      /
    </span>
  );
  return (
    <nav
      aria-label="Breadcrumb"
      className="flex min-w-0 items-center gap-2 font-mono text-[13px] text-fg-3"
    >
      <Link href={orgUrl(route.org)} className="link">
        {route.org}
      </Link>
      {sep}
      <Link
        href={repoUrl(run.repository)}
        title={run.repository}
        className="min-w-0 truncate underline decoration-control underline-offset-4 hover:text-fg"
      >
        {route.repo}
      </Link>
      {sep}
      <Link
        href={workflowUrl(run.repository, run.workflow)}
        title={run.workflow}
        className="min-w-0 truncate underline decoration-control underline-offset-4 hover:text-fg"
      >
        {run.workflow}
      </Link>
      {sep}
      <Link
        href={runUrl(run.repository, run.id)}
        className="underline decoration-control underline-offset-4 hover:text-fg"
      >
        #{run.run_number}
      </Link>
      {job && (
        <>
          {sep}
          <span
            aria-current="page"
            title={job.name}
            className="min-w-0 truncate text-fg"
          >
            {job.name}
          </span>
        </>
      )}
    </nav>
  );
}

function Header({
  run,
  jobs,
  access,
  now,
  reload,
}: {
  run: Run;
  jobs: CiJob[];
  access: CiRunDetail["access"];
  now: number;
  reload: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const finished = run.status === "completed";
  const took = duration((run.completed_at ?? now) - run.created_at);
  const branch = run.branch ?? shortRef(run.ref);
  // Held for approval and nothing running: the run waits on a person, not the queue.
  const awaiting = !finished && jobs.some((job) => job.held === "approval") && !jobs.some((job) => job.status === "in_progress");
  const status = awaiting ? "waiting" : ciStatus(run.status, run.conclusion);

  const cancel = async () => {
    if (
      !window.confirm(
        `Cancel run #${run.run_number}? Jobs that are running stop.`,
      )
    ) {
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
    <div className="flex flex-col gap-1.5">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          <h1 className="page-title min-w-0 break-words" title={run.workflow}>
            {run.workflow}{" "}
            <span className="font-mono text-xl font-medium tracking-normal text-fg-3 normal-case [font-stretch:100%]">
              #{run.run_number}
            </span>
          </h1>
          <StatusPill status={status} label={awaiting ? "Waiting for approval" : undefined}>
            <span className="font-normal text-fg-2">
              {awaiting ? `for ${took}` : finished ? `took ${took}` : `for ${took}`}
            </span>
          </StatusPill>
        </div>
        <div className="flex items-center gap-2">
          {access !== "read" && !finished && (
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
            className="inline-flex items-center gap-1.5 link"
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
        {run.pull_request && (
          <span className="flex min-w-0 items-center gap-2">
            <span className="cap">pull request</span>
            <Link href={repoUrl(run.repository, "pulls")} title={run.pull_request.title} className="max-w-[320px] truncate link">
              #{run.pull_request.number} {run.pull_request.title}
            </Link>
          </span>
        )}
        {run.run_attempt > 1 && (
          <span className="flex items-center gap-2">
            <span className="cap">attempt</span>
            <span className="text-fg">{run.run_attempt}</span>
          </span>
        )}
        {jobs
          .filter((job) => job.environment_url && /^https?:\/\//.test(job.environment_url))
          .map((job) => (
            <span key={job.id} className="flex min-w-0 items-center gap-2">
              <span className="cap">deployed</span>
              <a
                href={job.environment_url ?? undefined}
                aria-label={`${job.environment ?? job.name} deployment, opens ${job.environment_url ?? ""}`}
                className="inline-flex max-w-[260px] items-center gap-1.5 link"
              >
                <span className="truncate">{job.environment ?? job.name}</span>
                <ExternalLink size={10} aria-hidden="true" className="text-fg-3" />
              </a>
            </span>
          ))}
      </div>
      {error !== null && (
        <p role="alert" className="font-mono text-[13px] text-failed">
          {error}
        </p>
      )}
    </div>
  );
}

/** The artifacts the run's jobs uploaded, each a download. Nothing shows while there are none. */
function Artifacts({ run }: { run: Run }) {
  const [loaded, reload] = useLoad(
    () => listCiArtifacts(run.id),
    `${run.id}:${run.status}`,
    true,
  );
  // Jobs upload while the run goes; read again until it ends.
  useRefresh(run.status !== "completed", reload);
  if (loaded.state === "error") {
    return (
      <section aria-label="Artifacts" className="flex flex-col gap-2">
        <h2 className="text-base font-semibold">Artifacts</h2>
        <p
          role="alert"
          className="flex flex-wrap items-center gap-3 font-mono text-[13px] text-failed"
        >
          Could not load artifacts: {loaded.error.message}
          <button type="button" className={BTN} onClick={reload}>
            Retry
          </button>
        </p>
      </section>
    );
  }
  if (loaded.state !== "ready" || loaded.data.artifacts.length === 0) {
    return null;
  }
  return (
    <section aria-label="Artifacts" className="flex flex-col gap-2">
      <h2 className="text-base font-semibold">Artifacts</h2>
      <ul className="divide-y divide-rule overflow-hidden rounded-md border border-rule bg-panel">
        {loaded.data.artifacts.map((artifact) => (
          <li
            key={artifact.id}
            className="flex items-center justify-between gap-3 px-3 py-2 font-mono text-[13px]"
          >
            <a
              href={ciArtifactUrl(run.id, artifact.id)}
              className="min-w-0 truncate font-bold link"
            >
              {artifact.name}
            </a>
            <span className="shrink-0 text-fg-3">
              {bytes(artifact.size)} · expires{" "}
              {new Date(artifact.expires_at).toLocaleDateString()}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

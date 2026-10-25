// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The orchestrator's pages: a repository's runs, one run with its jobs,
// and one job with its log. Public repositories read without signing
// in; a viewer the API turns away (401, or 404 while signed out) gets
// the sign-in prompt.

import { Fragment, useCallback, useEffect, useState, type ReactNode } from "react";

import {
  ApiError,
  approveCiJob,
  getCiRun,
  listCiRuns,
  rejectCiJob,
  type CiJob,
  type CiRun,
  type CiRunDetail,
  type Session,
} from "@/api";
import { ChevronLeftIcon } from "@/components/icons";
import { span, state, StatusMark } from "@/components/JobStatus";
import { Shell } from "@/components/Shell";
import { Await, LoadError, Spinner } from "@/components/Status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useLoad, useNow } from "@/load";
import { Link, navigate, useLocation, type CiRoute } from "@/router";
import { SignIn } from "../SignIn";
import { DispatchDialog } from "./DispatchDialog";
import { JobLog } from "./JobLog";
import { RepoSettings } from "./RepoSettings";

/** How often a page with unfinished work reads again, in milliseconds. */
const REFRESH_MS = 5000;

const shown = (status: string, conclusion: string | null) =>
  state({
    status: status === "completed" || status === "in_progress" ? status : "queued",
    conclusion,
    launchState: null,
    launchRetry: null,
  });

/** The viewer the API turned away: signed out and not allowed to see this. */
const turnedAway = (error: Error, session: Session | null) =>
  session === null && error instanceof ApiError && (error.status === 401 || error.status === 404);

const shortRef = (ref: string) => ref.replace(/^refs\/(heads|tags)\//, "");

function Frame({ session, route, children }: { session: Session | null; route: CiRoute; children: ReactNode }) {
  return (
    <Shell
      user={session?.user ?? null}
      nav={
        <Link href={`/ci/${route.owner}/${route.repo}`} className="font-mono text-sm text-bone hover:text-chick">
          {route.owner}/{route.repo}
        </Link>
      }
    >
      <div className="mx-auto flex w-full max-w-[1080px] flex-col gap-4">{children}</div>
    </Shell>
  );
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
  if (route.settings) {
    return <RepoSettings session={session} owner={route.owner} repo={route.repo} />;
  }
  if (route.run !== null && route.job !== null) {
    return <JobView session={session} route={route} run={route.run} job={route.job} />;
  }
  if (route.run !== null) {
    return <RunView session={session} route={route} run={route.run} />;
  }
  return <RepoRuns session={session} route={route} />;
}

function RepoRuns({ session, route }: { session: Session | null; route: CiRoute }) {
  const { owner, repo } = route;
  const [first, reload] = useLoad(() => listCiRuns(owner, repo), `${owner}/${repo}`, true);
  // Runs of later pages, appended by "Load more".
  const [more, setMore] = useState<CiRun[]>([]);
  // Runs on the last page "Load more" fetched; null before the first click.
  const [lastPage, setLastPage] = useState<number | null>(null);
  const [moreError, setMoreError] = useState<Error | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [dispatching, setDispatching] = useState(false);
  const active = first.state === "ready" && first.data.runs.some((run) => run.status !== "completed");
  useRefresh(active, reload);

  if (first.state === "error" && turnedAway(first.error, session)) {
    return <SignIn returnTo={`/ci/${owner}/${repo}`} />;
  }
  const loadMore = async (before: number) => {
    setLoadingMore(true);
    setMoreError(null);
    try {
      const page = await listCiRuns(owner, repo, before);
      setMore((current) => [...current, ...page.runs]);
      setLastPage(page.runs.length);
    } catch (err) {
      setMoreError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setLoadingMore(false);
    }
  };
  return (
    <Frame session={session} route={route}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="display text-[26px] leading-[1.05]">Runs</h1>
        {session && (
          <div className="flex gap-2">
            <Link href={`/ci/${owner}/${repo}/settings`} className="button">
              Settings
            </Link>
            <button type="button" className="button button-primary" onClick={() => setDispatching(true)}>
              Run workflow
            </button>
          </div>
        )}
      </div>
      <Await loaded={first} retry={reload}>
        {({ runs }) => {
          const all = [...runs, ...more.filter((run) => !runs.some((seen) => seen.id === run.id))];
          const last = all[all.length - 1];
          // A full page means there may be older runs.
          const maybeMore = (lastPage ?? runs.length) >= 50;
          return all.length === 0 ? (
            <p className="muted">No runs yet.</p>
          ) : (
            <>
              <RunTable route={route} runs={all} />
              {moreError && <LoadError error={moreError} />}
              {maybeMore && last && (
                <div>
                  <button
                    type="button"
                    className="button"
                    disabled={loadingMore}
                    onClick={() => void loadMore(last.id)}
                  >
                    {loadingMore ? "Loading" : "Load more"}
                  </button>
                </div>
              )}
            </>
          );
        }}
      </Await>
      {session && (
        <DispatchDialog
          owner={owner}
          repo={repo}
          open={dispatching}
          onOpenChange={setDispatching}
          onStarted={(id) => navigate(`/ci/${owner}/${repo}/runs/${id}`)}
        />
      )}
    </Frame>
  );
}

function RunTable({ route, runs }: { route: CiRoute; runs: CiRun[] }) {
  const now = useNow(1000);
  return (
    <section aria-label="Runs" className="overflow-hidden rounded-md border border-rivet">
      <Table className="min-w-[760px]">
        <TableHeader>
          <TableRow>
            <TableHead className="cap">Status</TableHead>
            <TableHead className="cap">Run</TableHead>
            <TableHead className="cap">Ref</TableHead>
            <TableHead className="cap">Commit</TableHead>
            <TableHead className="cap">By</TableHead>
            <TableHead className="cap text-right">Took</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {runs.map((run) => (
            <TableRow key={run.id}>
              <TableCell>
                <StatusMark shown={shown(run.status, run.conclusion)} />
              </TableCell>
              <TableCell>
                <Link
                  href={`/ci/${route.owner}/${route.repo}/runs/${run.id}`}
                  className="font-semibold text-bone hover:text-chick"
                >
                  {run.workflow} #{run.run_number}
                </Link>
                <span className="ml-2 font-mono text-xs text-fog">{run.event}</span>
              </TableCell>
              <TableCell className="font-mono text-xs">{shortRef(run.ref)}</TableCell>
              <TableCell className="font-mono text-xs">{run.sha.slice(0, 7)}</TableCell>
              <TableCell className="font-mono text-xs">{run.actor}</TableCell>
              <TableCell className="text-right font-mono text-xs">{took(run, now)}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </section>
  );
}

const took = (run: Pick<CiRun, "status" | "created_at" | "completed_at">, now: number) =>
  span((run.completed_at ?? now) - run.created_at);

function Crumbs({ route, children }: { route: CiRoute; children: ReactNode }) {
  return (
    <nav aria-label="Breadcrumb" className="flex items-center gap-2 font-mono text-xs text-fog">
      <Link href={`/ci/${route.owner}/${route.repo}`} className="inline-flex items-center gap-1.5 text-bone hover:text-chick">
        <ChevronLeftIcon className="text-chick" />
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
    return <SignIn returnTo={`/ci/${route.owner}/${route.repo}/runs/${run}`} />;
  }
  return (
    <Frame session={session} route={route}>
      <Crumbs route={route}>
        <span className="text-rivet">/</span>
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
        <StatusMark shown={shown(run.status, run.conclusion)}>
          <span className="font-normal text-fog">{run.completed_at ? `took ${took(run, now)}` : `for ${took(run, now)}`}</span>
        </StatusMark>
        <h1 className="display text-[26px] leading-[1.05] break-words">
          {run.workflow}{" "}
          <span className="font-mono text-xl font-medium tracking-normal text-fog normal-case [font-stretch:100%]">
            #{run.run_number}
          </span>
        </h1>
        <span className="font-mono text-xs text-fog">
          {run.repository} · {shortRef(run.ref)} · {run.sha.slice(0, 7)} · {run.event} by {run.actor}
          {run.run_attempt > 1 && <> · attempt {run.run_attempt}</>}
        </span>
      </div>
      <section aria-label="Jobs" className="overflow-hidden rounded-md border border-rivet">
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
                    <StatusMark shown={shown(job.status, job.conclusion)} />
                  </TableCell>
                  <TableCell>
                    <Link
                      href={`/ci/${route.owner}/${route.repo}/runs/${run.id}/jobs/${job.id}`}
                      className="font-semibold text-bone hover:text-chick"
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
    </>
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
    return <p className="text-sm text-fog">Approved by {job.approval?.by}</p>;
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
    return <SignIn returnTo={`/ci/${route.owner}/${route.repo}/runs/${run}/jobs/${job}`} />;
  }
  return (
    <Frame session={session} route={route}>
      <Crumbs route={route}>
        <span className="text-rivet">/</span>
        <Link href={`/ci/${route.owner}/${route.repo}/runs/${run}`} className="text-bone hover:text-chick">
          Run {run}
        </Link>
        <span className="text-rivet">/</span>
        <span>{found?.name ?? `Job ${job}`}</span>
      </Crumbs>
      {detail.state === "loading" && <Spinner />}
      {detail.state === "error" && <Denied error={detail.error} session={session} retry={reload} />}
      {detail.state === "ready" && found === undefined && <p className="muted">This run has no such job.</p>}
      {detail.state === "ready" && detail.stale && (
        <p className="font-mono text-xs text-fog" role="status">
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
      <StatusMark shown={shown(job.status, job.conclusion)}>
        {job.started_at !== null && <span className="font-normal text-fog">{jobTook(job, now)}</span>}
      </StatusMark>
      <h1 className="display text-[26px] leading-[1.05] break-words">{job.name}</h1>
    </div>
  );
}

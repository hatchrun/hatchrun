// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Fragment, useEffect, useState } from "react";

import { getRun, type Job, type JobResources, type RunDetail } from "@/api";
import { ChevronLeftIcon, ChevronRightIcon, ExternalIcon, GitHubIcon } from "@/components/icons";
import {
  endOf,
  instanceUrl,
  runName,
  runRef,
  runSpan,
  runState,
  span,
  state,
  StatusMark,
  type Tone,
} from "@/components/JobStatus";
import { Await } from "@/components/Status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useLoad, useNow } from "@/load";
import { cn } from "@/lib/utils";
import { Link } from "@/router";
import { ProblemRow, problem } from "./Dashboard";
import { bytes, Destination, pct } from "./JobPage";

/** Seconds between refreshes while a job of the run is still going. */
const REFRESH = 10;

/** One run: its jobs, attempt by attempt, where each ran and what it used, and when. */
export function RunPage({ org, id }: { org: string; id: string }) {
  const [detail, reload] = useLoad(() => getRun(org, id), `${org}/${id}`);
  const live = detail.state === "ready" && detail.data.run.jobs.some((job) => job.current && job.status !== "completed");
  useEffect(() => {
    if (!live) {
      return;
    }
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") {
        reload();
      }
    }, REFRESH * 1000);
    return () => window.clearInterval(timer);
  }, [live, reload]);
  return (
    <div className="flex flex-col gap-4">
      <nav aria-label="Breadcrumb" className="flex flex-wrap items-center gap-2 font-mono text-xs text-fog">
        <Link href={`/o/${org}`} className="inline-flex items-center gap-1.5 text-bone hover:text-chick">
          <ChevronLeftIcon className="text-chick" />
          Jobs
        </Link>
        {detail.state === "ready" && (
          <>
            <span className="text-rivet">/</span>
            <span>{detail.data.run.repository}</span>
            <span className="text-rivet">/</span>
            <span>
              {runName(detail.data.run)} {runRef(detail.data.run)}
            </span>
          </>
        )}
      </nav>
      <Await loaded={detail} retry={reload}>
        {(data) => <RunView org={org} data={data} />}
      </Await>
    </div>
  );
}

function RunView({ org, data }: { org: string; data: RunDetail }) {
  const { run } = data;
  const now = useNow(1000);
  // null is the latest view: the current job of each name, whatever its attempt.
  const [attempt, setAttempt] = useState<number | null>(null);
  const current = run.jobs.filter((job) => job.current);
  const shownJobs = attempt === null ? current : run.jobs.filter((job) => job.runAttempt === attempt);
  const shown = runState(current);
  const took = runSpan(run, current, now);
  const resources = new Map((data.resources ?? []).map((r) => [r.job, r]));
  const region = data.region ?? null;
  const by = [run.headBranch, run.event, run.headSha.slice(0, 7), run.actor && `by ${run.actor}`].filter(Boolean);
  return (
    <>
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex min-w-0 flex-col gap-2">
          <StatusMark shown={shown}>
            <span className="font-normal text-fog">
              {took.live ? `for ${span(took.ms)}` : `took ${span(took.ms)}`}
            </span>
          </StatusMark>
          <h1 className="display text-[26px] leading-[1.05] break-words">
            {runName(run)}{" "}
            <span className="font-mono text-xl font-medium tracking-normal text-fog normal-case [font-stretch:100%]">
              {runRef(run)}
            </span>
          </h1>
          <span className="font-mono text-xs text-fog">
            {by.join(" · ")}
            {run.displayTitle && <span className="text-bone-dim"> · “{run.displayTitle}”</span>}
          </span>
        </div>
        <div className="flex flex-wrap gap-2">
          <Destination
            href={run.githubUrl}
            label="The run on GitHub, opens github.com"
            icon={<GitHubIcon size={18} className="text-bone" />}
            caption="Run on GitHub"
            value={`${run.repository} ${runRef(run)}`}
          />
          <Destination
            href={run.githubUrl.replace(/\/actions\/runs\/\d+$/, `/commit/${run.headSha}`)}
            label="The commit on GitHub, opens github.com"
            icon={<GitHubIcon size={18} className="text-bone" />}
            caption="Commit on GitHub"
            value={run.headSha.slice(0, 7)}
          />
        </div>
      </div>

      {run.attempt > 1 && <Attempts attempts={run.attempt} picked={attempt} onPick={setAttempt} />}

      <section aria-label="Jobs" className="overflow-hidden rounded-md border border-rivet">
        <Table className="min-w-[1040px]">
          <TableHeader>
            <TableRow>
              <TableHead className="cap">Status</TableHead>
              <TableHead>
                <span className="cap">Job</span> <span className="ml-1 font-mono text-[10px] text-chick">→ details</span>
              </TableHead>
              <TableHead className="cap">Label</TableHead>
              <TableHead>
                <span className="cap">EC2 instance</span>{" "}
                <span className="ml-1 font-mono text-[10px] text-steel">↗ aws console</span>
              </TableHead>
              <TableHead className="cap text-right" title="From the job being queued to a runner taking it">
                Waited
              </TableHead>
              <TableHead className="cap text-right">Ran</TableHead>
              <TableHead className="cap text-right">CPU</TableHead>
              <TableHead className="cap text-right">Memory</TableHead>
              <TableHead className="cap text-right" title="Processes killed for lack of memory">
                OOM
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {shownJobs.map((job, i) => (
              <JobRow
                key={job.id}
                org={org}
                job={job}
                region={region}
                now={now}
                last={i === shownJobs.length - 1}
                resources={resources.get(String(job.id))}
                attempt={run.attempt > 1 && attempt === null ? job.runAttempt : null}
              />
            ))}
          </TableBody>
        </Table>
      </section>

      {shownJobs.length > 0 && <Timeline jobs={shownJobs} now={now} />}

      <p className="font-mono text-xs text-steel">
        Only jobs on hatchrun runners show here. Jobs on other runners, and every job&apos;s steps and logs, are on
        GitHub.
      </p>
    </>
  );
}

/** Latest, then each attempt, newest first. */
function Attempts({
  attempts,
  picked,
  onPick,
}: {
  attempts: number;
  picked: number | null;
  onPick: (attempt: number | null) => void;
}) {
  const options: { id: number | null; label: string }[] = [
    { id: null, label: "Latest" },
    ...Array.from({ length: attempts }, (_, i) => ({ id: attempts - i, label: `Attempt ${attempts - i}` })),
  ];
  return (
    <div
      role="group"
      aria-label="Attempt"
      className="flex w-fit overflow-hidden rounded-sm border border-rivet font-mono text-xs"
    >
      {options.map((option) => {
        const on = option.id === picked;
        return (
          <button
            key={option.label}
            type="button"
            aria-pressed={on}
            onClick={() => onPick(option.id)}
            className={cn(
              "cursor-pointer border-r border-rivet px-3 py-1.5 last:border-r-0",
              on ? "bg-bone text-graphite" : "bg-transparent text-bone-dim hover:bg-seam hover:text-bone",
            )}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}

function JobRow({
  org,
  job,
  region,
  now,
  last,
  resources,
  attempt,
}: {
  org: string;
  job: Job;
  region: string | null;
  now: number;
  last: boolean;
  resources: JobResources | undefined;
  attempt: number | null;
}) {
  const shown = state(job);
  const trouble = problem(job);
  const edge =
    shown.tone === "failed"
      ? "shadow-[inset_3px_0_0_var(--failed)]"
      : shown.tone === "retrying"
        ? "shadow-[inset_3px_0_0_var(--chick)]"
        : "";
  const waited = job.startedAt !== null ? job.startedAt - job.queuedAt : job.status === "queued" ? now - job.queuedAt : null;
  const ran =
    job.startedAt === null ? null : (job.status === "completed" ? endOf(job) : now) - job.startedAt;
  const oom = Math.round(resources?.oomKills ?? 0);
  return (
    <Fragment>
      <TableRow>
        <TableCell className={edge}>
          <StatusMark shown={shown} />
        </TableCell>
        <TableCell>
          <Link
            href={`/o/${org}/jobs/${job.id}`}
            aria-label={`Job ${job.name || job.id}, open details`}
            className="group inline-flex items-center gap-2.5"
          >
            <span className="font-bold text-bone group-hover:text-chick">{job.name || `#${job.id}`}</span>
            {attempt !== null && <span className="font-mono text-[11px] text-fog">attempt {attempt}</span>}
            <ChevronRightIcon className="text-chick" />
          </Link>
        </TableCell>
        <TableCell className="font-mono text-xs text-bone-dim">{job.label}</TableCell>
        <TableCell className="font-mono text-xs">
          {job.instanceId ? (
            region ? (
              <a
                href={instanceUrl(region, job.instanceId)}
                aria-label={`Instance ${job.instanceId}, opens the AWS EC2 console`}
                className="group inline-flex items-center gap-1.5 text-bone-dim hover:text-bone"
              >
                <span className="quiet-underline group-hover:decoration-fog">{job.instanceId}</span>
                <ExternalIcon />
              </a>
            ) : (
              <span className="text-bone-dim">{job.instanceId}</span>
            )
          ) : (
            <span className="text-steel">{job.status === "completed" || shown.tone === "failed" ? "none" : "none yet"}</span>
          )}
        </TableCell>
        <TableCell className={cn("text-right font-mono text-xs", waited === null ? "text-steel" : "text-bone")}>
          {waited === null ? "-" : span(waited)}
        </TableCell>
        <TableCell
          className={cn(
            "text-right font-mono text-xs",
            ran === null ? "text-steel" : job.status === "in_progress" ? "text-blush" : "text-bone",
          )}
        >
          {ran === null ? "-" : span(ran)}
        </TableCell>
        <TableCell className="text-right font-mono text-xs text-bone">
          {resources ? (
            <span title={`highest ${pct(resources.cpuMax)}`}>{pct(resources.cpuAvg)}</span>
          ) : (
            <span className="text-steel">-</span>
          )}
        </TableCell>
        <TableCell className="text-right font-mono text-xs text-bone">
          {resources?.memMax != null ? (
            <span title={resources.memTotal != null ? `of ${bytes(resources.memTotal)}` : undefined}>
              {bytes(resources.memMax)}
            </span>
          ) : (
            <span className="text-steel">-</span>
          )}
        </TableCell>
        <TableCell className={cn("text-right font-mono text-xs", oom > 0 ? "font-bold text-failed" : "text-steel")}>
          {resources ? oom : "-"}
        </TableCell>
      </TableRow>
      {trouble && <ProblemRow trouble={trouble} edge={edge} last={last} colSpan={9} />}
    </Fragment>
  );
}

const BAR: Record<Tone, string> = {
  running: "var(--heat)",
  starting: "var(--blush)",
  waiting: "var(--steel)",
  done: "var(--passed)",
  retrying: "var(--chick)",
  failed: "var(--failed)",
};

const ROW = 22;
const LABEL = 200;
const WIDTH = 1100;

/**
 * Each job on one time axis from the first job's queue time: dashed
 * while it waited for a runner, solid while it ran. A job that waited
 * long shows at a glance.
 */
function Timeline({ jobs, now }: { jobs: Job[]; now: number }) {
  const start = Math.min(...jobs.map((job) => job.queuedAt));
  const ends = jobs.map((job) => (job.status === "completed" ? endOf(job) : now));
  const end = Math.max(start + 60_000, ...ends);
  const x = (at: number) => LABEL + ((at - start) / (end - start)) * (WIDTH - LABEL - 8);
  const height = jobs.length * ROW + 24;
  return (
    <section aria-label="Timeline" className="min-w-0 rounded-md border border-rivet bg-panel px-4 py-3.5">
      <div className="mb-2.5 flex flex-wrap justify-between gap-x-3 gap-y-1.5">
        <span className="cap text-bone">Timeline</span>
        <span className="font-mono text-[11px] text-fog">
          <span className="text-steel">┅</span> waiting for a runner <span className="ml-2.5 text-bone">━</span> running
        </span>
      </div>
      <svg
        viewBox={`0 0 ${WIDTH} ${height}`}
        role="img"
        aria-label={`When each of the ${jobs.length} jobs waited and ran`}
        className="block h-auto w-full"
      >
        {jobs.map((job, i) => {
          const y = i * ROW + ROW / 2;
          const jobEnd = ends[i] ?? now;
          const started = job.startedAt ?? (job.status === "completed" ? jobEnd : null);
          const waitEnd = started ?? now;
          const tone = state(job).tone;
          return (
            <g key={job.id}>
              <text x={0} y={y + 4} fontSize="11" className="font-mono" fill="var(--bone-dim)">
                {(job.name || `#${job.id}`).slice(0, 24)}
              </text>
              <line
                x1={x(job.queuedAt)}
                x2={x(waitEnd)}
                y1={y}
                y2={y}
                stroke="var(--fog)"
                strokeDasharray="3 3"
                strokeWidth={1.5}
              />
              {started !== null && jobEnd > started && (
                <rect x={x(started)} y={y - 5} width={Math.max(2, x(jobEnd) - x(started))} height={10} rx={1} fill={BAR[tone]} />
              )}
            </g>
          );
        })}
        <g className="font-mono" fontSize="11" fill="var(--steel)">
          <text x={LABEL} y={height - 4}>
            0 min
          </text>
          <text x={WIDTH - 4} y={height - 4} textAnchor="end">
            {span(end - start)}
          </text>
        </g>
      </svg>
    </section>
  );
}

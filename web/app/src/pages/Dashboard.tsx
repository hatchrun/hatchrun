// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { Fragment, useEffect, useRef, useState } from "react";

import { listJobs, type Job, type JobCounts, type JobPage, type Stack, type Tenant } from "@/api";
import { CopyButton } from "@/components/CopyButton";
import { ChevronRightIcon, ExternalIcon, GitHubIcon, OpenIcon } from "@/components/icons";
import { ago, instanceUrl, span, state, StatusMark, TONE_TEXT } from "@/components/JobStatus";
import { LoadError, Spinner } from "@/components/Status";
import { useNow } from "@/load";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { Link, useQuery } from "@/router";

/** Seconds between refreshes of the job list. */
const REFRESH = 10;
/** Jobs in a page when the API does not say. */
const PAGE = 50;
/** The profile the page suggests for runs-on. */
const SUGGESTED = "hatchrun-4cpu-arm64";

export function Dashboard({ tenant }: { tenant: Tenant }) {
  const org = tenant.org.login;
  const installed = useQuery("installed");
  const [page, setPage] = useState<JobPage | null>(null);
  const [older, setOlder] = useState<Job[]>([]);
  const [error, setError] = useState<Error | null>(null);
  const [more, setMore] = useState(true);
  const [paging, setPaging] = useState(false);
  const table = useRef<HTMLElement>(null);
  const now = useNow(REFRESH * 1000);

  useEffect(() => {
    let current = true;
    const load = () =>
      listJobs(org).then(
        (next) => {
          if (current) {
            setPage(next);
            setError(null);
          }
        },
        (err: unknown) => current && setError(err instanceof Error ? err : new Error(String(err))),
      );
    load();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") {
        load();
      }
    }, REFRESH * 1000);
    return () => {
      current = false;
      window.clearInterval(timer);
    };
  }, [org]);

  const pageSize = page?.pageSize ?? PAGE;
  const jobs = page ? [...page.jobs, ...older.filter((job) => !page.jobs.some((seen) => seen.id === job.id))] : [];
  const last = jobs[jobs.length - 1];
  const loadOlder = async () => {
    if (!last) {
      return;
    }
    setPaging(true);
    try {
      const next = await listJobs(org, last.queuedAt);
      setOlder((was) => [...was, ...next.jobs]);
      setMore(next.jobs.length === (next.pageSize ?? PAGE));
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setPaging(false);
    }
  };
  const backToNewest = () => {
    setOlder([]);
    setMore(true);
    table.current?.scrollIntoView({ block: "start" });
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1.5">
          <h1 className="display text-[26px]">Jobs</h1>
          <span className="inline-flex items-center gap-2 font-mono text-xs text-fog">
            <span aria-hidden="true" className={cn("size-1.5 rounded-full", error ? "bg-failed" : "bg-passed")} />
            {error ? "refresh failed, trying again" : `live, refreshes every ${REFRESH} s`}
          </span>
        </div>
        <RunsOn text={SUGGESTED} />
      </div>

      {installed && (
        <p role="status" className="rounded-md border border-rivet border-l-3 border-l-passed bg-panel px-3.5 py-2.5">
          The hatchrun App is installed. {org} is ready to run jobs.
        </p>
      )}

      <Fleet max={tenant.maxRunners} counts={page?.counts ?? null} jobs={page?.jobs ?? []} />

      {tenant.aws && <StackUpdate stack={tenant.aws.stack} region={tenant.aws.region} />}

      <section ref={table} aria-label="Jobs" className="scroll-mt-4 overflow-hidden rounded-md border border-rivet">
        <div className="flex flex-wrap items-center justify-between gap-2.5 border-b border-rivet bg-panel px-3.5 py-2.5">
          <span className="cap">Recent jobs</span>
          <span className="font-mono text-xs text-fog">newest first</span>
        </div>
        {error && (
          <div className="border-b border-rivet p-3.5">
            <LoadError error={error} />
          </div>
        )}
        {!page && !error && (
          <div className="p-3.5">
            <Spinner />
          </div>
        )}
        {page && jobs.length === 0 && (
          <p className="px-3.5 py-10 text-center text-fog">
            No jobs yet. Push a workflow with <code className="font-mono text-bone">runs-on: {SUGGESTED}</code> and it
            shows up here. <Link href={`/o/${org}/runners`} className="quiet-underline text-bone-dim hover:text-bone">See every runner profile</Link>.
          </p>
        )}
        {jobs.length > 0 && (
          <>
            <Table className="min-w-[1040px]">
              <TableHeader>
                <TableRow>
                  <TableHead className="cap">Status</TableHead>
                  <TableHead>
                    <span className="cap">Job</span>{" "}
                    <span className="ml-1 font-mono text-[10px] text-chick">→ details</span>
                  </TableHead>
                  <TableHead>
                    <span className="cap">GitHub run</span>{" "}
                    <span className="ml-1 font-mono text-[10px] text-steel">↗ logs</span>
                  </TableHead>
                  <TableHead className="cap">Label</TableHead>
                  <TableHead>
                    <span className="cap">EC2 instance</span>{" "}
                    <span className="ml-1 font-mono text-[10px] text-steel">↗ aws console</span>
                  </TableHead>
                  <TableHead className="cap">Queued</TableHead>
                  <TableHead className="cap text-right">Took</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {jobs.map((job) => (
                  <JobRow key={job.id} org={org} job={job} region={page?.region ?? null} now={now} />
                ))}
              </TableBody>
            </Table>
            <div className="flex flex-wrap items-center justify-between gap-2.5 border-t border-rivet bg-panel px-3.5 py-2.5 font-mono text-xs text-fog">
              <span>
                {jobs.length} {jobs.length === 1 ? "job" : "jobs"} shown
                {last && <>, back to {ago(last.queuedAt)}</>}
              </span>
              <div className="flex gap-2">
                {older.length > 0 && (
                  <Button variant="outline" onClick={backToNewest}>
                    ↑ Back to newest
                  </Button>
                )}
                {more && jobs.length >= pageSize && (
                  <Button variant="outline" onClick={loadOlder} disabled={paging}>
                    {paging ? "Loading" : `Load ${pageSize} older ↓`}
                  </Button>
                )}
              </div>
            </div>
          </>
        )}
      </section>
    </div>
  );
}

/** The profile to put in runs-on, with a copy button. */
function RunsOn({ text }: { text: string }) {
  return (
    <div className="flex items-stretch rounded-sm border border-rivet bg-housing font-mono text-xs">
      <span className="px-2.5 py-1.5 text-fog">runs-on:</span>
      <span className="py-1.5 pr-2.5 text-chick">{text}</span>
      <CopyButton
        text={`runs-on: ${text}`}
        label="copy"
        className="cursor-pointer border-0 border-l border-rivet bg-transparent px-2.5 text-bone hover:bg-seam"
      />
    </div>
  );
}

/**
 * The runner slots under max runners: running, booting and free, with
 * the jobs GitHub has queued beside them; and the last day's totals.
 */
function Fleet({ max, counts, jobs }: { max: number; counts: JobCounts | null; jobs: Job[] }) {
  const active = counts?.active ?? 0;
  const running = Math.min(active, counts?.running ?? jobs.filter((job) => job.status === "in_progress").length);
  const booting = active - running;
  const free = Math.max(0, max - active);
  const waiting = counts?.waiting ?? 0;
  const slots = Math.max(max, active);
  const queued = Math.min(waiting, 6);
  const n = (value: number) => (counts ? value : "-");

  return (
    <section
      aria-label="Fleet now and today"
      className="flex flex-wrap rounded-md border border-rivet bg-panel"
    >
      <div className="flex min-w-0 flex-[2_1_460px] flex-col gap-3 px-4.5 py-4">
        <div className="flex items-baseline justify-between gap-3">
          <span className="cap">Runner slots</span>
          <span className="font-mono text-xs text-fog">
            <span className="text-lg font-bold text-bone">{n(active)}</span> / {max} in use
          </span>
        </div>
        <div className="flex items-center gap-3.5">
          <div
            role="img"
            aria-label={`${running} running, ${booting} booting, ${free} free`}
            className="grid flex-1 grid-cols-10 gap-[3px] sm:grid-cols-20"
          >
            {Array.from({ length: slots }, (_, i) => (
              <span
                key={i}
                className={cn(
                  "h-4 rounded-xs border",
                  i < running
                    ? "border-heat bg-heat"
                    : i < active
                      ? "border-heat bg-[repeating-linear-gradient(135deg,var(--heat)_0_2px,transparent_2px_5px)]"
                      : "border-rivet",
                )}
              />
            ))}
          </div>
          <span aria-hidden="true" className="font-mono text-steel">
            ←
          </span>
          <div className="flex flex-col items-center gap-[3px]" aria-label={`${waiting} waiting on GitHub`} role="img">
            <div className="flex gap-[3px]">
              {queued === 0 ? (
                <span className="h-4 w-3.5 rounded-xs border border-dashed border-rivet" />
              ) : (
                Array.from({ length: queued }, (_, i) => (
                  <span key={i} className="h-4 w-3.5 rounded-xs border border-dashed border-fog" />
                ))
              )}
              {waiting > queued && <span className="font-mono text-[10px] text-fog">+{waiting - queued}</span>}
            </div>
            <span className="font-mono text-[10px] text-fog">queue</span>
          </div>
        </div>
        <div className="flex flex-wrap gap-x-4.5 gap-y-1.5 font-mono text-xs text-fog">
          <Legend swatch="border-heat bg-heat" value={n(running)} label="running" />
          <Legend
            swatch="border-heat bg-[repeating-linear-gradient(135deg,var(--heat)_0_2px,transparent_2px_4px)]"
            value={n(booting)}
            label="booting"
          />
          <Legend swatch="border-rivet" value={n(free)} label="free" />
          <Legend swatch="border-dashed border-fog" value={n(waiting)} label="waiting on GitHub" />
        </div>
      </div>
      <div className="flex flex-[1_1_260px] flex-col justify-between gap-3 border-t border-rivet px-4.5 py-4 sm:border-t-0 sm:border-l">
        <span className="cap">Last 24 hours</span>
        <div className="flex flex-wrap gap-x-8 gap-y-2 font-mono text-xs text-fog">
          <span className="flex flex-col">
            <span className="text-[22px] leading-tight font-bold text-bone">{n(counts?.completedToday ?? 0)}</span>
            finished
          </span>
          <span className="flex flex-col">
            <span
              className={cn(
                "text-[22px] leading-tight font-bold",
                (counts?.failedToday ?? 0) > 0 ? "text-failed" : "text-bone",
              )}
            >
              {n(counts?.failedToday ?? 0)}
            </span>
            {counts?.failedToday === 1 ? "failed launch" : "failed launches"}
          </span>
        </div>
      </div>
    </section>
  );
}

function Legend({ swatch, value, label }: { swatch: string; value: number | string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span aria-hidden="true" className={cn("size-[9px] rounded-[1px] border", swatch)} />
      <span className="text-bone">{value}</span> {label}
    </span>
  );
}

/** Asks the owner to update an older connect stack, in one line; the steps open on demand. */
function StackUpdate({ stack, region }: { stack: Stack; region: string }) {
  const [open, setOpen] = useState(false);
  if (stack.changes.length === 0 && !stack.familyList) {
    return null;
  }
  const outdated = stack.changes.length > 0;
  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      role="status"
      className="flex flex-wrap items-center gap-x-4 gap-y-2.5 rounded-md border border-rivet border-l-3 border-l-chick bg-panel px-3.5 py-2.5"
    >
      <span className="cap text-chick">Stack update</span>
      <span className="flex-[1_1_360px] text-bone-dim">
        {outdated ? (
          <>
            Your <code className="font-mono text-bone">hatchrun</code> stack in {region} is version {stack.version} and
            version {stack.latest} is out. Runners launch the old way until you update.
          </>
        ) : (
          <>
            Your <code className="font-mono text-bone">hatchrun</code> stack in {region} still names instance families,
            so types AWS adds stay off until you change InstanceTypes.
          </>
        )}
      </span>
      <CollapsibleTrigger asChild>
        <Button variant="outline">{open ? "Hide steps" : "Show steps"}</Button>
      </CollapsibleTrigger>
      <Button variant="outline" asChild className="text-chick">
        <a href={stack.consoleUrl}>
          CloudFormation
          <OpenIcon />
        </a>
      </Button>
      <CollapsibleContent className="basis-full font-mono text-xs text-bone-dim">
        {outdated && (
          <>
            <p className="mt-1 mb-1.5 text-fog">What the update brings:</p>
            <ul className="mb-3 flex list-disc flex-col gap-1 pl-5.5">
              {stack.changes.map((change) => (
                <li key={change}>{change}</li>
              ))}
            </ul>
          </>
        )}
        {!outdated && (
          <p className="mt-1 mb-3">
            It allows only the families it lists. The current default allows any size up to 8xlarge instead.
          </p>
        )}
        <ol className="flex list-decimal flex-col gap-1 pl-5.5">
          <li>Open the hatchrun stack in CloudFormation, choose Update, then Replace existing template.</li>
          <li>
            Choose Amazon S3 URL and paste{" "}
            {stack.templateUrl ? (
              <>
                <code className="break-all text-bone">{stack.templateUrl}</code>{" "}
                <CopyButton text={stack.templateUrl} label="copy URL" className={INLINE_COPY} />
              </>
            ) : (
              "the template URL"
            )}
          </li>
          <li>
            Keep the parameters as they are
            {stack.familyList && (
              <>
                , except InstanceTypes: set it to{" "}
                <code className="text-bone">*.medium,*.large,*.xlarge,*.2xlarge,*.4xlarge,*.8xlarge</code> to stop
                naming families
              </>
            )}
            . Acknowledge that the stack changes IAM resources and submit.
          </li>
        </ol>
        {stack.updateCommand && (
          <div className="mt-3">
            <p className="mb-1.5 flex items-center gap-3 text-fog">
              Or with the AWS CLI <CopyButton text={stack.updateCommand} label="copy" className={INLINE_COPY} />
            </p>
            <pre className="overflow-x-auto rounded-sm border border-rivet bg-graphite p-3 text-bone">
              <code>{stack.updateCommand}</code>
            </pre>
          </div>
        )}
        <p className="mt-3 mb-0.5 text-fog">
          hatchrun reads the stack again once a day, so the update shows here by tomorrow, or at once when you verify
          the role again under Settings.
        </p>
      </CollapsibleContent>
    </Collapsible>
  );
}

const INLINE_COPY =
  "cursor-pointer rounded-sm border border-rivet bg-housing px-1.5 py-0.5 font-mono text-[11px] text-bone uppercase hover:border-steel";

function JobRow({ org, job, region, now }: { org: string; job: Job; region: string | null; now: number }) {
  const shown = state(job);
  const trouble = problem(job);
  const edge =
    shown.tone === "failed"
      ? "shadow-[inset_3px_0_0_var(--failed)]"
      : shown.tone === "retrying"
        ? "shadow-[inset_3px_0_0_var(--chick)]"
        : "";
  const live = job.status === "in_progress";
  const took =
    job.status === "completed" ? span(job.updatedAt - job.queuedAt) : live ? span(now - job.queuedAt) : null;
  return (
    <Fragment>
      <TableRow>
        <TableCell className={edge}>
          <StatusMark shown={shown} />
        </TableCell>
        <TableCell>
          <Link
            href={`/o/${org}/jobs/${job.id}`}
            aria-label={`Job ${job.id} in ${job.repository}, open details`}
            className="group inline-flex items-center gap-2.5"
          >
            <span className="font-bold text-bone group-hover:text-chick">{job.repository}</span>
            <span className="font-mono text-xs text-fog">#{job.id}</span>
            <ChevronRightIcon className="text-chick" />
          </Link>
        </TableCell>
        <TableCell className="font-mono">
          <a
            href={`https://github.com/${job.repository}/actions/runs/${job.runId}/job/${job.id}`}
            aria-label={`GitHub run ${job.runId} logs, opens github.com`}
            className="group inline-flex items-center gap-1.5 text-xs text-bone-dim hover:text-bone"
          >
            <GitHubIcon className="text-fog" />
            <span className="quiet-underline group-hover:decoration-fog">{job.runId}</span>
            <ExternalIcon />
          </a>
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
        <TableCell className="text-[13px] text-fog" title={new Date(job.queuedAt).toLocaleString()}>
          {ago(job.queuedAt)}
        </TableCell>
        <TableCell
          className={cn("text-right font-mono text-xs", took === null ? "text-steel" : live ? "text-blush" : "text-bone")}
        >
          {took ?? "-"}
        </TableCell>
      </TableRow>
      {trouble && (
        <TableRow className="border-t-0 hover:bg-transparent">
          <TableCell colSpan={7} className={cn("pt-0 pb-2.5 pl-9 font-mono text-xs whitespace-normal", edge)}>
            <span className="text-steel">└ </span>
            <span className={TONE_TEXT_PROBLEM[trouble.tone]}>{trouble.text}</span>
            {trouble.next && <span className="text-fog"> · {trouble.next}</span>}
          </TableCell>
        </TableRow>
      )}
    </Fragment>
  );
}

const TONE_TEXT_PROBLEM = {
  error: "text-failed-soft",
  retry: TONE_TEXT.retrying,
  note: "text-fog",
};

/** What went wrong with the job's launch and what happens next, in plain words. */
function problem(job: Job): { text: string; tone: keyof typeof TONE_TEXT_PROBLEM; next?: string } | null {
  if (!job.launchError) {
    return null;
  }
  const attempt = job.launchAttempts > 0 ? `Attempt ${job.launchAttempts} failed: ` : "";
  if (job.status !== "queued" || job.launchState === "launched") {
    return { text: job.launchError, tone: "error" };
  }
  if (job.launchState !== "failed") {
    return { text: `Trying again after a failed launch: ${job.launchError}`, tone: "note" };
  }
  if (job.launchRetry === "backoff") {
    return { text: `${attempt}${job.launchError}`, tone: "retry", next: nextTry(job.nextLaunchAt) };
  }
  if (job.launchRetry === "setup") {
    return {
      text: job.launchError,
      tone: "error",
      next:
        "This will fail the same way until the setup changes. It launches again once you verify the AWS role, edit a " +
        "runner profile, change the settings or get a licence.",
    };
  }
  return { text: job.launchError, tone: "error" };
}

/** When the reconciler, which runs every minute, tries the launch again. */
function nextTry(at: number | null): string {
  if (at === null) {
    return "Trying again soon.";
  }
  const seconds = Math.round((at - Date.now()) / 1000);
  if (seconds <= 0) {
    return "Trying again within a minute.";
  }
  if (seconds <= 60) {
    return "Trying again in about a minute.";
  }
  return `Trying again in about ${Math.ceil(seconds / 60)} minutes.`;
}

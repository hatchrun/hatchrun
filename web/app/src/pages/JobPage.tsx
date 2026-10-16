// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { getJobMetrics, type JobMetrics, type MetricsSummary, type MetricsWindow } from "@/api";
import { ChevronLeftIcon, ExternalIcon, GitHubIcon } from "@/components/icons";
import { endOf, instanceUrl, runRef, span, state, StatusMark } from "@/components/JobStatus";
import { Await } from "@/components/Status";
import { useLoad, useNow } from "@/load";
import { cn } from "@/lib/utils";
import { Link } from "@/router";

/** A gap this long between windows means the runner reported nothing for a while. */
const GAP_SECONDS = 30;

/** One job: where it ran, links to its logs and instance, and its CPU and memory. */
export function JobPage({ org, id }: { org: string; id: string }) {
  const [metrics, reload] = useLoad(() => getJobMetrics(org, id), `${org}/${id}`);
  return (
    <div className="flex flex-col gap-4">
      <nav aria-label="Breadcrumb" className="flex items-center gap-2 font-mono text-xs text-fog">
        <Link href={`/o/${org}`} className="inline-flex items-center gap-1.5 text-bone hover:text-chick">
          <ChevronLeftIcon className="text-chick" />
          Jobs
        </Link>
        {metrics.state === "ready" && (
          <>
            <span className="text-rivet">/</span>
            <span>{metrics.data.job.repository}</span>
            <span className="text-rivet">/</span>
            <Link href={`/o/${org}/runs/${metrics.data.job.runId}`} className="text-bone hover:text-chick">
              {metrics.data.job.workflowName ?? "Run"}{" "}
              {runRef({ runNumber: metrics.data.job.runNumber, id: metrics.data.job.runId })}
            </Link>
          </>
        )}
        <span className="text-rivet">/</span>
        <span>{metrics.state === "ready" && metrics.data.job.name ? metrics.data.job.name : `#${id}`}</span>
      </nav>
      <Await loaded={metrics} retry={reload}>
        {(data) => <JobView data={data} />}
      </Await>
    </div>
  );
}

function JobView({ data }: { data: JobMetrics }) {
  const { job, summary, windows } = data;
  const shown = state({ status: job.status, launchState: null, launchRetry: null, conclusion: job.conclusion });
  const region = data.region ?? null;
  const queued = new Date(job.queuedAt);
  const now = useNow(1000);
  return (
    <>
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex min-w-0 flex-col gap-2">
          <StatusMark shown={shown}>
            <span className="font-normal text-fog">
              {job.status === "in_progress"
                ? `for ${span(now - job.queuedAt)}`
                : job.status === "completed"
                  ? `took ${span(endOf(job) - job.queuedAt)}`
                  : `since ${queued.toLocaleTimeString()}`}
            </span>
          </StatusMark>
          <h1 className="display text-[26px] leading-[1.05] break-words">
            {job.name || job.repository}{" "}
            <span className="font-mono text-xl font-medium tracking-normal text-fog normal-case [font-stretch:100%]">
              #{job.id}
            </span>
          </h1>
          <span className="font-mono text-xs text-fog">
            <span className="text-chick">{job.label}</span>
            {job.runnerName && <> on {job.runnerName}</>} · queued{" "}
            <time dateTime={queued.toISOString()} title={queued.toLocaleString()}>
              {queued.toLocaleTimeString()}
            </time>
            {job.startedAt !== null && <> · waited {span(job.startedAt - job.queuedAt)}</>}
            {job.startedAt !== null && job.completedAt !== null && <>, ran {span(job.completedAt - job.startedAt)}</>}
            {job.runAttempt > 1 && <> · attempt {job.runAttempt}</>}
          </span>
        </div>
        <div className="flex flex-wrap gap-2">
          <Destination
            href={data.githubUrl}
            label="Logs on GitHub, opens github.com"
            icon={<GitHubIcon size={18} className="text-bone" />}
            caption="Logs on GitHub"
            value={`${job.repository} run ${job.runId}`}
          />
          <Destination
            href={job.instanceId && region ? instanceUrl(region, job.instanceId) : null}
            label="Instance in the AWS EC2 console"
            icon={<span className="w-[18px] font-mono text-[10px] font-bold text-blush">EC2</span>}
            caption="Instance in AWS console"
            value={job.instanceId ?? (job.status === "completed" ? "none" : "none yet")}
          />
        </div>
      </div>
      {summary && windows.length > 0 ? (
        <>
          <Readout summary={summary} />
          <div className="grid gap-4 lg:grid-cols-2">
            <ChartPanel
              title="CPU"
              legend={
                <>
                  <span className="text-heat">━</span> mean <span className="ml-2.5 text-ember">█</span> quietest to
                  busiest second
                </>
              }
            >
              <Chart
                windows={windows}
                max={100}
                format={(value) => `${Math.round(value)}%`}
                line={(w) => w.cpuAvg}
                band={(w) => [w.cpuMin, w.cpuMax]}
                tone="heat"
                label={`CPU: mean ${pct(summary.cpuAvg)}, highest ${pct(summary.cpuMax)}`}
              />
            </ChartPanel>
            <ChartPanel
              title="Memory"
              legend={
                <>
                  <span className="text-chick">━</span> highest in each 10 s <span className="ml-2.5">┅</span>{" "}
                  instance memory
                </>
              }
            >
              <Chart
                windows={windows}
                max={Math.max(...windows.map((w) => w.memTotal), 1)}
                format={bytes}
                line={(w) => w.memMax}
                tone="chick"
                label={`Memory: highest ${bytes(summary.memMax ?? 0)} of ${bytes(summary.memTotal ?? 0)}`}
              />
            </ChartPanel>
          </div>
          <p className="font-mono text-xs text-steel">
            The runner reports once a minute while the job runs. Steps and logs stay on GitHub.
          </p>
        </>
      ) : (
        <div className="rounded-md border border-rivet bg-panel px-4 py-6">
          <p className="mb-1 font-semibold">No metrics for this job.</p>
          <p className="text-[13px] text-fog">
            Runners report once a minute while the job runs, so a job that just started shows up shortly. A job that
            never got a runner has nothing to show. Its logs are on GitHub.
          </p>
        </div>
      )}
    </>
  );
}

/** A link button that names where it goes; without an href it says what is missing. */
export function Destination({
  href,
  label,
  icon,
  caption,
  value,
}: {
  href: string | null;
  label: string;
  icon: ReactNode;
  caption: string;
  value: string;
}) {
  const body = (
    <>
      {icon}
      <span className="flex flex-col">
        <span className="cap text-[10px]">{caption}</span>
        <span className={cn("font-mono text-xs", href ? "quiet-underline text-bone" : "text-steel")}>{value}</span>
      </span>
      {href && <ExternalIcon size={11} className="text-fog" />}
    </>
  );
  const box = "flex items-center gap-3 rounded-md border border-rivet bg-panel px-3 py-2";
  if (!href) {
    return <div className={cn(box, "opacity-80")}>{body}</div>;
  }
  return (
    <a href={href} aria-label={label} className={cn(box, "hover:border-steel")}>
      {body}
    </a>
  );
}

/** The six numbers in one strip. */
function Readout({ summary }: { summary: MetricsSummary }) {
  const oom = Math.round(summary.oomKills ?? 0);
  const cells: { label: string; value: string; hint: string; tone?: string; edge?: boolean }[] = [
    { label: "CPU", value: pct(summary.cpuAvg), hint: `mean, highest ${pct(summary.cpuMax)}` },
    { label: "Memory", value: bytes(summary.memMax ?? 0), hint: `highest, of ${bytes(summary.memTotal ?? 0)}` },
    {
      label: "Out of memory",
      value: String(oom),
      hint: oom > 0 ? "processes killed" : "nothing killed",
      tone: oom > 0 ? "text-failed" : "text-passed",
      edge: oom > 0,
    },
    { label: "Disk", value: bytes(summary.diskUsed ?? 0), hint: "used at most" },
    { label: "Network", value: bytes(summary.netRx ?? 0), hint: `in, ${bytes(summary.netTx ?? 0)} out` },
    { label: "Steal", value: pct(summary.cpuSteal), hint: "CPU taken by other guests" },
  ];
  return (
    <section
      aria-label="The job's resources"
      className="grid grid-cols-2 overflow-hidden rounded-md border border-rivet bg-panel sm:grid-cols-3 xl:grid-cols-6"
    >
      {cells.map((cell) => (
        <div
          key={cell.label}
          className={cn(
            "-mt-px -ml-px flex flex-col gap-1 border-t border-l border-seam px-4 py-3.5",
            cell.edge && "shadow-[inset_0_2px_0_var(--failed)]",
          )}
        >
          <span className="cap">{cell.label}</span>
          <span className={cn("font-mono text-[22px] leading-[1.1] font-bold", cell.tone ?? "text-bone")}>
            {cell.value}
          </span>
          <span className="text-xs text-fog">{cell.hint}</span>
        </div>
      ))}
    </section>
  );
}

function ChartPanel({ title, legend, children }: { title: string; legend: ReactNode; children: ReactNode }) {
  return (
    <section aria-label={title} className="min-w-0 rounded-md border border-rivet bg-panel px-4 py-3.5">
      <div className="mb-2.5 flex flex-wrap justify-between gap-x-3 gap-y-1.5">
        <span className="cap text-bone">{title}</span>
        <span className="font-mono text-[11px] text-fog">{legend}</span>
      </div>
      {children}
    </section>
  );
}

const WIDTH = 640;
const HEIGHT = 200;
const LEFT = 52;
const TOP = 8;
const BOTTOM = 172;

/**
 * A line over time, with an optional band, drawn as SVG. A gap in the
 * reports breaks the line instead of drawing across it. Without a band
 * the line gets a faint area and the top is marked as the limit.
 */
function Chart({
  windows,
  max,
  format,
  line,
  band,
  tone,
  label,
}: {
  windows: MetricsWindow[];
  max: number;
  format: (value: number) => string;
  line: (window: MetricsWindow) => number;
  band?: (window: MetricsWindow) => [number, number];
  tone: "heat" | "chick";
  label: string;
}) {
  const start = (windows[0]?.at ?? 0) - (windows[0]?.seconds ?? 0);
  const end = Math.max(windows[windows.length - 1]?.at ?? 0, start + 60);
  const x = (at: number) => LEFT + ((at - start) / (end - start)) * (WIDTH - LEFT);
  const y = (value: number) => BOTTOM - (Math.min(value, max) / max) * (BOTTOM - TOP);
  const runs = split(windows);
  const stroke = tone === "heat" ? "var(--heat)" : "var(--chick)";
  return (
    <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={label} className="block h-auto w-full">
      <g className="font-mono" fontSize="11" fill="var(--steel)">
        {[1, 0.5, 0].map((share) => (
          <text key={share} x={LEFT - 8} y={y(max * share) + 4} textAnchor="end">
            {share === 0 && !band ? "0" : format(max * share)}
          </text>
        ))}
        <text x={LEFT} y={HEIGHT - 4}>
          0 min
        </text>
        <text x={WIDTH - 4} y={HEIGHT - 4} textAnchor="end">
          {Math.round((end - start) / 60)} min
        </text>
      </g>
      <g stroke="var(--seam)">
        {(band ? [1, 0.5, 0] : [0.5, 0]).map((share) => (
          <line key={share} x1={LEFT} x2={WIDTH} y1={y(max * share)} y2={y(max * share)} />
        ))}
      </g>
      {!band && <line x1={LEFT} x2={WIDTH} y1={TOP} y2={TOP} stroke="var(--fog)" strokeDasharray="4 4" />}
      {runs.map((run) => {
        const points = run.map((w) => `${x(w.at)},${y(line(w))}`);
        const first = run[0];
        const last = run[run.length - 1];
        return (
          <g key={first?.at}>
            {band ? (
              <polygon
                fill="var(--ember)"
                opacity={0.7}
                points={[
                  ...run.map((w) => `${x(w.at)},${y(band(w)[1])}`),
                  ...[...run].reverse().map((w) => `${x(w.at)},${y(band(w)[0])}`),
                ].join(" ")}
              />
            ) : (
              first &&
              last && (
                <polygon
                  fill={stroke}
                  opacity={0.12}
                  points={[...points, `${x(last.at)},${BOTTOM}`, `${x(first.at)},${BOTTOM}`].join(" ")}
                />
              )
            )}
            <polyline
              points={points.join(" ")}
              fill="none"
              stroke={stroke}
              strokeWidth={2}
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          </g>
        );
      })}
    </svg>
  );
}

/** The windows in runs without a reporting gap. A single window is drawn as a short flat run. */
function split(windows: MetricsWindow[]): MetricsWindow[][] {
  const runs: MetricsWindow[][] = [];
  for (const window of windows) {
    const run = runs[runs.length - 1];
    const last = run?.[run.length - 1];
    if (run && last && window.at - last.at <= GAP_SECONDS) {
      run.push(window);
    } else {
      runs.push([{ ...window, at: window.at - window.seconds }, window]);
    }
  }
  return runs;
}

export function pct(value: number | null): string {
  return value === null ? "-" : `${Math.round(value)}%`;
}

export function bytes(value: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

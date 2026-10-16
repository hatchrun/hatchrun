// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { getJobMetrics, type JobMetrics, type MetricsSummary, type MetricsWindow } from "../api";
import { Await } from "../components/Status";
import { useLoad } from "../load";
import { Link } from "../router";

/** A gap this long between windows means the runner reported nothing for a while. */
const GAP_SECONDS = 30;

/** One job's CPU and memory, with a link to its logs on GitHub. */
export function JobPage({ org, id }: { org: string; id: string }) {
  const [metrics, reload] = useLoad(() => getJobMetrics(org, id), `${org}/${id}`);
  return (
    <section className="job-page">
      <p className="fine">
        <Link href={`/o/${org}`}>All jobs</Link>
      </p>
      <Await loaded={metrics} retry={reload}>
        {(data) => <JobMetricsView data={data} />}
      </Await>
    </section>
  );
}

function JobMetricsView({ data }: { data: JobMetrics }) {
  const { job, summary, windows } = data;
  return (
    <>
      <div className="page-head">
        <div>
          <p className="eyebrow">Job #{job.id}</p>
          <h1>{job.repository}</h1>
          <p className="muted">
            <code>{job.label}</code>
            {job.runnerName && <span className="mono small"> on {job.runnerName}</span>}
          </p>
        </div>
        <a className="button" href={data.githubUrl}>
          Logs on GitHub
        </a>
      </div>
      {summary && windows.length > 0 ? (
        <div className="dashboard">
          <Summary summary={summary} />
          <section className="card panel">
            <h2>CPU</h2>
            <p className="muted small">Busy share of all cores. The band spans the quietest and busiest second.</p>
            <Chart
              windows={windows}
              max={100}
              format={(value) => `${Math.round(value)}%`}
              line={(w) => w.cpuAvg}
              band={(w) => [w.cpuMin, w.cpuMax]}
              label={`CPU: mean ${pct(summary.cpuAvg)}, highest ${pct(summary.cpuMax)}`}
            />
          </section>
          <section className="card panel">
            <h2>Memory</h2>
            <p className="muted small">Highest use in each ten seconds, against the instance's memory.</p>
            <Chart
              windows={windows}
              max={Math.max(...windows.map((w) => w.memTotal), 1)}
              format={bytes}
              line={(w) => w.memMax}
              label={`Memory: highest ${bytes(summary.memMax ?? 0)} of ${bytes(summary.memTotal ?? 0)}`}
            />
          </section>
        </div>
      ) : (
        <div className="card empty">
          <p>No metrics for this job.</p>
          <p className="muted small">
            Runners report once a minute while the job runs, so a job that just started shows up shortly. A job
            that never got a runner has nothing to show. Its logs are on GitHub.
          </p>
        </div>
      )}
    </>
  );
}

function Summary({ summary }: { summary: MetricsSummary }) {
  const oom = (summary.oomKills ?? 0) > 0;
  return (
    <section className="stats stats-wide" aria-label="The job's resources">
      <Stat label="CPU" value={pct(summary.cpuAvg)} hint={`mean, highest ${pct(summary.cpuMax)}`} />
      <Stat
        label="Memory"
        value={bytes(summary.memMax ?? 0)}
        hint={`highest, of ${bytes(summary.memTotal ?? 0)}`}
      />
      <Stat
        label="Out of memory"
        value={String(Math.round(summary.oomKills ?? 0))}
        hint={oom ? "processes killed" : "nothing killed"}
        alarm={oom}
      />
      <Stat label="Disk" value={bytes(summary.diskUsed ?? 0)} hint="used at most" />
      <Stat label="Network" value={bytes(summary.netRx ?? 0)} hint={`in, ${bytes(summary.netTx ?? 0)} out`} />
      <Stat label="Steal" value={pct(summary.cpuSteal)} hint="CPU taken by other guests" />
    </section>
  );
}

function Stat({ label, value, hint, alarm }: { label: string; value: string; hint: string; alarm?: boolean }) {
  return (
    <div className={`card stat${alarm ? " stat-alarm" : ""}`}>
      <span className="stat-label">{label}</span>
      <span className="stat-value">{value}</span>
      <span className="stat-hint">{hint}</span>
    </div>
  );
}

const WIDTH = 640;
const HEIGHT = 180;
const LEFT = 52;
const BOTTOM = 22;

/**
 * A line over time, with an optional band, drawn as SVG. A gap in the
 * reports breaks the line instead of drawing across it.
 */
function Chart({
  windows,
  max,
  format,
  line,
  band,
  label,
}: {
  windows: MetricsWindow[];
  max: number;
  format: (value: number) => string;
  line: (window: MetricsWindow) => number;
  band?: (window: MetricsWindow) => [number, number];
  label: string;
}) {
  const start = (windows[0]?.at ?? 0) - (windows[0]?.seconds ?? 0);
  const end = Math.max(windows[windows.length - 1]?.at ?? 0, start + 60);
  const x = (at: number) => LEFT + ((at - start) / (end - start)) * (WIDTH - LEFT - 4);
  const y = (value: number) => 4 + (1 - Math.min(value, max) / max) * (HEIGHT - BOTTOM - 4);
  const runs = split(windows);
  return (
    <svg className="chart" viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-label={label}>
      {[0, 0.5, 1].map((share) => (
        <g key={share}>
          <line className="chart-grid" x1={LEFT} x2={WIDTH} y1={y(max * share)} y2={y(max * share)} />
          <text className="chart-axis" x={LEFT - 6} y={y(max * share) + 4} textAnchor="end">
            {format(max * share)}
          </text>
        </g>
      ))}
      {band &&
        runs.map((run) => (
          <polygon
            key={`band-${run[0]?.at}`}
            className="chart-band"
            points={[
              ...run.map((w) => `${x(w.at)},${y(band(w)[1])}`),
              ...[...run].reverse().map((w) => `${x(w.at)},${y(band(w)[0])}`),
            ].join(" ")}
          />
        ))}
      {runs.map((run) => (
        <polyline
          key={`line-${run[0]?.at}`}
          className="chart-line"
          points={run.map((w) => `${x(w.at)},${y(line(w))}`).join(" ")}
        />
      ))}
      <text className="chart-axis" x={LEFT} y={HEIGHT - 4}>
        0 min
      </text>
      <text className="chart-axis" x={WIDTH} y={HEIGHT - 4} textAnchor="end">
        {Math.round((end - start) / 60)} min
      </text>
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

function pct(value: number | null): string {
  return value === null ? "-" : `${Math.round(value)}%`;
}

function bytes(value: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

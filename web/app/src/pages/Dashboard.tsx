// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";

import { listJobs, type Job, type JobPage, type Stack, type Tenant } from "../api";
import { CopyButton } from "../components/CopyButton";
import { LoadError, Spinner } from "../components/Status";
import { Link, useQuery } from "../router";

/** Seconds between refreshes of the job list. */
const REFRESH = 10;

export function Dashboard({ tenant }: { tenant: Tenant }) {
  const org = tenant.org.login;
  const installed = useQuery("installed");
  const [page, setPage] = useState<JobPage | null>(null);
  const [older, setOlder] = useState<Job[]>([]);
  const [error, setError] = useState<Error | null>(null);
  const [more, setMore] = useState(true);

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

  const jobs = page ? [...page.jobs, ...older.filter((job) => !page.jobs.some((seen) => seen.id === job.id))] : [];
  const loadOlder = async () => {
    const last = jobs[jobs.length - 1];
    if (!last) {
      return;
    }
    const next = await listJobs(org, last.queuedAt);
    setOlder((was) => [...was, ...next.jobs]);
    setMore(next.jobs.length === (next.pageSize ?? 50));
  };

  return (
    <div className="dashboard">
      {installed && (
        <p className="notice notice-success" role="status">
          The hatchrun App is installed. {org} is ready to run jobs.
        </p>
      )}
      {tenant.aws && <StackUpdate stack={tenant.aws.stack} region={tenant.aws.region} />}
      <div className="dashboard-grid">
        <section className="card panel">
          <h2>Run a job on hatchrun</h2>
          <p className="muted">
            Set <code>runs-on</code> to one runner profile. Each job gets a fresh instance in{" "}
            {tenant.aws?.region ?? "your AWS account"}.
          </p>
          <Snippet />
          <dl className="options">
            <div>
              <dt>
                <code>hatchrun-2cpu</code> to <code>hatchrun-16cpu</code>
              </dt>
              <dd>Built-in profiles, 4 GiB per vCPU, add <code>-arm64</code> for Graviton</dd>
            </div>
            <div>
              <dt>
                <code>/disk=N</code>, <code>/spot=true</code>
              </dt>
              <dd>Change one job's root volume or market</dd>
            </div>
            <div>
              <dt>
                <code>/cpu=N</code>, <code>/ram=N</code>
              </dt>
              <dd>At least N vCPUs, N GiB of memory</dd>
            </div>
          </dl>
          <p className="fine">
            <Link href={`/o/${org}/runners`}>Every profile, with its instance type and price</Link>
          </p>
        </section>
        <section className="stats" aria-label="Jobs now">
          <Stat label="Running" value={page?.counts.active} hint={`of ${tenant.maxRunners} at most`} />
          <Stat label="Waiting" value={page?.counts.waiting} hint="for a runner" />
          <Stat label="Finished" value={page?.counts.completedToday} hint="last 24 hours" />
          <Stat
            label="Failed launches"
            value={page?.counts.failedToday}
            hint="last 24 hours"
            alarm={(page?.counts.failedToday ?? 0) > 0}
          />
        </section>
      </div>

      <section className="card">
        <div className="panel-head">
          <h2>Recent jobs</h2>
          <span className="muted small">Refreshes every {REFRESH} seconds</span>
        </div>
        {error && <LoadError error={error} />}
        {!page && !error && <Spinner />}
        {page && jobs.length === 0 && (
          <div className="empty">
            <p className="muted">
              No jobs yet. Push a workflow that runs on <code>hatchrun</code> and it shows up here.
            </p>
          </div>
        )}
        {jobs.length > 0 && (
          <>
            <div className="table-wrap">
              <table className="jobs">
                <thead>
                  <tr>
                    <th scope="col">Job</th>
                    <th scope="col">Label</th>
                    <th scope="col">Status</th>
                    <th scope="col">Instance</th>
                    <th scope="col">Queued</th>
                    <th scope="col">Took</th>
                  </tr>
                </thead>
                <tbody>
                  {jobs.map((job) => (
                    <JobRow key={job.id} job={job} region={page?.region ?? null} />
                  ))}
                </tbody>
              </table>
            </div>
            {more && jobs.length >= (page?.pageSize ?? 50) && (
              <div className="panel-foot">
                <button type="button" className="button button-small" onClick={loadOlder}>
                  Load older jobs
                </button>
              </div>
            )}
          </>
        )}
      </section>
    </div>
  );
}

/** Asks the owner to update an older connect stack, and says how. */
function StackUpdate({ stack, region }: { stack: Stack; region: string }) {
  if (stack.changes.length === 0 && !stack.familyList) {
    return null;
  }
  const outdated = stack.changes.length > 0;
  return (
    <section className="notice notice-accent stack-update" aria-labelledby="stack-update-title">
      <h2 id="stack-update-title" className="stack-update-title">
        {outdated ? "Connect stack update available" : "Your connect stack still names instance families"}
      </h2>
      {outdated ? (
        <>
          <p>
            Your <code>hatchrun</code> stack in {region} is version {stack.version}; the latest is {stack.latest}.
            Until you update it, runners launch the way they did before.
          </p>
          <ul className="stack-changes">
            {stack.changes.map((change) => (
              <li key={change}>{change}</li>
            ))}
          </ul>
        </>
      ) : (
        <p>
          It allows only the families it lists, so instance types AWS adds stay off until you change
          InstanceTypes. The current default allows any size up to 8xlarge instead.
        </p>
      )}
      <ol className="stack-steps">
        <li>
          Open the <a href={stack.consoleUrl}>hatchrun stack in the CloudFormation console</a> and choose
          Update, then Replace existing template.
        </li>
        <li>
          Choose Amazon S3 URL and paste{" "}
          {stack.templateUrl ? <code className="break">{stack.templateUrl}</code> : "the template URL"}
          {stack.templateUrl && <CopyButton text={stack.templateUrl} label="Copy URL" />}
        </li>
        <li>
          Keep the parameters as they are
          {stack.familyList && (
            <>
              , except InstanceTypes: set it to <code>*.medium,*.large,*.xlarge,*.2xlarge,*.4xlarge,*.8xlarge</code>{" "}
              to stop naming families
            </>
          )}
          . Acknowledge that the stack changes IAM resources and submit.
        </li>
      </ol>
      {stack.updateCommand && (
        <details className="stack-cli">
          <summary>Or with the AWS CLI</summary>
          <div className="snippet">
            <pre>
              <code>{stack.updateCommand}</code>
            </pre>
            <CopyButton text={stack.updateCommand} />
          </div>
        </details>
      )}
      <p className="fine">
        hatchrun reads the stack again once a day, so the update shows here by tomorrow, or at once
        when you verify the role again under Settings.
      </p>
    </section>
  );
}

function Snippet() {
  const text = "runs-on: hatchrun-4cpu-arm64";
  return (
    <div className="snippet">
      <pre>
        <code>
          <span className="muted">jobs:{"\n"}  build:{"\n"}    </span>
          {text}
        </code>
      </pre>
      <CopyButton text={text} />
    </div>
  );
}

function Stat({ label, value, hint, alarm }: { label: string; value: number | undefined; hint: string; alarm?: boolean }) {
  return (
    <div className={`card stat${alarm ? " stat-alarm" : ""}`}>
      <span className="stat-label">{label}</span>
      <span className="stat-value">{value ?? "-"}</span>
      <span className="stat-hint">{hint}</span>
    </div>
  );
}

const STATUS: Record<Job["status"], string> = {
  queued: "Queued",
  in_progress: "Running",
  completed: "Done",
};

function state(job: Job): { text: string; tone: string } {
  if (job.launchState === "failed" && job.status === "queued") {
    if (job.launchRetry === "backoff") {
      return { text: "Retrying", tone: "accent" };
    }
    if (job.launchRetry === "setup") {
      return { text: "Needs setup", tone: "danger" };
    }
  }
  if (job.launchState === "failed") {
    return { text: "Launch failed", tone: "danger" };
  }
  if (job.status === "queued") {
    return { text: job.launchState === "launching" ? "Launching" : job.launchState === "launched" ? "Starting" : "Waiting", tone: "accent" };
  }
  return { text: STATUS[job.status], tone: job.status === "completed" ? "" : "success" };
}

function JobRow({ job, region }: { job: Job; region: string | null }) {
  const shown = state(job);
  return (
    <tr>
      <td>
        <a href={`https://github.com/${job.repository}/actions/runs/${job.runId}`} className="job-link">
          {job.repository}
        </a>
        <span className="muted small"> #{job.id}</span>
      </td>
      <td>
        <code>{job.label}</code>
      </td>
      <td>
        <span className={`badge${shown.tone ? ` badge-${shown.tone}` : ""}`}>{shown.text}</span>
        <LaunchProblem job={job} />
      </td>
      <td>
        {job.instanceId ? (
          region ? (
            <a
              className="mono small"
              href={`https://${region}.console.aws.amazon.com/ec2/home?region=${region}#InstanceDetails:instanceId=${job.instanceId}`}
            >
              {job.instanceId}
            </a>
          ) : (
            <span className="mono small">{job.instanceId}</span>
          )
        ) : (
          <span className="muted">-</span>
        )}
      </td>
      <td className="nowrap" title={new Date(job.queuedAt).toLocaleString()}>
        {ago(job.queuedAt)}
      </td>
      <td className="nowrap">{job.status === "completed" ? span(job.updatedAt - job.queuedAt) : <span className="muted">-</span>}</td>
    </tr>
  );
}

/** What went wrong with the job's launch and what happens next, in plain words. */
function LaunchProblem({ job }: { job: Job }) {
  if (!job.launchError) {
    return null;
  }
  const attempt = job.launchAttempts > 0 ? `Attempt ${job.launchAttempts} failed: ` : "";
  if (job.status !== "queued" || job.launchState === "launched") {
    return <div className="job-error">{job.launchError}</div>;
  }
  if (job.launchState !== "failed") {
    return (
      <div className="job-note">
        Trying again after a failed launch: {job.launchError}
      </div>
    );
  }
  if (job.launchRetry === "backoff") {
    return (
      <>
        <div className="job-error">
          {attempt}
          {job.launchError}
        </div>
        <div className="job-note">{nextTry(job.nextLaunchAt)}</div>
      </>
    );
  }
  if (job.launchRetry === "setup") {
    return (
      <>
        <div className="job-error">{job.launchError}</div>
        <div className="job-note">
          This will fail the same way until the setup changes. It launches again once you verify the AWS role, edit a
          runner profile, change the settings or get a licence.
        </div>
      </>
    );
  }
  return <div className="job-error">{job.launchError}</div>;
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

function ago(ms: number): string {
  const seconds = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 48) {
    return `${hours} h ago`;
  }
  return new Date(ms).toLocaleDateString();
}

function span(ms: number): string {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) {
    return `${seconds} s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes} min ${seconds % 60} s`;
  }
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

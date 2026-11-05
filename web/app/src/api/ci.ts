// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// /api/ci: runs, logs, secrets, environments and repositories of the
// orchestrator. Shapes follow crates/ci/src/api.rs. Times are Unix
// milliseconds. List routes answer `Page<T>`.

import { adminOnly, ApiError, del, failure, get, patch, post, put, queryString, retrying, type Page, type Paging } from "./core";

const orgPath = (org: string) => `/api/ci/orgs/${encodeURIComponent(org)}`;
const repoPath = (owner: string, repo: string) => `/api/ci/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}`;

/** What a run's jobs add up to. */
export interface CiJobCounts {
  total: number;
  passed: number;
  failed: number;
  running: number;
  waiting: number;
  cancelled: number;
  skipped: number;
}

/** A run of the orchestrator. */
export interface CiRun {
  id: number;
  run_number: number;
  run_attempt: number;
  /** owner/name. */
  repository: string;
  workflow: string;
  event: string;
  /** The full ref, such as refs/heads/main. */
  ref: string;
  /** The branch the run is on; null for a tag or a ref without one. */
  branch: string | null;
  sha: string;
  actor: string;
  /** The head commit's subject, at most 200 characters; null when the trigger carried none. */
  message: string | null;
  status: string;
  conclusion: string | null;
  created_at: number;
  completed_at: number | null;
  /** Null while the run goes. */
  duration_ms: number | null;
  jobs: CiJobCounts;
}

/** A step of a job. A running job's steps have a summary once they end. */
export interface CiStep {
  index?: number;
  name: string;
  summary?: { conclusion: string; duration_ms: number } | null;
  /** A finished job's steps are stored as summaries. */
  conclusion?: string;
  duration_ms?: number;
}

export interface CiJob {
  id: number;
  plan_id: string;
  /** The `plan_id`s of the jobs this one waits on. */
  needs: string[];
  name: string;
  status: string;
  conclusion: string | null;
  attempt: number;
  started_at: number | null;
  completed_at: number | null;
  steps: CiStep[];
  has_log: boolean;
  /** "approval" while the job waits for an organization admin to approve or reject it. */
  held: string | null;
  /** The environment the job deploys to, when it has one. */
  environment: string | null;
  /** The admin's decision on a job held for approval; null until one is made. */
  approval: { decision: "approved" | "rejected"; by: string; at: number } | null;
}

export interface CiRunDetail {
  /** What the viewer may do with the repository. */
  access: "read" | "write" | "admin";
  run: CiRun;
  jobs: CiJob[];
}

export type CiRunStatus = "queued" | "running" | "waiting" | "success" | "failure" | "cancelled";

/** The filters of the run lists. Every one is optional. */
export interface RunFilters {
  status?: CiRunStatus[];
  /** `owner/name`; org list only. */
  repo?: string[];
  branch?: string;
  /** A login, or "me" for the viewer. */
  actor?: string;
  /** Text the run's workflow, title, branch or sha contains. */
  q?: string;
  /** Unix ms. The server defaults `since` to seven days ago. */
  since?: number;
  until?: number;
}

const runQuery = ({ status, repo, branch, actor, q, since, until, cursor, limit }: RunFilters & Paging) =>
  queryString({ status, repo, branch, actor, q, since, until, cursor, limit });

/** The runs of every repository of an organization the viewer can read, newest first. */
/** Runs per Runs-page tab under the same filters, status aside. */
export interface RunCounts {
  all: number;
  running: number;
  waiting: number;
  failed: number;
  passed: number;
}

export const listOrgRuns = (org: string, filters: RunFilters & Paging = {}) =>
  retrying(() => get<Page<CiRun> & { counts: RunCounts }>(`${orgPath(org)}/runs${runQuery(filters)}`));

/** A repository's runs, newest first. `repo` is not a filter here. */
export const listRepoRuns = (owner: string, repo: string, filters: Omit<RunFilters, "repo"> & Paging = {}) =>
  retrying(() => get<Page<CiRun>>(`${repoPath(owner, repo)}/runs${runQuery(filters)}`));

/** A job held for an approval the viewer can give. `since` is when it began to wait. */
export interface CiApproval {
  run: CiRun;
  job: { id: number; name: string; environment: string | null };
  since: number;
}

/** A repository whose latest finished run on the default branch failed. */
export interface CiFailing {
  repository: string;
  branch: string;
  run: CiRun;
}

export interface NeedsYou {
  approvals: CiApproval[];
  failing: CiFailing[];
}

export const getNeedsYou = (org: string) => retrying(() => get<NeedsYou>(`${orgPath(org)}/needs-you`));

export const getCiRun = (id: string | number) => retrying(() => get<CiRunDetail>(`/api/ci/runs/${id}`));

/** An artifact a run's jobs uploaded. */
export interface CiArtifact {
  id: number;
  name: string;
  /** Bytes. */
  size: number;
  created_at: number;
  /** When the run's retention deletes it. */
  expires_at: number;
}

/** The run's finished artifacts, oldest first. */
export const listCiArtifacts = (run: string | number) =>
  retrying(() => get<{ artifacts: CiArtifact[] }>(`/api/ci/runs/${run}/artifacts`));

/** Where an artifact downloads from: the API answers with a redirect to a short-lived link. */
export const ciArtifactUrl = (run: string | number, artifact: number) => `/api/ci/runs/${run}/artifacts/${artifact}`;

export const cancelCiRun = (id: number) => post<undefined>(`/api/ci/runs/${id}/cancel`);

/** Lets a job held for approval run. Answers 409 when the job is not waiting. */
export const approveCiJob = (id: number) => post<undefined>(`/api/ci/jobs/${id}/approve`);

/** Fails a job held for approval. Answers 409 when the job is not waiting. */
export const rejectCiJob = (id: number) => post<undefined>(`/api/ci/jobs/${id}/reject`);

/** A line of a log the agent or the viewer marks as an error. */
export interface LogError {
  /** Byte offset of the line. */
  offset: number;
  /** 0-based line number. */
  line: number;
  step: number;
  /** At most 300 characters. */
  text: string;
}

/** A step of a finished log: where it starts, in bytes and in lines. */
export interface LogStep {
  index: number;
  name: string;
  offset: number;
  /** First line, 0-based. */
  line: number;
  lines: number;
}

/** The map of a finished log. */
export interface LogIndex {
  bytes: number;
  lines: number;
  steps: LogStep[];
  dropped_lines: number;
  /** At most 200. */
  errors: LogError[];
  first_error: LogError | null;
}

/** The job's log: the stored one once it ended, else the newest text the run holds. */
export interface CiLog {
  text: string;
  /** Where the text starts in the whole log; null when the answer is a finished, stored log. */
  offset: number | null;
}

/** A byte range of a stored log. */
export interface CiLogRange {
  text: string;
  offset: number;
  /** The whole log's size in bytes. */
  bytes: number;
}

/** The job's log. For a stored one it is the whole log; use `getCiLogRange` to read part of it. */
export const getCiLog = (job: string | number) =>
  retrying(async (): Promise<CiLog> => {
    const response = await fetch(`/api/ci/jobs/${job}/log`, { credentials: "same-origin" });
    if (!response.ok) {
      throw await failure(response);
    }
    const offset = response.headers.get("x-log-offset");
    return { text: await response.text(), offset: offset === null ? null : Number(offset) };
  });

/** At most this many bytes in one `getCiLogRange` call. */
export const LOG_RANGE_MAX = 1024 * 1024;

/** `length` bytes of a stored log from `offset`; `length` is at most `LOG_RANGE_MAX`. */
export const getCiLogRange = (job: string | number, offset: number, length: number) =>
  retrying(async (): Promise<CiLogRange> => {
    const response = await fetch(`/api/ci/jobs/${job}/log${queryString({ offset, length })}`, { credentials: "same-origin" });
    if (!response.ok) {
      throw await failure(response);
    }
    return {
      text: await response.text(),
      offset: Number(response.headers.get("x-log-offset") ?? offset),
      bytes: Number(response.headers.get("x-log-bytes") ?? 0),
    };
  });

/** The index of a finished log; null when there is none (the job runs, or the log is not stored yet). */
export const getCiLogIndex = async (job: string | number): Promise<LogIndex | null> => {
  try {
    return await retrying(() => get<LogIndex>(`/api/ci/jobs/${job}/log/index`));
  } catch (err) {
    if (err instanceof ApiError && err.status === 404) {
      return null;
    }
    throw err;
  }
};

export interface LogMatch {
  offset: number;
  line: number;
  step: number;
}

export interface LogSearch {
  matches: LogMatch[];
  total: number;
  /** More than 1000 matches: the list stops there. */
  truncated: boolean;
}

/** Case-insensitive substring search of a stored log, at most 1000 matches a page. */
export const searchCiLog = (job: string | number, q: string, cursor?: string | null) =>
  retrying(() => get<LogSearch>(`/api/ci/jobs/${job}/log/search${queryString({ q, cursor })}`));

export interface DispatchRequest {
  workflow: string;
  /** A branch, a tag or a full ref; empty for the default branch. */
  ref: string;
  inputs: Record<string, string | boolean | number>;
}

/** Starts a workflow by hand; answers the ids of the runs it started. */
export const dispatchWorkflow = (owner: string, repo: string, body: DispatchRequest) =>
  post<{ runs: number[] }>(`${repoPath(owner, repo)}/dispatch`, body);

/** Runs started per bucket of the activity chart. */
export interface ActivityBucket {
  /** Start of the bucket, Unix ms. */
  at: number;
  started: number;
  passed: number;
  failed: number;
}

export interface Activity {
  /** 3,600,000 for "24h" (24 buckets), 21,600,000 for "7d" (28 buckets). */
  bucket_ms: number;
  buckets: ActivityBucket[];
  /** Jobs running and queued now, and the most that may run at once. */
  concurrency: { running: number; queued: number; limit: number };
}

export type ActivityRange = "24h" | "7d";

export const getActivity = (org: string, range: ActivityRange, repo?: string[]) =>
  retrying(() => get<Activity>(`${orgPath(org)}/activity${queryString({ range, repo })}`));

/** The card on file at Polar. */
export interface CiCard {
  brand: string;
  last4: string;
  exp_month: number;
  exp_year: number;
}

/** One day of the month's spend. */
export interface CiDay {
  /** UTC date, YYYY-MM-DD. */
  day: string;
  seconds: number;
  cents: number;
}

/** A Polar order, newest first, at most 12. */
export interface CiInvoice {
  id: string;
  /** RFC 3339, as Polar answers. */
  date: string;
  amount_cents: number;
  status: string;
  /** Null until Polar has generated the invoice. */
  url: string | null;
}

/** What /api/ci/orgs/:org/billing answers. */
export interface CiBilling {
  /** The card on file; null when there is none. */
  card: CiCard | null;
  /** This month, one entry per UTC day up to today. */
  daily: CiDay[];
  invoices: CiInvoice[];
  /** Billed runner seconds this month, every attempt at least 60. */
  seconds: number;
  /** Billing units this month; one unit is one 2 vCPU second. */
  units: number;
  /** This month's cost, in cents. */
  spendCents: number;
  /** The monthly spend cap, in whole dollars. */
  capDollars: number;
  /** The range the cap can be set within, in whole dollars. */
  minCapDollars: number;
  maxCapDollars: number;
  running: number;
  maxRunning: number;
}

export const getCiBilling = (org: string) => adminOnly(() => get<CiBilling>(`${orgPath(org)}/billing`));
export const setCiCap = (org: string, capDollars: number) =>
  adminOnly(() => put<undefined>(`${orgPath(org)}/billing/cap`, { capDollars }));

/** A repository of the organization's App installation. */
export interface CiOrgRepo {
  /** owner/name. */
  name: string;
  default_branch: string;
  /** Whether the repository starts runs. */
  enabled: boolean;
  last_run: { id: number; run_number: number; status: string; conclusion: string | null; created_at: number } | null;
}

export interface OrgRepoQuery extends Paging {
  q?: string;
  enabled?: boolean;
}

/** Every repository of the installation, not only those that ran. Last run newest first, then by name. */
export const listOrgRepos = (org: string, { q, enabled, cursor, limit }: OrgRepoQuery = {}) =>
  retrying(() => get<Page<CiOrgRepo>>(`${orgPath(org)}/repos${queryString({ q, enabled, cursor, limit })}`));

/**
 * Changes which repositories start runs (org admin). `all` turns CI on
 * for every repository or off for the ones not added; `add` and
 * `remove` are `owner/name` lists that leave the rest as they are.
 */
export const patchEnabledRepos = (org: string, body: { all?: boolean; add?: string[]; remove?: string[] }) =>
  adminOnly(() => patch<undefined>(`${orgPath(org)}/repos/enabled`, body));

export interface OrgSetup {
  app: { installed: boolean; repos: number; manage_url: string };
  enabled: { all: boolean; count: number };
  first_run: { id: number; repository: string } | null;
  billing: { card: boolean; cap_dollars: number };
}

export const getOrgSetup = (org: string) => get<OrgSetup>(`${orgPath(org)}/setup`);

/** Whose secrets and variables: an organization's, a repository's, or one of its environments'. */
export type CiScope = { org: string } | { owner: string; repo: string } | { owner: string; repo: string; environment: string };

export type CiItemKind = "secrets" | "vars";

const scopePath = (scope: CiScope) =>
  "org" in scope
    ? orgPath(scope.org)
    : "environment" in scope
      ? `${repoPath(scope.owner, scope.repo)}/environments/${encodeURIComponent(scope.environment)}`
      : repoPath(scope.owner, scope.repo);

/**
 * A secret as listed: never its value. `repositories` is the
 * `owner/repo` names an organization item reaches, or null for all of
 * them; other scopes always answer null.
 */
export interface CiSecret {
  name: string;
  updatedAt: number;
  repositories: string[] | null;
  /** Where the item lives. A list also holds items inherited from a wider scope. */
  scope: "org" | "repo" | "env";
  /** An inherited item that a narrower one of the same name hides. */
  shadowed: boolean;
  /** When a job last received it, Unix ms; null if none did. */
  last_used_at: number | null;
}

export interface CiVar extends CiSecret {
  value: string;
}

export interface ItemQuery extends Paging {
  /** Text the name contains. */
  q?: string;
}

const itemQuery = ({ q, cursor, limit }: ItemQuery) => queryString({ q, cursor, limit });

export const listCiSecrets = (scope: CiScope, query: ItemQuery = {}) =>
  adminOnly(() => get<Page<CiSecret>>(`${scopePath(scope)}/secrets${itemQuery(query)}`));
export const listCiVars = (scope: CiScope, query: ItemQuery = {}) =>
  adminOnly(() => get<Page<CiVar>>(`${scopePath(scope)}/vars${itemQuery(query)}`));

/**
 * Sets or replaces one secret or variable; the API uppercases the
 * name. Org scope sends `repositories` (null for all); other scopes
 * never send it.
 */
export const putCiItem = (scope: CiScope, kind: CiItemKind, name: string, value: string, repositories?: string[] | null) =>
  adminOnly(() =>
    put<undefined>(
      `${scopePath(scope)}/${kind}/${encodeURIComponent(name)}`,
      "org" in scope ? { value, repositories: repositories ?? null } : { value },
    ),
  );

export const deleteCiItem = (scope: CiScope, kind: CiItemKind, name: string) =>
  adminOnly(() => del<undefined>(`${scopePath(scope)}/${kind}/${encodeURIComponent(name)}`));

/** A deployment environment of a repository. No branches means any branch. */
export interface CiEnvironment {
  name: string;
  branches: string[];
  approval: boolean;
  updatedAt: number;
}

/** An environment in the org-wide list, with how many secrets and variables it holds. */
export interface CiOrgEnvironment extends CiEnvironment {
  /** owner/name. */
  repository: string;
  secrets: number;
  vars: number;
}

export interface OrgEnvironmentQuery extends Paging {
  q?: string;
  /** owner/name. */
  repo?: string;
}

export const listOrgEnvironments = (org: string, { q, repo, cursor, limit }: OrgEnvironmentQuery = {}) =>
  adminOnly(() => get<Page<CiOrgEnvironment>>(`${orgPath(org)}/environments${queryString({ q, repo, cursor, limit })}`));

const environmentsPath = (owner: string, repo: string) => `${repoPath(owner, repo)}/environments`;

export const listCiEnvironments = (owner: string, repo: string) =>
  adminOnly(() => get<CiEnvironment[]>(environmentsPath(owner, repo)));

export const putCiEnvironment = (owner: string, repo: string, name: string, body: { branches: string[]; approval: boolean }) =>
  adminOnly(() => put<CiEnvironment>(`${environmentsPath(owner, repo)}/${encodeURIComponent(name)}`, body));

export const deleteCiEnvironment = (owner: string, repo: string, name: string) =>
  adminOnly(() => del<undefined>(`${environmentsPath(owner, repo)}/${encodeURIComponent(name)}`));

/** What an audit entry did. */
export type AuditAction = "secret.put" | "secret.delete" | "var.put" | "var.delete" | "environment.put" | "environment.delete" | "repos.enabled";

export interface AuditEntry {
  id: number;
  /** Unix ms. */
  at: number;
  /** GitHub login. */
  actor: string;
  action: AuditAction;
  /** "org", "owner/repo" or "owner/repo/environment". */
  scope: string;
  /** The secret, variable, environment or repository the action touched. */
  target: string;
}

/** The org's audit log, newest first (org admin). */
export const listAudit = (org: string, { repo, cursor, limit }: { repo?: string } & Paging = {}) =>
  adminOnly(() => get<Page<AuditEntry>>(`${orgPath(org)}/audit${queryString({ repo, cursor, limit })}`));

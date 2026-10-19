// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// Calls to the Worker's /api. Every request that changes something
// carries the session's CSRF token, which /api/session hands out.

export interface User {
  id: number;
  login: string;
  name: string | null;
  avatarUrl: string;
}

export interface Session {
  user: User;
  csrfToken: string;
}

/** An installation of the App on an organization the user can see. */
export interface InstallationChoice {
  id: number;
  login: string;
  avatarUrl: string;
  suspended: boolean;
  /** The organization's login when the user is already in its tenant. */
  tenant: string | null;
}

export interface Installations {
  /** Installs the App on another account, tied to this session. */
  installUrl: string;
  installations: InstallationChoice[];
}

export interface TenantSummary {
  id: string;
  login: string;
  avatarUrl: string;
}

export interface Aws {
  roleArn: string;
  region: string;
  launchTemplateId: string;
  subnetIds: string[];
  stack: Stack;
}

/** The connect stack's version and how to update it. */
export interface Stack {
  /** 1 for a stack from before the version tag. */
  version: number;
  latest: number;
  /** Unix milliseconds. */
  checkedAt: number | null;
  /** What newer versions bring, oldest first; empty when current. */
  changes: string[];
  /** Whether instance types are still limited by the old family list. */
  familyList: boolean;
  consoleUrl: string;
  templateUrl: string | null;
  updateCommand: string | null;
}

export interface Billing {
  beta: boolean;
  /** In the beta or paying, so there is nothing to set up. */
  licensed: boolean;
  subscription: null | {
    status: string;
    currentPeriodEnd: string | null;
    cancelAtPeriodEnd: boolean;
  };
  usage: Usage;
}

/** Job minutes this calendar month (UTC). */
export interface Usage {
  monthStart: string;
  monthEnd: string;
  minutes: number;
  freeMinutes: number;
  /** When the free minutes ran out this month. */
  freeUsedUpAt: string | null;
  /** When runners stop launching without a card. */
  graceEndsAt: string | null;
  /** Whether runners launch now. */
  mayLaunch: boolean;
}

/** The metered Polar product. */
export interface Plan {
  id: string;
  name: string;
  description: string | null;
  /** Smallest currency unit, such as cents. */
  amount: number | null;
  currency: string | null;
  interval: string | null;
  /** Price per minute in the smallest currency unit, a decimal string such as "0.1". */
  unitAmount: string | null;
  /** Most a billing period costs, in the smallest currency unit. */
  capAmount: number | null;
}

export interface Plans {
  /** Shortest billing interval first. */
  plans: Plan[];
}

export interface Steps {
  app: boolean;
  aws: boolean;
}

export interface Tenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  aws: Aws | null;
  maxRunners: number;
  /** null for every region. */
  allowedRegions: string[] | null;
  billing: Billing;
  steps: Steps;
  installUrl: string;
}

export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message.charAt(0).toUpperCase() + message.slice(1));
    this.status = status;
  }
}

let csrfToken = "";

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (method !== "GET") {
    headers["X-CSRF-Token"] = csrfToken;
  }
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const answer = (await response.json()) as { error?: string };
      if (answer.error) {
        message = answer.error;
      }
    } catch {
      // Not JSON: keep the status line.
    }
    throw new ApiError(response.status, message);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

export const get = <T>(path: string) => request<T>("GET", path);
export const post = <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {});
export const put = <T>(path: string, body?: unknown) => request<T>("PUT", path, body ?? {});

/** The signed-in session, or null when nobody is signed in. */
export async function getSession(): Promise<Session | null> {
  try {
    const session = await get<Session>("/api/session");
    csrfToken = session.csrfToken;
    return session;
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) {
      return null;
    }
    throw err;
  }
}

export function signInUrl(returnTo: string): string {
  return `/api/auth/login?return_to=${encodeURIComponent(returnTo)}`;
}

export const signOut = () => post<undefined>("/api/auth/logout");

export const listInstallations = () => get<Installations>("/api/installations");
export const listTenants = () => get<TenantSummary[]>("/api/tenants");
export const getTenant = (org: string) => get<Tenant>(`/api/tenants/${encodeURIComponent(org)}`);
export const createTenant = (installationId: number) => post<Tenant>("/api/tenants", { installationId });

const tenantPath = (org: string) => `/api/tenants/${encodeURIComponent(org)}`;
export const getPlan = () => get<Plans>("/api/plan");
export const startCheckout = (org: string) => post<{ url: string }>(`${tenantPath(org)}/checkout`);
export const openPortal = (org: string) => post<{ url: string }>(`${tenantPath(org)}/portal`);
export const redeemInvite = (org: string, code: string) => post<Tenant>(`${tenantPath(org)}/beta`, { code });

export interface AwsRegion {
  id: string;
  name: string;
  hasImage: boolean;
  allowed: boolean;
  launchUrl: string | null;
}

export interface AwsInstructions {
  tenantId: string;
  /** AWS Marketplace pages that subscribe the account to the runner images. */
  marketplace: { amd64: string; arm64: string };
  templateUrl: string | null;
  regions: AwsRegion[];
}

export const getAwsInstructions = (org: string) => get<AwsInstructions>(`${tenantPath(org)}/aws`);
export const connectAws = (org: string, region: string, roleArn: string) =>
  post<Tenant>(`${tenantPath(org)}/aws`, { region, roleArn });

export interface Job {
  id: number;
  runId: number;
  /** The job's name as the run shows it, such as "Save (host)". */
  name: string;
  /** The run attempt the job belongs to, from 1. */
  runAttempt: number;
  label: string;
  status: "queued" | "in_progress" | "completed";
  /** GitHub's, once completed: success, failure, cancelled, skipped, timed_out, neutral, action_required. */
  conclusion: string | null;
  launchState: "launching" | "launched" | "failed" | null;
  runnerName: string | null;
  instanceId: string | null;
  launchError: string | null;
  /** Launches started for the job. */
  launchAttempts: number;
  /**
   * After a failed launch: "backoff" when it launches again at nextLaunchAt,
   * "setup" when it waits for the organization's setup to change.
   */
  launchRetry: "backoff" | "setup" | null;
  /** Unix milliseconds. */
  nextLaunchAt: number | null;
  /** Unix milliseconds: when hatchrun heard of the job. */
  queuedAt: number;
  /** Unix milliseconds, GitHub's: when a runner took the job, and when it finished. */
  startedAt: number | null;
  completedAt: number | null;
  updatedAt: number;
}

/** A workflow run with at least one hatchrun job. */
export interface Run {
  id: number;
  repository: string;
  workflowName: string | null;
  headBranch: string | null;
  headSha: string;
  /** The newest attempt. */
  attempt: number;
  /** Unix milliseconds: when the newest attempt's first job was queued. */
  queuedAt: number;
  /** Unix milliseconds: when its last current job ended; null while one has not. */
  endedAt: number | null;
  /** Null until hatchrun has read the run on GitHub. */
  runNumber: number | null;
  event: string | null;
  displayTitle: string | null;
  actor: string | null;
  path: string | null;
  githubUrl: string;
}

/** A run in the list, with its current jobs: the newest attempt of each job name. */
export interface ListedRun extends Run {
  jobs: Job[];
  /** Jobs of earlier attempts that later ones replaced. */
  olderJobs: number;
}

export interface JobCounts {
  waiting: number;
  active: number;
  /** Of the active ones, those GitHub has handed a job; the rest are booting. */
  running?: number;
  completedToday: number;
  failedToday: number;
  /** Runs with a job in each group, as the status tabs list them. */
  waitingRuns: number;
  activeRuns: number;
  failedRunsToday: number;
}

/** Finished and failed jobs per bucket over the last day, oldest first. */
export interface JobActivity {
  /** Unix milliseconds: the start of the first bucket. */
  since: number;
  bucketMs: number;
  finished: number[];
  failed: number[];
}

export interface RunPage {
  runs: ListedRun[];
  counts: JobCounts;
  region: string | null;
  pageSize?: number;
  /** On the first page only. */
  activity?: JobActivity;
}

/** A status tab of the runs table: the runs with a current job in the group. */
export type JobGroup = "active" | "waiting" | "failed";

/**
 * The columns the runs table sorts by. By repository, each repository's
 * runs are newest first. By took, the runs still going come after the
 * finished ones, oldest first either way. A run's state is left out: it
 * changes while the run goes, so a page boundary in it would move.
 */
export type RunSort = "started" | "repository" | "took";
export type SortDir = "asc" | "desc";

export interface RunQuery {
  /** The last run of the page before. */
  before?: Run;
  status?: JobGroup;
  /** Started, newest first, when not set. */
  sort?: RunSort;
  dir?: SortDir;
  /** Text a run's repository, workflow or branch, or one of its jobs' name, label or instance, contains. */
  q?: string;
}

export const listRuns = (org: string, { before, status, sort = "started", dir = "desc", q }: RunQuery = {}) => {
  const query = new URLSearchParams();
  if (status) {
    query.set("status", status);
  }
  if (sort !== "started" || dir !== "desc") {
    query.set("sort", sort);
    query.set("dir", dir);
  }
  if (q) {
    query.set("q", q);
  }
  if (before) {
    const third =
      sort === "repository" ? `:${before.repository}` : sort === "took" ? `:${before.endedAt ?? ""}` : "";
    query.set("before", `${before.queuedAt}:${before.id}${third}`);
  }
  const search = query.toString();
  return get<RunPage>(`${tenantPath(org)}/runs${search ? `?${search}` : ""}`);
};

/** What one job used of its instance; maxima are approximate when the data is sampled. */
export interface JobResources {
  /** The job id, as a string. */
  job: string;
  cpuAvg: number | null;
  cpuMax: number | null;
  memMax: number | null;
  memTotal: number | null;
  oomKills: number | null;
}

export interface RunDetail {
  run: Run & { jobs: (Job & { current: boolean })[] };
  region: string | null;
  /** null when the numbers could not be read. */
  resources: JobResources[] | null;
}

export const getRun = (org: string, id: string) => get<RunDetail>(`${tenantPath(org)}/runs/${encodeURIComponent(id)}`);

/** Ten seconds of a runner's samples, as the agent summed them up. */
export interface MetricsWindow {
  /** When the window ended, Unix seconds. */
  at: number;
  seconds: number;
  /** Busy CPU across all cores, percent: lowest and highest second, and the mean. */
  cpuMin: number;
  cpuAvg: number;
  cpuMax: number;
  cpuSteal: number;
  /** Bytes. */
  memMax: number;
  memTotal: number;
  cgroupPeak: number;
  oomKills: number;
  diskUsed: number;
  netRx: number;
  netTx: number;
}

/** A job's windows together; maxima are approximate when the data is sampled. */
export interface MetricsSummary {
  seconds: number | null;
  cpuAvg: number | null;
  cpuMax: number | null;
  cpuSteal: number | null;
  memMax: number | null;
  memTotal: number | null;
  cgroupPeak: number | null;
  oomKills: number | null;
  diskUsed: number | null;
  netRx: number | null;
  netTx: number | null;
}

export interface JobMetrics {
  /** The job on GitHub, with its logs. */
  githubUrl: string;
  /** The tenant's region, for the instance's console page; null when AWS is not connected. */
  region?: string | null;
  job: Pick<
    Job,
    | "id"
    | "runId"
    | "name"
    | "runAttempt"
    | "label"
    | "status"
    | "conclusion"
    | "runnerName"
    | "instanceId"
    | "queuedAt"
    | "startedAt"
    | "completedAt"
    | "updatedAt"
  > & { repository: string; workflowName: string | null; runNumber: number | null };
  /** null when the runner reported nothing. */
  summary: MetricsSummary | null;
  /** Oldest first. */
  windows: MetricsWindow[];
}

export const getJobMetrics = (org: string, id: string) =>
  get<JobMetrics>(`${tenantPath(org)}/jobs/${encodeURIComponent(id)}/metrics`);
export const saveSettings = (org: string, maxRunners: number, allowedRegions: string[] | null) =>
  put<Tenant>(`${tenantPath(org)}/settings`, { maxRunners, allowedRegions });

export type Arch = "amd64" | "arm64";
export type Capacity = "on-demand" | "spot" | "spot-with-fallback";

/** A runner profile as workflows name it in runs-on. */
export interface Profile {
  name: string;
  arch: Arch;
  /** Instance categories such as "m"; empty for any. */
  categories: string[];
  /** The oldest generation; null for any. The newest that fits goes first. */
  minGeneration: number | null;
  /** Families to pin; empty for any that meets the rest. */
  families: string[];
  cpuMin: number;
  cpuMax: number | null;
  /** GiB. */
  memoryMin: number;
  memoryMax: number | null;
  /** GiB; null for the connect stack's default. */
  diskGib: number | null;
  capacity: Capacity;
  image: string;
  extras: string[];
  warmPool: number;
}

export interface Estimate {
  /** Newest generation first, cheapest first within it. */
  instanceTypes: string[];
  vcpus: number;
  memoryGib: number;
  diskGib: number;
  /** On-demand, millionths of a US dollar. */
  hourlyMicros: number;
}

export interface ProfileView extends Profile {
  source: "builtIn" | "replaced" | "custom";
  estimate: Estimate | null;
  /** Why it cannot be launched. */
  problem: string | null;
}

export interface Family {
  name: string;
  /** As in "m". */
  category: string;
  generation: number;
  arch: Arch;
  gibPerVcpu: number;
  /** [size name, vCPUs], smallest first. */
  sizes: [string, number][];
  /** Whether the region offers it. */
  offered: boolean;
  /** Whether the connect stack allows at least one size. */
  allowed: boolean;
  /** GPUs, accelerators, bare metal, burstable or flex: only a pin launches it. */
  special: boolean;
  pricePerVcpu: number | null;
}

export interface Runners {
  /** The tenant's region, or where prices come from before AWS is connected. */
  region: string;
  connected: boolean;
  limits: null | {
    instanceTypes: string[];
    excludedInstanceTypes: string[];
    volumeSizeLimit: number;
    rootVolumeSize: number;
  };
  images: string[];
  families: Family[];
  profiles: ProfileView[];
  maxProfiles: number;
}

export type Preview = Partial<ProfileView> & { problem: string | null };

const runnersPath = (org: string) => `${tenantPath(org)}/runners`;
export const listRunners = (org: string) => get<Runners>(runnersPath(org));
export const previewProfile = (org: string, profile: Profile) => post<Preview>(`${runnersPath(org)}/preview`, profile);
export const createProfile = (org: string, profile: Profile) => post<Runners>(runnersPath(org), profile);
export const replaceProfile = (org: string, name: string, profile: Profile) =>
  put<Runners>(`${runnersPath(org)}/${encodeURIComponent(name)}`, profile);
export const deleteProfile = (org: string, name: string) =>
  request<Runners>("DELETE", `${runnersPath(org)}/${encodeURIComponent(name)}`);

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

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
}

export interface Billing {
  beta: boolean;
  licensed: boolean;
  subscription: null | {
    status: string;
    currentPeriodEnd: string | null;
    cancelAtPeriodEnd: boolean;
  };
}

/** One of the licence's Polar products, one per billing interval. */
export interface Plan {
  id: string;
  name: string;
  description: string | null;
  /** Smallest currency unit, such as cents. */
  amount: number | null;
  currency: string | null;
  interval: string | null;
}

export interface Plans {
  /** Shortest billing interval first. */
  plans: Plan[];
}

export interface Steps {
  app: boolean;
  billing: boolean;
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
  templateUrl: string | null;
  regions: AwsRegion[];
}

export const getAwsInstructions = (org: string) => get<AwsInstructions>(`${tenantPath(org)}/aws`);
export const connectAws = (org: string, region: string, roleArn: string) =>
  post<Tenant>(`${tenantPath(org)}/aws`, { region, roleArn });

export interface Job {
  id: number;
  repository: string;
  runId: number;
  label: string;
  status: "queued" | "in_progress" | "completed";
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
  /** Unix milliseconds. */
  queuedAt: number;
  updatedAt: number;
}

export interface JobCounts {
  waiting: number;
  active: number;
  completedToday: number;
  failedToday: number;
}

export interface JobPage {
  jobs: Job[];
  counts: JobCounts;
  region: string | null;
  pageSize?: number;
}

export const listJobs = (org: string, before?: number) =>
  get<JobPage>(`${tenantPath(org)}/jobs${before ? `?before=${before}` : ""}`);
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

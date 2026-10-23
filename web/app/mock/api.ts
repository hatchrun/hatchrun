// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, Server, ServerResponse } from "node:http";
import type { Plugin } from "vite";

import type { Job, ListedRun, Run } from "../src/api.ts";
import { handleCi, serveLiveSockets } from "./ci.ts";
import { billing as ciBilling } from "./ci-settings.ts";
import * as profiles from "./runners.ts";

interface MockTenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  aws: null | {
    roleArn: string;
    region: string;
    launchTemplateId: string;
    subnetIds: string[];
    stack: ReturnType<typeof oldStack>;
  };
  maxRunners: number;
  allowedRegions: string[] | null;
  beta: boolean;
  subscription: null | { status: string; currentPeriodEnd: string | null; cancelAtPeriodEnd: boolean };
  /** Runner seconds this month. */
  seconds: number;
}

interface State {
  signedIn: boolean;
  tenants: MockTenant[];
  /** Organization ids the App is installed on. */
  installed: number[];
}

const state: State = { signedIn: false, tenants: [], installed: [] };

const square = (color: string) =>
  `data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><rect width='2' height='2' fill='${encodeURIComponent(color)}'/></svg>`;

const orgs = [
  { id: 101, login: "acme-corp", avatarUrl: square("#3b82f6") },
  { id: 102, login: "octo-labs", avatarUrl: square("#10b981") },
];

function tenantView(tenant: MockTenant) {
  const licensed = tenant.beta || tenant.subscription?.status === "active";
  const now = new Date();
  const monthStart = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1));
  const monthEnd = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 1));
  const { seconds } = tenant;
  return {
    ...tenant,
    billing: {
      beta: tenant.beta,
      licensed,
      subscription: tenant.subscription,
      usage: {
        monthStart: monthStart.toISOString(),
        monthEnd: monthEnd.toISOString(),
        seconds,
        spendCents: licensed && !tenant.beta ? Math.floor(Math.floor((seconds * 1_000) / 60) / 10_000) : 0,
        priceMicrosPerMinute: 1_000,
        mayLaunch: licensed,
      },
    },
    steps: { app: tenant.installationId !== null, aws: tenant.aws !== null },
    installUrl: installUrl(),
  };
}

// GitHub's install page, played by /api/mock/install: it installs on the
// first organization without the App.
const installUrl = () => "/api/mock/install?state=mock";

const findTenant = (login: string) => state.tenants.find((tenant) => tenant.org.login === login);

const user = { id: 1, login: "octocat", name: "Mona Octocat", avatarUrl: square("#f5a524") };

type Handler = (req: IncomingMessage, url: URL, body: unknown) => [number, unknown] | string;

const routes: Record<string, Handler> = {
  "GET /api/auth/login": (_req, url) => {
    state.signedIn = true;
    return url.searchParams.get("return_to") ?? "/";
  },
  "POST /api/auth/logout": () => {
    state.signedIn = false;
    return [204, null];
  },
  "GET /api/session": () =>
    state.signedIn ? [200, { user, csrfToken: "mock" }] : [401, { error: "not signed in" }],
  "GET /api/installations": () => [
    200,
    {
      installUrl: installUrl(),
      installations: orgs
        .filter((org) => state.installed.includes(org.id))
        .map((org) => ({
          id: org.id * 10,
          login: org.login,
          avatarUrl: org.avatarUrl,
          suspended: false,
          tenant: findTenant(org.login)?.org.login ?? null,
        })),
    },
  ],
  "GET /api/mock/install": (_req, url) => {
    const org = orgs.find((candidate) => !state.installed.includes(candidate.id));
    if (!org) {
      return "/";
    }
    state.installed.push(org.id);
    return `/api/github/setup?installation_id=${org.id * 10}&setup_action=install&state=${url.searchParams.get("state")}`;
  },
  "GET /api/tenants": () => [
    200,
    state.tenants.map((tenant) => ({ id: tenant.id, login: tenant.org.login, avatarUrl: tenant.org.avatarUrl })),
  ],
  "POST /api/tenants": (_req, _url, body) => {
    const tenant = adopt((body as { installationId: number }).installationId);
    return tenant ? [200, tenantView(tenant)] : [404, { error: "that installation isn't one you can see on GitHub" }];
  },
  "GET /api/tenants/:org": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    return tenant ? [200, tenantView(tenant)] : [404, { error: "no organization here" }];
  },
  "POST /api/tenants/:org/checkout": (_req, url) => {
    const org = url.pathname.split("/")[3] ?? "";
    return [200, { url: `/api/mock/polar?org=${org}${url.searchParams.get("return") === "ci" ? "&return=ci" : ""}` }];
  },
  "GET /api/mock/polar": (_req, url) => {
    const org = url.searchParams.get("org") ?? "";
    const tenant = findTenant(org);
    if (tenant) {
      const end = new Date(Date.now() + 30 * 24 * 3600 * 1000).toISOString();
      setTimeout(() => {
        tenant.subscription = { status: "active", currentPeriodEnd: end, cancelAtPeriodEnd: false };
      }, 4000);
    }
    if (url.searchParams.get("return") === "ci") {
      ciBilling.card = true;
      return `/o/${org}/ci?checkout=mock`;
    }
    return `/o/${org}/settings?checkout=mock`;
  },
  "POST /api/tenants/:org/portal": (_req, url) => {
    const org = url.pathname.split("/")[3];
    return [200, { url: url.searchParams.get("return") === "ci" ? `/o/${org}/ci?portal=mock` : `/o/${org}?portal=mock` }];
  },
  "POST /api/tenants/:org/beta": (_req, url, body) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    if ((body as { code: string }).code.trim() !== "BETA") {
      return [400, { error: "that invite code is not valid or used up" }];
    }
    tenant.beta = true;
    return [200, tenantView(tenant)];
  },
  "GET /api/tenants/:org/aws": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    const regions: [string, string][] = [
      ["us-east-1", "US East (N. Virginia)"],
      ["us-west-2", "US West (Oregon)"],
      ["eu-west-1", "Europe (Ireland)"],
      ["ap-southeast-2", "Asia Pacific (Sydney)"],
    ];
    const template = "https://hatchrun-templates.s3.us-east-1.amazonaws.com/connect.yaml";
    return [
      200,
      {
        tenantId: tenant.id,
        marketplace: {
          amd64: "https://aws.amazon.com/marketplace/pp/prodview-znhn62372z6jq",
          arm64: "https://aws.amazon.com/marketplace/pp/prodview-nchg6i6uzlfr2",
        },
        templateUrl: template,
        regions: regions.map(([id, name]) => ({
          id,
          name,
          hasImage: id !== "ap-southeast-2",
          allowed: tenant.allowedRegions === null || tenant.allowedRegions.includes(id),
          launchUrl: `https://${id}.console.aws.amazon.com/cloudformation/home?region=${id}#/stacks/create/review?templateURL=${encodeURIComponent(template)}&stackName=hatchrun&param_TenantId=${tenant.id}`,
        })),
      },
    ];
  },
  "POST /api/tenants/:org/aws": (_req, url, body) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    const { region, roleArn } = body as { region: string; roleArn: string };
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    if (!/^arn:aws[a-z-]*:iam::\d{12}:role\/.+$/.test(roleArn)) {
      return [422, { error: "That is not a role ARN. Paste the RoleArn output of the stack." }];
    }
    if (roleArn.endsWith("/wrong")) {
      return [
        422,
        {
          error: `AWS would not let hatchrun assume ${roleArn}. Check that the stack was created with tenant ID ${tenant.id} and that this is its RoleArn output. AWS said: Not authorized to perform sts:AssumeRoleWithWebIdentity`,
        },
      ];
    }
    tenant.aws = {
      roleArn,
      region,
      launchTemplateId: "lt-0a1b2c3d4e5f60718",
      subnetIds: ["subnet-0aa11", "subnet-0bb22"],
      stack: oldStack(region),
    };
    return [200, tenantView(tenant)];
  },
  "GET /api/tenants/:org/runs": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    const now = Date.now();
    const runs = sampleRuns(now);
    const all = runs.flatMap((run) => run.jobs);
    const [at = "", id = "", ...rest] = (url.searchParams.get("before") ?? "").split(":");
    const status = url.searchParams.get("status");
    const group = status === "active" || status === "waiting" || status === "failed" ? IN_GROUP[status] : null;
    const sort = url.searchParams.get("sort") ?? "started";
    const dir = url.searchParams.get("dir") ?? "desc";
    if ((status && !group) || !["started", "repository", "took"].includes(sort) || !["asc", "desc"].includes(dir)) {
      return [400, { error: "status is active, waiting or failed; sort is started, repository or took; dir is asc or desc." }];
    }
    const q = (url.searchParams.get("q") ?? "").trim().toLowerCase();
    // As list_query in crates/cloud/src/app/dashboard.rs: by repository
    // case aside, each repository newest first; by took, the runs still
    // going, oldest first, then finished runs by how long they took, ties
    // by id; and the cursor is the last run of the page before.
    type Key = Pick<Run, "queuedAt" | "id" | "repository" | "endedAt">;
    const newest = (a: Key, b: Key) => b.queuedAt - a.queuedAt || b.id - a.id;
    const name = (run: Key) => run.repository.toLowerCase();
    const took = (a: Key, b: Key) => {
      if ((a.endedAt === null) !== (b.endedAt === null)) {
        return a.endedAt === null ? -1 : 1;
      }
      if (a.endedAt === null || b.endedAt === null) {
        return a.queuedAt - b.queuedAt || a.id - b.id;
      }
      return (a.endedAt - a.queuedAt - (b.endedAt - b.queuedAt)) * (dir === "asc" ? 1 : -1) || a.id - b.id;
    };
    const order =
      sort === "repository"
        ? (a: Key, b: Key) => (name(a) < name(b) ? -1 : name(a) > name(b) ? 1 : 0) * (dir === "asc" ? 1 : -1) || newest(a, b)
        : sort === "took"
          ? took
          : (a: Key, b: Key) => newest(a, b) * (dir === "asc" ? -1 : 1);
    const first = !at;
    const third = rest.join(":");
    const cursor: Key = {
      queuedAt: Number(at),
      id: Number(id),
      repository: third,
      endedAt: sort === "took" && third !== "" ? Number(third) : null,
    };
    const matches = (run: ListedRun) =>
      !q ||
      [run.repository, run.workflowName, run.headBranch, ...[...run.jobs, ...(OLDER.get(run.id) ?? [])].flatMap((j) => [j.name, j.label, j.instanceId])].some(
        (value) => value?.toLowerCase().includes(q),
      );
    const pageSize = 8;
    const day = all.filter((job) => job.updatedAt >= now - 86_400_000);
    const activity = first ? sampleActivity(now, day) : undefined;
    const sum = (values: number[] | undefined) => (values ?? []).reduce((a, b) => a + b, 0);
    const runsIn = (test: (job: Job) => boolean, list = runs) => list.filter((run) => run.jobs.some(test));
    const sorted = (group ? runsIn(group) : runs).filter(matches).sort(order);
    // The sample times move with the clock between requests, so the
    // page starts after the cursor's run by id while it is still listed.
    const after = sorted.findIndex((run) => run.id === cursor.id);
    const listed = (first ? sorted : after >= 0 ? sorted.slice(after + 1) : sorted.filter((run) => order(cursor, run) < 0)).slice(
      0,
      pageSize,
    );
    return [
      200,
      {
        runs: listed,
        counts: {
          waiting: all.filter(IN_GROUP.waiting).length,
          active: all.filter(IN_GROUP.active).length,
          running: all.filter((job) => IN_GROUP.active(job) && job.status === "in_progress").length,
          completedToday: activity ? sum(activity.finished) : day.filter((job) => job.status === "completed").length,
          failedToday: day.filter(IN_GROUP.failed).length,
          waitingRuns: runsIn(IN_GROUP.waiting).length,
          activeRuns: runsIn(IN_GROUP.active).length,
          failedRunsToday: runsIn((job) => IN_GROUP.failed(job) && job.updatedAt >= now - 86_400_000).length,
        },
        region: tenant.aws?.region ?? null,
        pageSize,
        ...(activity && { activity }),
      },
    ];
  },
  "GET /api/tenants/:org/runs/:id": (_req, url) => {
    const [, , , org, , id] = url.pathname.split("/");
    const tenant = findTenant(org ?? "");
    const now = Date.now();
    const run = sampleRuns(now).find((run) => String(run.id) === id);
    if (!tenant || !run) {
      return [404, { error: `no run ${id} here` }];
    }
    const { jobs: current, ...rest } = run;
    delete (rest as Partial<ListedRun>).olderJobs;
    const jobs = [...(OLDER.get(run.id) ?? []).map((job) => ({ ...job, current: false })), ...current.map((job) => ({ ...job, current: true }))];
    return [
      200,
      {
        run: { ...rest, jobs },
        region: tenant.aws?.region ?? null,
        resources: jobs
          .filter((job) => job.launchState === "launched")
          .map((job) => {
            const windows = sampleWindows(job.queuedAt).filter((w) => w.at * 1000 <= now);
            const summary = summarize(windows);
            return {
              job: String(job.id),
              cpuAvg: summary.cpuAvg,
              cpuMax: summary.cpuMax,
              memMax: summary.memMax,
              memTotal: summary.memTotal,
              oomKills: job.id % 7 === 0 ? 2 : 0,
            };
          }),
      },
    ];
  },
  "GET /api/tenants/:org/jobs/:id/metrics": (_req, url) => {
    const [, , , org, , id] = url.pathname.split("/");
    const tenant = findTenant(org ?? "");
    const run = sampleRuns(Date.now()).find((run) => run.jobs.some((job) => String(job.id) === id));
    const job = run?.jobs.find((job) => String(job.id) === id);
    if (!tenant || !run || !job) {
      return [404, { error: `no job ${id} here` }];
    }
    const windows =
      job.launchState === "launched"
        ? sampleWindows(job.queuedAt).filter((w) => w.at * 1000 <= (job.completedAt ?? Date.now()))
        : [];
    return [
      200,
      {
        githubUrl: `https://github.com/${run.repository}/actions/runs/${run.id}/job/${job.id}`,
        job: { ...job, repository: run.repository, workflowName: run.workflowName, runNumber: run.runNumber },
        region: tenant.aws?.region ?? null,
        summary: windows.length === 0 ? null : summarize(windows),
        windows,
      },
    ];
  },
  "PUT /api/tenants/:org/settings": (_req, url, body) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    const { maxRunners, allowedRegions } = body as { maxRunners: number; allowedRegions: string[] | null };
    if (allowedRegions && tenant.aws && !allowedRegions.includes(tenant.aws.region)) {
      return [
        422,
        { error: `AWS is connected in ${tenant.aws.region}. Keep it allowed, or connect another region first.` },
      ];
    }
    tenant.maxRunners = maxRunners;
    tenant.allowedRegions = allowedRegions;
    return [200, tenantView(tenant)];
  },
  "GET /api/tenants/:org/runners": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    return tenant
      ? [200, profiles.runners(tenant.org.login, tenant.aws?.region ?? null)]
      : [404, { error: "no organization here" }];
  },
  "POST /api/tenants/:org/runners/preview": (_req, url, body) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    return tenant
      ? [200, profiles.preview(body as Parameters<typeof profiles.preview>[0], tenant.aws?.region ?? null)]
      : [404, { error: "no organization here" }];
  },
  "POST /api/tenants/:org/runners": (req, url, body) => runnerChange(req, url, body),
  "PUT /api/tenants/:org/runners/:name": (req, url, body) => runnerChange(req, url, body),
  "DELETE /api/tenants/:org/runners/:name": (req, url, body) => runnerChange(req, url, body),
  "GET /api/github/setup": (_req, url) => {
    if (url.searchParams.get("setup_action") === "request") {
      return "/?install=requested";
    }
    if (url.searchParams.get("state") !== "mock") {
      return "/?error=install-state";
    }
    const tenant = adopt(Number(url.searchParams.get("installation_id")));
    return tenant ? `/o/${tenant.org.login}?installed=1` : "/?error=install-unseen";
  },
};

/** The tenant of an installed organization, made on first use. */
function adopt(installationId: number): MockTenant | null {
  const org = orgs.find((candidate) => candidate.id * 10 === installationId && state.installed.includes(candidate.id));
  if (!org) {
    return null;
  }
  let tenant = findTenant(org.login);
  if (!tenant) {
    tenant = {
      id: `mock${org.id}tenant00`,
      org,
      installationId: null,
      aws: null,
      maxRunners: 40,
      allowedRegions: null,
      beta: false,
      subscription: null,
      seconds: 74_400,
    };
    state.tenants.push(tenant);
  }
  tenant.installationId = installationId;
  return tenant;
}

function runnerChange(req: IncomingMessage, url: URL, body: unknown): [number, unknown] {
  const [, , , org = "", , name] = url.pathname.split("/");
  const tenant = findTenant(org);
  if (!tenant) {
    return [404, { error: "no organization here" }];
  }
  return profiles.change(
    org,
    tenant.aws?.region ?? null,
    req.method ?? "GET",
    name ? decodeURIComponent(name) : null,
    (body as Parameters<typeof profiles.preview>[0] | undefined) ?? null,
  );
}

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(chunk as Buffer);
  }
  const text = Buffer.concat(chunks).toString();
  return text ? JSON.parse(text) : undefined;
}

function answer(res: ServerResponse, status: number, body: unknown) {
  res.statusCode = status;
  if (status === 204) {
    res.end();
    return;
  }
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(body));
}

export function mockApi(): Plugin {
  return {
    name: "hatchrun-mock-api",
    configureServer(server) {
      if (server.httpServer) {
        // The mock server is plain HTTP.
        serveLiveSockets(server.httpServer as unknown as Server);
      }
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        if (!url.pathname.startsWith("/api/")) {
          next();
          return;
        }
        if (url.pathname.startsWith("/api/ci/")) {
          await handleCi(req, res, url, { signedIn: state.signedIn, csrf: "mock" }, await readBody(req));
          return;
        }
        const key = `${req.method} ${url.pathname}`;
        const handler =
          routes[key] ??
          Object.entries(routes).find(([pattern]) => {
            const regex = new RegExp(`^${pattern.replace(/:[a-z]+/g, "[^/]+")}$`);
            return regex.test(key);
          })?.[1];
        if (!handler) {
          answer(res, 404, { error: "not found" });
          return;
        }
        const result = handler(req, url, await readBody(req));
        if (typeof result === "string") {
          res.statusCode = 302;
          res.setHeader("Location", result);
          res.end();
          return;
        }
        answer(res, result[0], result[1]);
      });
    },
  };
}

/** Twelve minutes of a build: a busy compile, a quiet test run, one OOM kill. */
function sampleWindows(queuedAt: number) {
  const gib = 1024 ** 3;
  const start = Math.round(queuedAt / 1000) + 40;
  return Array.from({ length: 72 }, (_, i) => {
    const busy = i < 40 ? 70 + 25 * Math.sin(i / 3) : 25 + 10 * Math.sin(i / 2);
    return {
      at: start + (i + 1) * 10,
      seconds: 10,
      cpuMin: Math.max(0, busy - 20),
      cpuAvg: busy,
      cpuMax: Math.min(100, busy + 15),
      cpuSteal: 0.4,
      memMax: (i < 40 ? 2 + i * 0.12 : 3.2) * gib,
      memTotal: 8 * gib,
      cgroupPeak: 6.9 * gib,
      oomKills: i === 38 ? 1 : 0,
      diskUsed: (11 + i * 0.05) * gib,
      netRx: i < 6 ? 120e6 : 2e6,
      netTx: 1e6,
    };
  });
}

function summarize(windows: ReturnType<typeof sampleWindows>) {
  const sum = (pick: (w: (typeof windows)[number]) => number) => windows.reduce((total, w) => total + pick(w), 0);
  const max = (pick: (w: (typeof windows)[number]) => number) => Math.max(...windows.map(pick));
  return {
    seconds: sum((w) => w.seconds),
    cpuAvg: sum((w) => w.cpuAvg) / windows.length,
    cpuMax: max((w) => w.cpuMax),
    cpuSteal: sum((w) => w.cpuSteal) / windows.length,
    memMax: max((w) => w.memMax),
    memTotal: max((w) => w.memTotal),
    cgroupPeak: max((w) => w.cgroupPeak),
    oomKills: sum((w) => w.oomKills),
    diskUsed: max((w) => w.diskUsed),
    netRx: sum((w) => w.netRx),
    netTx: sum((w) => w.netTx),
  };
}

/** Jobs of earlier attempts, by run, that later attempts replaced. */
const OLDER = new Map<number, Job[]>();

function sampleRuns(now: number): ListedRun[] {
  const minute = 60_000;
  let next = 9200;
  const job = (
    name: string,
    label: string,
    status: Job["status"],
    launchState: Job["launchState"],
    queuedAgo: number,
    took: number,
    {
      conclusion = status === "completed" ? "success" : null,
      launchError = null,
      attempts = launchState ? 1 : 0,
      wait = null,
      inMinutes,
      waited = 40,
      runAttempt = 1,
    }: {
      conclusion?: string | null;
      launchError?: string | null;
      attempts?: number;
      wait?: "backoff" | "setup" | null;
      inMinutes?: number;
      waited?: number;
      runAttempt?: number;
    } = {},
  ): Job => {
    const id = next++;
    const queuedAt = now - queuedAgo * minute;
    const started = status !== "queued" && launchState === "launched" ? queuedAt + waited * 1000 : null;
    return {
      id,
      runId: 0,
      name,
      runAttempt,
      label,
      status,
      conclusion,
      launchState,
      runnerName: launchState ? `hatchrun-${id}-ab12cd34` : null,
      instanceId: launchState === "launched" ? `i-0${(id * 2654435761).toString(16).padStart(16, "0").slice(-16)}` : null,
      launchError,
      launchAttempts: attempts,
      launchRetry: wait,
      nextLaunchAt: inMinutes === undefined ? null : now + inMinutes * minute,
      queuedAt,
      startedAt: started,
      completedAt: status === "completed" ? queuedAt + took * 1000 : null,
      updatedAt: queuedAt + took * 1000,
    };
  };
  const run = (
    id: number,
    fields: Partial<Run> & Pick<Run, "repository" | "workflowName">,
    jobs: Job[],
    older: Job[] = [],
  ): ListedRun => {
    for (const j of [...jobs, ...older]) {
      j.runId = id;
    }
    OLDER.set(id, older);
    const attempt = Math.max(1, ...jobs.map((j) => j.runAttempt));
    const queuedAt = Math.min(...jobs.filter((j) => j.runAttempt === attempt).map((j) => j.queuedAt));
    // As the jobs_end triggers in crates/cloud/migrations/0024_run_ended.sql.
    const endedAt = jobs.some((j) => j.status !== "completed")
      ? null
      : Math.max(queuedAt, ...jobs.map((j) => j.completedAt ?? j.updatedAt));
    return {
      id,
      headBranch: "main",
      headSha: (id * 2654435761).toString(16).padStart(40, "a").slice(0, 40),
      attempt,
      queuedAt,
      endedAt,
      runNumber: id % 1000,
      event: "push",
      displayTitle: null,
      actor: "octocat",
      path: ".github/workflows/ci.yml",
      githubUrl: `https://github.com/${fields.repository}/actions/runs/${id}`,
      ...fields,
      jobs,
      olderJobs: older.length,
    };
  };
  const repos = ["acme-corp/api", "acme-corp/web", "acme-corp/infra", "acme-corp/mobile", "acme-corp/data"];
  const workflows = ["CI", "Build images", "Release", "Nightly", "Lint"];
  const titles = [
    "fix(api): retry the token refresh",
    "feat(web): show runs as a tree",
    "chore(deps): bump vite to 8.3",
    "refactor(infra): split the network module",
    "docs: explain the cache scopes",
  ];
  // Finished runs going back two days, a few minutes apart.
  const done = Array.from({ length: 30 }, (_, i) => {
    const ago = 45 + i * 83;
    const jobs = Array.from({ length: 1 + (i % 3) }, (_, k) =>
      job(["build", "test", "lint"][k] ?? "build", ["hatchrun-4cpu-arm64", "hatchrun-2cpu", "hatchrun-8cpu-arm64"][(i + k) % 3] ?? "hatchrun", "completed", "launched",
        ago, 150 + ((i * 97 + k * 31) % 600), { conclusion: i % 9 === 4 && k === 0 ? "failure" : "success" }),
    );
    return run(18234560000 - i * 17, {
      repository: repos[i % repos.length] ?? "",
      workflowName: workflows[i % workflows.length] ?? "CI",
      displayTitle: titles[i % titles.length] ?? null,
      event: i % 4 === 0 ? "pull_request" : "push",
    }, jobs);
  });
  const label = "hatchrun-4cpu-arm64";
  return [
    run(18234567890, { repository: "acme-corp/api", workflowName: "Smoke test", displayTitle: "fix(agent): retry the cache upload", runNumber: 412 }, [
      job("Save (host)", label, "completed", "launched", 3, 72),
      job("Save (container)", label, "completed", "launched", 3, 90),
      job("Restore (host)", label, "in_progress", "launched", 3, 0, { waited: 35 }),
      job("Restore (container)", label, "queued", "launching", 3, 0),
      job("Verify", label, "queued", null, 3, 0),
    ]),
    run(18234567811, { repository: "acme-corp/mobile", workflowName: "CI", headBranch: "fix-login", event: "pull_request", displayTitle: "Fix login on Android 16" }, [
      job("build (android)", "hatchrun-16cpu", "queued", "failed", 6, 0, {
        launchError: "no capacity for c7g.xlarge, m7g.xlarge in any subnet: InsufficientInstanceCapacity: We currently do not have sufficient capacity in the Availability Zone you requested.",
        attempts: 3, wait: "backoff", inMinutes: 2,
      }),
      job("build (ios)", "hatchrun-16cpu", "in_progress", "launched", 6, 0),
    ]),
    run(18234567702, { repository: "acme-corp/data", workflowName: "Backfill", event: "workflow_dispatch", displayTitle: "Backfill" }, [
      job("load", "hatchrun-8cpu/spot=true", "queued", "failed", 8, 0, {
        launchError: "OptInRequired: subscribe this account to the hatchrun runner images in AWS Marketplace", wait: "setup",
      }),
    ]),
    run(18234567650, { repository: "acme-corp/web", workflowName: "CI", displayTitle: "feat(web): show runs as a tree" }, [
      job("lint", "hatchrun-2cpu", "completed", "launched", 40, 60),
      job("test", "hatchrun-2cpu", "in_progress", "launched", 2, 0, { runAttempt: 2 }),
    ], [job("test", "hatchrun-2cpu", "completed", "launched", 40, 200, { conclusion: "failure" })]),
    // One job each, waiting for a runner, launching one, starting on one and running.
    run(18234567990, { repository: "acme-corp/web", workflowName: "Lint", displayTitle: "chore(deps): bump vite to 8.3" }, [
      job("lint", "hatchrun-2cpu", "queued", null, 0.2, 0),
    ]),
    run(18234567980, { repository: "acme-corp/api", workflowName: "Build images", displayTitle: "fix(api): retry the token refresh" }, [
      job("image (arm64)", label, "queued", "launching", 0.5, 0),
    ]),
    run(18234567970, { repository: "acme-corp/data", workflowName: "CI", headBranch: "dbt-1.9", event: "pull_request", displayTitle: "Upgrade dbt to 1.9" }, [
      job("dbt build", "hatchrun-8cpu-arm64", "queued", "launched", 1, 0),
    ]),
    run(18234567960, { repository: "acme-corp/infra", workflowName: "Release", event: "release", displayTitle: "v2.14.0" }, [
      job("publish", label, "in_progress", "launched", 4, 0, { waited: 50 }),
    ]),
    run(18234567600, { repository: "acme-corp/infra", workflowName: "Nightly", event: "schedule", headBranch: "main" }, [
      job("plan", "hatchrun-16cpu/disk=300", "completed", "failed", 22, 4, {
        conclusion: "cancelled",
        launchError: "hatchrun-16cpu asks for a 300 GiB root volume and the connect stack allows at most 200 GiB. Raise VolumeSizeLimit on the stack or ask for less.",
      }),
    ]),
    ...done,
  ].sort((a, b) => b.queuedAt - a.queuedAt || b.id - a.id);
}

// The same groups as the counts query in crates/cloud/src/app/dashboard.rs.
const IN_GROUP: Record<"active" | "waiting" | "failed", (job: Job) => boolean> = {
  active: (job) => job.status !== "completed" && (job.launchState === "launching" || job.launchState === "launched"),
  waiting: (job) =>
    job.status === "queued" &&
    (job.launchState === null || (job.launchState === "failed" && job.launchRetry === "backoff")),
  failed: (job) => job.launchState === "failed" && !(job.status === "queued" && job.launchRetry === "backoff"),
};

// A day of 15 minute buckets: the sample jobs plus a busy working day
// around them, so the strip has a shape.
function sampleActivity(now: number, day: Job[]) {
  const bucketMs = 900_000;
  const since = (Math.floor(now / bucketMs) + 1 - 96) * bucketMs;
  const finished = Array.from({ length: 96 }, (_, i) => {
    const hour = new Date(since + i * bucketMs).getHours();
    const busy = hour >= 8 && hour <= 19 ? 1 : 0.15;
    return Math.round((2 + 4 * Math.abs(Math.sin(i * 1.7)) + 2 * Math.abs(Math.cos(i * 0.6))) * busy);
  });
  const failed = Array.from({ length: 96 }, () => 0);
  for (const job of day) {
    const i = Math.floor((job.updatedAt - since) / bucketMs);
    if (i < 0 || i >= 96) {
      continue;
    }
    if (IN_GROUP.failed(job)) {
      failed[i] = (failed[i] ?? 0) + 1;
    }
    if (job.status === "completed") {
      finished[i] = (finished[i] ?? 0) + 1;
    }
  }
  return { since, bucketMs, finished, failed };
}

// A stack from before the version tag, so the dashboard offers the update.
function oldStack(region: string) {
  const templateUrl = "https://hatchrun-templates.s3.us-east-1.amazonaws.com/connect.yaml";
  return {
    version: 1,
    latest: 3,
    checkedAt: Date.now() - 3_600_000,
    changes: [
      "Runners launch through EC2 Fleet, which picks the cheapest instance type with capacity. hatchrun reads which instance types your region offers and what they cost each day. The default limit allows any size up to 8xlarge and refuses GPU, accelerator and bare metal types, instead of naming families.",
      "Runners launch from the hatchrun images in AWS Marketplace. Subscribe this account to the amd64 and arm64 images once, then update the stack; until then runners do not start.",
    ],
    familyList: true,
    consoleUrl: `https://${region}.console.aws.amazon.com/cloudformation/home?region=${region}#/stacks?filteringText=hatchrun&filteringStatus=active&viewNested=true`,
    templateUrl,
    updateCommand: `aws cloudformation update-stack --region ${region} --stack-name hatchrun --template-url ${templateUrl} --capabilities CAPABILITY_IAM --parameters ParameterKey=TenantId,UsePreviousValue=true ParameterKey=OidcProviderArn,UsePreviousValue=true ParameterKey=VolumeSizeLimit,UsePreviousValue=true 'ParameterKey=InstanceTypes,ParameterValue="*.medium,*.large,*.xlarge,*.2xlarge,*.4xlarge,*.8xlarge"' ParameterKey=RootVolumeSize,UsePreviousValue=true ParameterKey=VpcId,UsePreviousValue=true ParameterKey=SubnetIds,UsePreviousValue=true`,
  };
}

// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";

import type { Job } from "../src/api.ts";
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
  return {
    ...tenant,
    billing: { beta: tenant.beta, licensed, subscription: tenant.subscription },
    steps: { app: tenant.installationId !== null, billing: licensed, aws: tenant.aws !== null },
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
  "GET /api/plan": () =>
    // MOCK_BILLING=off answers like a Worker without a Polar product.
    process.env.MOCK_BILLING === "off"
      ? [503, { error: "billing is not set up yet" }]
      : [
          200,
          {
            plans: [
              {
                id: "prod_month",
                name: "hatchrun Team, monthly",
                description: "Runners for one GitHub organization",
                amount: 4900,
                currency: "usd",
                interval: "month",
              },
              {
                id: "prod_year",
                name: "hatchrun Team, yearly",
                description: "Runners for one GitHub organization",
                amount: 49000,
                currency: "usd",
                interval: "year",
              },
            ],
          },
        ],
  "POST /api/tenants/:org/checkout": (_req, url) => {
    const org = url.pathname.split("/")[3] ?? "";
    return [200, { url: `/api/mock/polar?org=${org}` }];
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
    return `/o/${org}?checkout=mock`;
  },
  "POST /api/tenants/:org/portal": (_req, url) => [200, { url: `/o/${url.pathname.split("/")[3]}?portal=mock` }],
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
  "GET /api/tenants/:org/jobs": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    if (!tenant) {
      return [404, { error: "no organization here" }];
    }
    const now = Date.now();
    const all = sampleJobs(now);
    const before = Number(url.searchParams.get("before")) || Infinity;
    const status = url.searchParams.get("status");
    const group = status === "active" || status === "waiting" || status === "failed" ? IN_GROUP[status] : null;
    if (status && !group) {
      return [400, { error: "status is one of active, waiting or failed." }];
    }
    const pageSize = 12;
    const day = all.filter((job) => job.updatedAt >= now - 86_400_000);
    const activity = before === Infinity ? sampleActivity(now, day) : undefined;
    const sum = (values: number[] | undefined) => (values ?? []).reduce((a, b) => a + b, 0);
    return [
      200,
      {
        jobs: all.filter((job) => job.queuedAt < before && (!group || group(job))).slice(0, pageSize),
        counts: {
          waiting: all.filter(IN_GROUP.waiting).length,
          active: all.filter(IN_GROUP.active).length,
          running: all.filter((job) => IN_GROUP.active(job) && job.status === "in_progress").length,
          completedToday: activity ? sum(activity.finished) : day.filter((job) => job.status === "completed").length,
          failedToday: day.filter(IN_GROUP.failed).length,
        },
        region: tenant.aws?.region ?? null,
        pageSize,
        ...(activity && { activity }),
      },
    ];
  },
  "GET /api/tenants/:org/jobs/:id/metrics": (_req, url) => {
    const [, , , org, , id] = url.pathname.split("/");
    const tenant = findTenant(org ?? "");
    const job = sampleJobs(Date.now()).find((job) => String(job.id) === id);
    if (!tenant || !job) {
      return [404, { error: `no job ${id} here` }];
    }
    const windows =
      job.launchState === "launched"
        ? sampleWindows(job.queuedAt).filter((w) => w.at * 1000 <= Date.now())
        : [];
    return [
      200,
      {
        githubUrl: `https://github.com/${job.repository}/actions/runs/${job.runId}/job/${job.id}`,
        job,
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
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        if (!url.pathname.startsWith("/api/")) {
          next();
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

function sampleJobs(now: number): Job[] {
  const minute = 60_000;
  const job = (
    id: number,
    repository: string,
    label: string,
    status: Job["status"],
    launchState: Job["launchState"],
    queuedAgo: number,
    took: number,
    launchError: string | null = null,
    retry: { attempts: number; wait: "backoff" | "setup" | null; inMinutes?: number } = {
      attempts: launchState ? 1 : 0,
      wait: null,
    },
  ) => ({
    id,
    repository,
    runId: 11582000000 + id * 13,
    label,
    status,
    launchState,
    runnerName: launchState ? `hatchrun-${id}-ab12cd34` : null,
    instanceId: launchState === "launched" ? `i-0${(id * 2654435761).toString(16).padStart(16, "0").slice(-16)}` : null,
    launchError,
    launchAttempts: retry.attempts,
    launchRetry: retry.wait,
    nextLaunchAt: retry.inMinutes === undefined ? null : now + retry.inMinutes * minute,
    queuedAt: now - queuedAgo * minute,
    updatedAt: now - queuedAgo * minute + took * 1000,
  });
  const repos = ["acme-corp/api", "acme-corp/web", "acme-corp/infra", "acme-corp/mobile", "acme-corp/data"];
  const labels = ["hatchrun-4cpu-arm64", "hatchrun-2cpu", "hatchrun-8cpu-arm64", "hatchrun/disk=100", "hatchrun-16cpu"];
  // Finished jobs going back two days, a few minutes apart.
  const done = Array.from({ length: 70 }, (_, i) =>
    job(9118 - i, repos[i % repos.length] ?? "", labels[(i * 3) % labels.length] ?? "", "completed", "launched",
      45 + i * 37, 150 + ((i * 97) % 600)),
  );
  return [
    job(9127, "acme-corp/infra", "hatchrun", "queued", null, 0.1, 0),
    job(9126, "acme-corp/api", "hatchrun-8cpu-arm64", "queued", "launching", 0.3, 0),
    job(9123, "acme-corp/web", "hatchrun-2cpu", "queued", "launched", 0.8, 0),
    job(9124, "acme-corp/mobile", "hatchrun-16cpu", "queued", "failed", 1, 0,
      "no capacity for c7g.xlarge, m7g.xlarge in any subnet: InsufficientInstanceCapacity: We currently do not have sufficient capacity in the Availability Zone you requested.",
      { attempts: 3, wait: "backoff", inMinutes: 2 }),
    job(9125, "acme-corp/data", "hatchrun-8cpu/spot=true", "queued", "failed", 2, 0,
      "OptInRequired: subscribe this account to the hatchrun runner images in AWS Marketplace",
      { attempts: 1, wait: "setup" }),
    job(9122, "acme-corp/api", "hatchrun-4cpu-arm64", "in_progress", "launched", 5, 0),
    job(9121, "acme-corp/api", "hatchrun-8cpu-arm64/spot=true", "in_progress", "launched", 9, 0),
    job(9120, "acme-corp/infra", "hatchrun-16cpu/disk=300", "completed", "failed", 22, 4,
      "hatchrun-16cpu asks for a 300 GiB root volume and the connect stack allows at most 200 GiB. Raise VolumeSizeLimit on the stack or ask for less."),
    job(9119, "acme-corp/web", "hatchrun", "completed", "launched", 41, 312),
    ...done,
  ];
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

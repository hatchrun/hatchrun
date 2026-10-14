// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";

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
    const jobs = sampleJobs(now);
    return [
      200,
      {
        jobs: url.searchParams.has("before") ? [] : jobs,
        counts: { waiting: 1, active: 2, completedToday: 37, failedToday: 1 },
        region: tenant.aws?.region ?? null,
        pageSize: 5,
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
      maxRunners: 10,
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

function sampleJobs(now: number) {
  const minute = 60_000;
  const job = (
    id: number,
    repository: string,
    label: string,
    status: string,
    launchState: string | null,
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
    runId: id * 7,
    label,
    status,
    launchState,
    runnerName: launchState ? `hatchrun-${id}-ab12cd34` : null,
    instanceId: launchState === "launched" ? `i-0${id.toString(16)}f00ba4` : null,
    launchError,
    launchAttempts: retry.attempts,
    launchRetry: retry.wait,
    nextLaunchAt: retry.inMinutes === undefined ? null : now + retry.inMinutes * minute,
    queuedAt: now - queuedAgo * minute,
    updatedAt: now - queuedAgo * minute + took * 1000,
  });
  return [
    job(9123, "acme-corp/api", "hatchrun-8cpu-arm64", "queued", null, 0.2, 0),
    job(9124, "acme-corp/api", "hatchrun-4cpu-arm64", "queued", "failed", 1, 0,
      "no capacity for c7g.xlarge, m7g.xlarge in any subnet: InsufficientInstanceCapacity: We currently do not have sufficient capacity in the Availability Zone you requested.",
      { attempts: 3, wait: "backoff", inMinutes: 2 }),
    job(9125, "acme-corp/web", "hatchrun", "queued", "failed", 2, 0,
      "AssumeRoleWithWebIdentity: AccessDenied: Not authorized to perform sts:AssumeRoleWithWebIdentity",
      { attempts: 1, wait: "setup" }),
    job(9122, "acme-corp/web", "hatchrun", "in_progress", "launched", 3, 0),
    job(9121, "acme-corp/api", "hatchrun-8cpu-arm64/spot=true", "in_progress", "launched", 6, 0),
    job(9120, "acme-corp/infra", "hatchrun-16cpu/disk=300", "completed", "failed", 22, 4,
      "hatchrun-16cpu asks for a 300 GiB root volume and the connect stack allows at most 200 GiB. Raise VolumeSizeLimit on the stack or ask for less."),
    job(9119, "acme-corp/web", "hatchrun", "completed", "launched", 41, 312),
  ];
}

// A stack from before the version tag, so the dashboard offers the update.
function oldStack(region: string) {
  const templateUrl = "https://hatchrun-templates.s3.us-east-1.amazonaws.com/connect.yaml";
  return {
    version: 1,
    latest: 2,
    checkedAt: Date.now() - 3_600_000,
    changes: [
      "Runners launch through EC2 Fleet, which picks the cheapest instance type with capacity. hatchrun reads which instance types your region offers and what they cost each day. The default limit allows any size up to 8xlarge and refuses GPU, accelerator and bare metal types, instead of naming families.",
    ],
    familyList: true,
    consoleUrl: `https://${region}.console.aws.amazon.com/cloudformation/home?region=${region}#/stacks?filteringText=hatchrun&filteringStatus=active&viewNested=true`,
    templateUrl,
    updateCommand: `aws cloudformation update-stack --region ${region} --stack-name hatchrun --template-url ${templateUrl} --capabilities CAPABILITY_IAM --parameters ParameterKey=TenantId,UsePreviousValue=true ParameterKey=OidcProviderArn,UsePreviousValue=true ParameterKey=VolumeSizeLimit,UsePreviousValue=true 'ParameterKey=InstanceTypes,ParameterValue="*.medium,*.large,*.xlarge,*.2xlarge,*.4xlarge,*.8xlarge"' ParameterKey=RootVolumeSize,UsePreviousValue=true ParameterKey=VpcId,UsePreviousValue=true ParameterKey=SubnetIds,UsePreviousValue=true`,
  };
}

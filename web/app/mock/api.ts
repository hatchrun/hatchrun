// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";

interface MockTenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  aws: null | { roleArn: string; region: string; launchTemplateId: string; subnetIds: string[] };
  maxRunners: number;
}

interface State {
  signedIn: boolean;
  tenants: MockTenant[];
}

const state: State = { signedIn: false, tenants: [] };

const square = (color: string) =>
  `data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><rect width='2' height='2' fill='${encodeURIComponent(color)}'/></svg>`;

const orgs = [
  { id: 101, login: "acme-corp", avatarUrl: square("#3b82f6") },
  { id: 102, login: "octo-labs", avatarUrl: square("#10b981") },
];

function tenantView(tenant: MockTenant) {
  return {
    ...tenant,
    steps: { organization: true, aws: tenant.aws !== null, app: tenant.installationId !== null },
    installUrl: `/api/github/setup?installation_id=${tenant.org.id * 10}&setup_action=install&state=${tenant.id}`,
  };
}

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
  "GET /api/orgs": () => [
    200,
    orgs.map((org) => ({ ...org, tenant: findTenant(org.login)?.id ?? null })),
  ],
  "GET /api/tenants": () => [
    200,
    state.tenants.map((tenant) => ({ id: tenant.id, login: tenant.org.login, avatarUrl: tenant.org.avatarUrl })),
  ],
  "POST /api/tenants": (_req, _url, body) => {
    const login = (body as { org: string }).org;
    const org = orgs.find((candidate) => candidate.login === login);
    if (!org) {
      return [403, { error: `you are not an owner of ${login} on GitHub` }];
    }
    let tenant = findTenant(login);
    if (!tenant) {
      tenant = { id: `mock${org.id}tenant00`, org, installationId: null, aws: null, maxRunners: 10 };
      state.tenants.push(tenant);
    }
    return [200, tenantView(tenant)];
  },
  "GET /api/tenants/:org": (_req, url) => {
    const tenant = findTenant(url.pathname.split("/")[3] ?? "");
    return tenant ? [200, tenantView(tenant)] : [404, { error: "no organization here" }];
  },
  "GET /api/github/setup": (_req, url) => {
    const tenant = state.tenants.find((candidate) => candidate.id === url.searchParams.get("state"));
    if (!tenant) {
      return "/?error=install-state";
    }
    tenant.installationId = Number(url.searchParams.get("installation_id"));
    return `/o/${tenant.org.login}?installed=1`;
  },
};

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

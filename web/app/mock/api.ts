// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, Server, ServerResponse } from "node:http";
import type { Plugin } from "vite";

import { handleCi, serveLiveSockets } from "./ci.ts";
import { billing as ciBilling } from "./ci-settings.ts";

interface MockTenant {
  id: string;
  org: { id: number; login: string; avatarUrl: string };
  installationId: number | null;
  role: "admin" | "member";
}

interface State {
  signedIn: boolean;
  tenants: MockTenant[];
  /** Organization ids the App is installed on. */
  installed: number[];
}

const square = (color: string) =>
  `data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><rect width='2' height='2' fill='${encodeURIComponent(color)}'/></svg>`;

/** Organizations the App is not installed on yet: the start page installs them. */
const orgs = [
  { id: 101, login: "acme-corp", avatarUrl: square("#3b82f6") },
  { id: 102, login: "octo-labs", avatarUrl: square("#10b981") },
];

/** 52 organizations to switch between; "acme" has the org-scale data (mock/scale.ts), the rest are empty. */
const NAMES = "acme acme-labs globex initech umbrella hooli stark-industries wayne-enterprises wonka cyberdyne tyrell soylent".split(" ");
const seeded: MockTenant[] = Array.from({ length: 52 }, (_, i) => {
  const login = i < NAMES.length ? (NAMES[i] as string) : `${NAMES[i % NAMES.length]}-${Math.floor(i / NAMES.length)}`;
  return { id: `mock${i}tenant00`, org: { id: 200 + i, login, avatarUrl: square(`hsl(${(i * 47) % 360} 50% 50%)`) }, installationId: 2000 + i, role: i % 5 === 3 ? "member" : "admin" };
});

const state: State = { signedIn: false, tenants: seeded, installed: [] };

const installUrl = () => "/api/mock/install?state=mock";
const tenantView = (tenant: MockTenant) => ({ ...tenant, installUrl: installUrl() });

const findTenant = (login: string) => state.tenants.find((tenant) => tenant.org.login === login);

const user = { id: 1, login: "octocat", name: "Mona Octocat", avatarUrl: "" };

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
    ciBilling.card = true;
    return url.searchParams.get("return") === "ci" ? `/o/${org}/settings/billing?checkout=mock` : `/o/${org}/settings?checkout=mock`;
  },
  "POST /api/tenants/:org/portal": (_req, url) => {
    const org = url.pathname.split("/")[3];
    return [200, { url: `/o/${org}/settings/billing?portal=mock` }];
  },
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
    tenant = { id: `mock${org.id}tenant00`, org, installationId: null, role: "admin" };
    state.tenants.push(tenant);
  }
  tenant.installationId = installationId;
  return tenant;
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

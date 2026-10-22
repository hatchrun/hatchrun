// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The settings routes of /api/ci under `npm run dev:mock`, in the
// shapes of crates/ci/src/settings_api.rs: an organization's billing
// the secrets and variables of an organization (with the repositories
// each reaches), a repository or one of its environments, and a
// repository's environments.
// Any organization of the mock tenants answers, and so does the
// repository demo of each. Billing has a card (the mock Polar checkout adds one), 19,200 seconds and a $50
// cap; MOCK_CARD=off drops the card. Values are kept in memory and a
// secret's is never answered.

import type { IncomingMessage, ServerResponse } from "node:http";

interface Item {
  value: string;
  updatedAt: number;
  /** Organization items only: the repositories reached, null for all. */
  repositories?: string[] | null;
}

interface Environment {
  branches: string[];
  approval: boolean;
  updatedAt: number;
}

type Table = Map<string, Item>;

const orgs = ["acme", "acme-corp", "octo-labs"];
const day = 24 * 3600 * 1000;

const table = (rows: Record<string, string>): Table =>
  new Map(Object.entries(rows).map(([name, value], i) => [name, { value, updatedAt: Date.now() - (i + 1) * day }]));

/** The repositories of each organization that have run CI. */
const orgRepos = (org: string) => [`${org}/api`, `${org}/web`];

/** Environments by `<owner>/<repo>`, in the order of creation. */
const environments = new Map<string, Map<string, Environment>>();

function seedEnvironments(key: string): Map<string, Environment> {
  let found = environments.get(key);
  if (!found) {
    found = new Map([
      ["production", { branches: ["main"], approval: true, updatedAt: Date.now() - day }],
      ["staging", { branches: [], approval: false, updatedAt: Date.now() - 2 * day }],
    ]);
    environments.set(key, found);
  }
  return found;
}

/** Secrets and vars by scope key: `org:<login>`, `repo:<owner>/<repo>` or `env:<owner>/<repo>/<name>`. */
const secrets = new Map<string, Table>();
const vars = new Map<string, Table>();

function seed(key: string, kind: "secrets" | "vars"): Table {
  const store = kind === "secrets" ? secrets : vars;
  let found = store.get(key);
  if (!found) {
    const org = key.startsWith("org:");
    const env = key.startsWith("env:");
    found = table(
      kind === "secrets"
        ? org
          ? { NPM_TOKEN: "x", DEPLOY_KEY: "x" }
          : env
            ? key.endsWith("/production") ? { DEPLOY_TOKEN: "x" } : {}
            : { API_TOKEN: "x" }
        : org
          ? { REGION: "ap-south-1" }
          : {},
    );
    if (org && kind === "secrets") {
      const first = found.get("NPM_TOKEN");
      if (first) first.repositories = [`${key.slice("org:".length)}/api`];
    }
    store.set(key, found);
  }
  return found;
}

export const billing = { card: process.env.MOCK_CARD !== "off", capDollars: 50 };

const MAX_BYTES = 48 * 1024;
const NAME_MAX = 100;

const json = (res: ServerResponse, status: number, body?: unknown) => {
  res.statusCode = status;
  if (status === 204) {
    res.end();
    return;
  }
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(body));
};

/** The same checks as secrets::valid_name; the name uppercased, or the message. */
function validName(raw: string): string | { error: string } {
  const name = raw.toUpperCase();
  if (name === "") return { error: "Secret names cannot be empty" };
  if (name.length > NAME_MAX) return { error: `Secret names are at most ${NAME_MAX} characters` };
  if (!/^[A-Z0-9_]+$/.test(name)) return { error: "Use letters, digits and _ in secret names" };
  if (/^[0-9]/.test(name)) return { error: "Secret names cannot start with a digit" };
  if (name.startsWith("GITHUB_")) return { error: "Secret names cannot start with GITHUB_" };
  return name;
}

/** The organization item's repositories: null for all, the list, or the 422 for an unknown name. */
function checkRepositories(org: string, given: unknown): string[] | null | { error: string } {
  if (given === undefined || given === null) return null;
  if (!Array.isArray(given) || given.some((name) => typeof name !== "string")) {
    return { error: "repositories must be a list of names or null" };
  }
  const known = orgRepos(org);
  const unknown = (given as string[]).filter((name) => !known.some((seen) => seen.toLowerCase() === name.toLowerCase()));
  if (unknown.length > 0) return { error: `Unknown repositories: ${[...new Set(unknown)].join(", ")}` };
  return [...new Set(given as string[])].sort();
}

const environmentJson = (name: string, row: Environment) => ({ name, ...row });

function environmentRoute(
  res: ServerResponse,
  method: string,
  rows: Map<string, Environment>,
  name: string,
  body: unknown,
): true {
  if (name === "") {
    if (method !== "GET") {
      json(res, 405, { error: "method not allowed" });
      return true;
    }
    json(res, 200, [...rows.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([n, row]) => environmentJson(n, row)));
  } else if (method === "DELETE") {
    json(res, rows.delete(name) ? 204 : 404, { error: "not found" });
  } else if (method === "PUT") {
    const { branches, approval } = (body ?? {}) as { branches?: unknown; approval?: unknown };
    if (!/^[A-Za-z0-9._-]+$/.test(name) || /^\.+$/.test(name) || name.length > 100) {
      json(res, 400, { error: "Use letters, digits, - _ and . in environment names" });
    } else if (!Array.isArray(branches) || branches.some((b) => typeof b !== "string") || typeof approval !== "boolean") {
      json(res, 400, { error: 'expected {"branches": [...], "approval": true}' });
    } else {
      const existing = [...rows.keys()].find((seen) => seen.toLowerCase() === name.toLowerCase()) ?? name;
      const row = { branches: branches as string[], approval, updatedAt: Date.now() };
      rows.set(existing, row);
      json(res, 200, environmentJson(existing, row));
    }
  } else {
    json(res, 405, { error: "method not allowed" });
  }
  return true;
}

/**
 * Answers a settings request of /api/ci; false when `path` (after
 * `/api/ci/`) is not one, so the caller goes on.
 */
export function handleCiSettings(
  req: IncomingMessage,
  res: ServerResponse,
  path: string,
  session: { signedIn: boolean; csrf: string },
  body: unknown,
): boolean {
  const found =
    /^orgs\/([^/]+)\/(billing(?:\/cap)?|secrets|vars|repositories)(?:\/([^/]+))?$/.exec(path) ??
    /^repos\/([^/]+)\/([^/]+)\/(secrets|vars)(?:\/([^/]+))?$/.exec(path) ??
    /^repos\/([^/]+)\/([^/]+)\/environments(?:\/([^/]+)(?:\/(secrets|vars)(?:\/([^/]+))?)?)?$/.exec(path);
  if (!found) {
    return false;
  }
  const org = path.startsWith("orgs/");
  const inEnvironments = !org && path.split("/")[3] === "environments";
  const decoded = (part: string | undefined) => decodeURIComponent(part ?? "");
  const owner = decoded(found[1]);
  const repo = org ? "" : decoded(found[2]);
  // An environment's own route has no `what`; its secrets and variables do.
  const environment = inEnvironments ? decoded(found[3]) : "";
  const what = (org ? found[2] : inEnvironments ? found[4] : found[3]) ?? "";
  const item = decoded(org ? found[3] : inEnvironments ? found[5] : found[4]);
  const method = req.method ?? "GET";

  // A viewer who cannot manage the scope sees a 404, as the API answers.
  const known = org ? orgs.includes(owner) : (orgs.includes(owner) || owner === "e2e") && repo === "demo";
  if (!session.signedIn) {
    json(res, 401, { error: "sign in to manage billing" });
    return true;
  }
  if (!known) {
    json(res, 404, { error: "not found" });
    return true;
  }
  if (method !== "GET" && req.headers["x-csrf-token"] !== session.csrf) {
    json(res, 403, { error: "missing or wrong CSRF token" });
    return true;
  }

  if (what.startsWith("billing")) {
    const sub = what.slice("billing/".length);
    if (what === "billing" && method === "GET") {
      json(res, 200, {
        card: billing.card,
        seconds: 19_200,
        units: 19_200,
        spendCents: Math.floor((19_200 * 2_000) / 60 / 10_000),
        capDollars: billing.capDollars,
        minCapDollars: 50,
        maxCapDollars: 10_000,
        running: 2,
        maxRunning: 5,
      });
    } else if (sub === "cap" && method === "PUT") {
      const asked = (body as { capDollars?: unknown } | undefined)?.capDollars;
      if (typeof asked !== "number" || !Number.isInteger(asked)) {
        json(res, 400, { error: 'expected {"capDollars": 75}' });
      } else if (asked < 50 || asked > 10_000) {
        json(res, 422, { error: "Set a cap from $50.00 to $10000.00" });
      } else {
        billing.capDollars = asked;
        json(res, 204);
      }
    } else {
      json(res, 405, { error: "method not allowed" });
    }
    return true;
  }

  if (org && what === "repositories") {
    if (method !== "GET") {
      json(res, 405, { error: "method not allowed" });
    } else {
      json(res, 200, orgRepos(owner).map((name) => ({ name })));
    }
    return true;
  }

  const environmentRows = inEnvironments ? seedEnvironments(`${owner}/${repo}`) : null;
  if (environmentRows && what === "") {
    return environmentRoute(res, method, environmentRows, environment, body);
  }
  if (environmentRows && !environmentRows.has(environment)) {
    json(res, 404, { error: "not found" });
    return true;
  }

  const kind = what as "secrets" | "vars";
  const rows = seed(org ? `org:${owner}` : inEnvironments ? `env:${owner}/${repo}/${environment}` : `repo:${owner}/${repo}`, kind);
  if (item === "" && method === "GET") {
    const list = [...rows.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, row]) => {
        const repositories = org ? (row.repositories ?? null) : null;
        return kind === "secrets" ? { name, updatedAt: row.updatedAt, repositories } : { name, value: row.value, updatedAt: row.updatedAt, repositories };
      });
    json(res, 200, list);
    return true;
  }
  const name = validName(item);
  if (item === "" || typeof name !== "string") {
    json(res, 400, typeof name === "string" ? { error: "not found" } : name);
    return true;
  }
  if (method === "DELETE") {
    json(res, rows.delete(name) ? 204 : 404, { error: "not found" });
  } else if (method === "PUT") {
    const { value, repositories } = (body ?? {}) as { value?: unknown; repositories?: unknown };
    const chosen = org ? checkRepositories(owner, repositories) : null;
    if (typeof chosen === "object" && chosen !== null && "error" in chosen) {
      json(res, 422, chosen);
    } else if (typeof value !== "string") {
      json(res, 400, { error: 'expected {"value": "..."}' });
    } else if (value === "") {
      json(res, 400, { error: "Values cannot be empty" });
    } else if (Buffer.byteLength(value) > MAX_BYTES) {
      json(res, 413, { error: "Values are at most 48 KB" });
    } else if (!rows.has(name) && rows.size >= (org ? 1000 : 100)) {
      json(res, 409, { error: `${org ? owner : `${owner}/${repo}`} has ${org ? 1000 : 100} ${kind}, the most allowed` });
    } else {
      rows.set(name, { value, updatedAt: Date.now(), repositories: chosen as string[] | null });
      json(res, 204);
    }
  } else {
    json(res, 405, { error: "method not allowed" });
  }
  return true;
}

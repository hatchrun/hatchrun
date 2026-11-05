// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The settings routes of /api/ci under `npm run dev:mock`, in the shapes of
// crates/ci/src/settings_api.rs: an organization's billing the secrets and
// variables of an organization (with the repositories each reaches), a
// repository or one of its environments, and a repository's environments.
// An organization answers to its admins (a member or a stranger gets a 404),
// and so does the repository demo of each. Billing has a card (the mock Polar checkout adds one),
// 19,200 seconds and a $50 cap; MOCK_CARD=off drops the card. Values are
// kept in memory and a secret's is never answered.

import type { IncomingMessage, ServerResponse } from "node:http";

import { concurrencyNow, orgItems, paginate, SCALE_ORG, scaleEnvironments, scaleRepoNames } from "./scale.ts";

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

const day = 24 * 3600 * 1000;

const table = (rows: Record<string, string>): Table =>
  new Map(Object.entries(rows).map(([name, value], i) => [name, { value, updatedAt: Date.now() - (i + 1) * day }]));

/** The mock session: who is signed in, the CSRF token, and the viewer's role in an organization (null when it has no tenant). */
export interface Session {
  signedIn: boolean;
  csrf: string;
  role: (login: string) => "admin" | "member" | null;
}

/** Environments by `<owner>/<repo>`, in the order of creation. */
const environments = new Map<string, Map<string, Environment>>();

/** The environments of `<owner>/<repo>`; the organization-wide list reads them from here too. */
export function seedEnvironments(key: string): Map<string, Environment> {
  let found = environments.get(key);
  if (!found) {
    found = new Map(
      key.startsWith(`${SCALE_ORG}/`) && scaleRepoNames().includes(key)
        ? scaleEnvironments(key)
        : [
            ["production", { branches: ["main"], approval: true, updatedAt: Date.now() - day }],
            ["staging", { branches: [], approval: false, updatedAt: Date.now() - 2 * day }],
          ],
    );
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
            : key.startsWith(`repo:${SCALE_ORG}/`)
              ? { API_TOKEN: "x", TOKEN_NPM_1: "x" }
              : { API_TOKEN: "x", NPM_TOKEN: "x" }
        : org
          ? { REGION: "ap-south-1" }
          : {},
    );
    if (key === `org:${SCALE_ORG}`) {
      found = new Map(orgItems(kind).map(({ name, ...item }) => [name, item]));
    } else if (org && kind === "secrets") {
      const first = found.get("NPM_TOKEN");
      if (first) first.repositories = [`${key.slice("org:".length)}/api`];
    }
    store.set(key, found);
  }
  return found;
}

/** How many secrets and variables an environment of `<owner>/<repo>` holds. */
export const environmentCounts = (repository: string, name: string) => ({
  secrets: seed(`env:${repository}/${name}`, "secrets").size,
  vars: seed(`env:${repository}/${name}`, "vars").size,
});

export const billing = { capDollars: 200 };

// ---- spend per day, invoices, audit log -----------------------------------------

const small = (a: number, b: number) => {
  let h = (Math.imul(a + 0x9e3779b9, 0x85ebca6b) ^ Math.imul(b + 0x7f4a7c15, 0xc2b2ae35)) >>> 0;
  h ^= h >>> 15;
  h = Math.imul(h, 0x2c1b3c6d) >>> 0;
  return (h ^ (h >>> 13)) >>> 0;
};
const orgSeed = (org: string) => [...org].reduce((sum, ch) => sum * 31 + ch.charCodeAt(0), 7) >>> 0;

/** This UTC month's billed seconds and cents per day up to today: busy on weekdays, the same on every start. */
export function dailySpend(org: string) {
  const now = new Date();
  const factor = org === SCALE_ORG ? 1 : 0.04;
  const seed = orgSeed(org);
  return Array.from({ length: now.getUTCDate() }, (_, n) => {
    const date = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), n + 1));
    const weekend = date.getUTCDay() === 0 || date.getUTCDay() === 6;
    const seconds = Math.round((weekend ? 30_000 : 150_000) * factor * (0.6 + (small(seed, n) % 80) / 100));
    return { day: date.toISOString().slice(0, 10), seconds, cents: Math.floor(seconds / 300) };
  });
}

/** The last 12 months' Polar orders before this one, newest first. */
export function invoicesOf(org: string) {
  const now = new Date();
  const seed = orgSeed(org);
  return Array.from({ length: 12 }, (_, n) => {
    const date = Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - n, 1);
    const id = `ord_${(small(seed, n) >>> 0).toString(36).padStart(7, "0")}`;
    return { id, date, amount_cents: Math.round((org === SCALE_ORG ? 38_000 : 1_400) * (0.7 + (small(seed, 40 + n) % 60) / 100)), status: n === 4 ? "refunded" : "paid", url: `https://polar.sh/mock/orders/${id}` };
  });
}

export type AuditAction = "secret.put" | "secret.delete" | "var.put" | "var.delete" | "environment.put" | "environment.delete" | "repos.enabled";
interface AuditRow {
  id: number;
  at: number;
  actor: string;
  action: AuditAction;
  scope: string;
  target: string;
}
const AUDIT_ACTORS = ["octocat", "gamunu", "alice", "bob", "carol", "dave"];
const AUDIT_ACTIONS: AuditAction[] = ["secret.put", "secret.put", "secret.delete", "var.put", "var.delete", "environment.put", "environment.delete", "repos.enabled"];
/** Changes made through the mock since it started, newest first. */
const audited: AuditRow[] = [];
let auditSeq = 1_000_000;
export function recordAudit(action: AuditAction, scope: string, target: string, actor = "octocat") {
  audited.unshift({ id: (auditSeq += 1), at: Date.now(), actor, action, scope, target });
}

/** The acme log: 1,200 entries, one every 37 minutes, then what happened since the start. */
function auditRows(org: string, repos: () => string[]): AuditRow[] {
  if (org !== SCALE_ORG) return audited.filter((row) => row.scope === org || row.scope.startsWith(`${org}/`));
  const names = repos();
  const base = Array.from({ length: 1200 }, (_, n): AuditRow => {
    const action = AUDIT_ACTIONS[small(n, 1) % AUDIT_ACTIONS.length] as AuditAction;
    const repo = names[small(n, 2) % 40] as string;
    const wide = small(n, 3) % 3 === 0;
    const kind = action.startsWith("secret") ? "secret" : action.startsWith("var") ? "var" : action.startsWith("env") ? "environment" : "repos";
    return {
      id: 900_000 - n,
      at: Date.now() - (n + 1) * 37 * 60_000,
      actor: AUDIT_ACTORS[small(n, 4) % AUDIT_ACTORS.length] as string,
      action,
      scope: kind === "repos" || wide ? org : kind === "environment" ? `${repo}/production` : repo,
      target: kind === "repos" ? `${1 + (small(n, 5) % 40)} repositories` : kind === "environment" ? ["production", "staging", "qa"][small(n, 6) % 3] as string : `${["NPM", "DEPLOY", "AWS", "SENTRY"][small(n, 7) % 4]}_${kind === "secret" ? "TOKEN" : "REGION"}_${small(n, 8) % 90}`,
    };
  });
  return [...audited, ...base];
}

/** Item age since a job last read it: a fifth never, a fifth over 90 days ago, the rest within the last 60. */
function lastUsed(name: string): number | null {
  const roll = small(orgSeed(name), 9) % 100;
  if (roll < 20) return null;
  if (roll < 40) return Date.now() - (91 + (roll % 40)) * day;
  return Date.now() - (roll % 60) * day - 3_600_000;
}

/** Organizations the mock Polar checkout put a card on. */
const carded = new Set<string>();
export const addCard = (org: string) => void carded.add(org.toLowerCase());
/** Whether the organization has a card: all do unless MOCK_CARD=off, then those that went through the checkout. */
export const hasCard = (org: string) => process.env.MOCK_CARD !== "off" || carded.has(org.toLowerCase());

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
function validName(raw: string, noun: "secret" | "variable"): string | { error: string } {
  const name = raw.toUpperCase();
  const title = noun === "secret" ? "Secret" : "Variable";
  if (name === "") return { error: `${title} names cannot be empty` };
  if (name.length > NAME_MAX) return { error: `${title} names are at most ${NAME_MAX} characters` };
  if (!/^[A-Z0-9_]+$/.test(name)) return { error: `Use letters, digits and _ in ${noun} names` };
  if (/^[0-9]/.test(name)) return { error: `${title} names cannot start with a digit` };
  if (name.startsWith("GITHUB_")) return { error: `${title} names cannot start with GITHUB_` };
  return name;
}

/** The organization item's repositories: null for all, the list, or the 422 for an unknown name. */
function checkRepositories(org: string, given: unknown): string[] | null | { error: string } {
  if (given === undefined || given === null) return null;
  if (!Array.isArray(given) || given.some((name) => typeof name !== "string")) {
    return { error: "repositories must be a list of names or null" };
  }
  const known = org === SCALE_ORG ? scaleRepoNames() : [`${org}/api`, `${org}/web`];
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
  scope: string,
): true {
  if (name === "") {
    if (method !== "GET") {
      json(res, 405, { error: "method not allowed" });
      return true;
    }
    json(res, 200, [...rows.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([n, row]) => environmentJson(n, row)));
  } else if (method === "DELETE") {
    const removed = rows.delete(name);
    if (removed) recordAudit("environment.delete", scope, name);
    json(res, removed ? 204 : 404, { error: "not found" });
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
      recordAudit("environment.put", scope, existing);
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
  session: Session,
  body: unknown,
  url: URL = new URL("http://localhost/"),
): boolean {
  const found =
    /^orgs\/([^/]+)\/(billing(?:\/cap)?|secrets|vars|audit)(?:\/([^/]+))?$/.exec(path) ??
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
  const known = org ? session.role(owner) === "admin" : (owner === "e2e" || session.role(owner) === "admin") && (repo === "demo" || owner === SCALE_ORG);
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
      const daily = dailySpend(owner);
      json(res, 200, {
        card: hasCard(owner) ? { brand: "visa", last4: "4242", exp_month: 8, exp_year: 2029 } : null,
        daily,
        invoices: invoicesOf(owner),
        seconds: daily.reduce((sum, d) => sum + d.seconds, 0),
        units: daily.reduce((sum, d) => sum + d.seconds, 0),
        spendCents: daily.reduce((sum, d) => sum + d.cents, 0),
        capDollars: billing.capDollars,
        minCapDollars: 50,
        maxCapDollars: 10_000,
        running: concurrencyNow(owner).running,
        maxRunning: concurrencyNow(owner).limit,
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

  if (what === "audit") {
    if (method !== "GET") {
      json(res, 405, { error: "method not allowed" });
      return true;
    }
    const repo = url.searchParams.get("repo");
    json(res, 200, paginate(auditRows(owner, scaleRepoNames).filter((row) => !repo || row.scope === repo || row.scope.startsWith(`${repo}/`)), url));
    return true;
  }

  const environmentRows = inEnvironments ? seedEnvironments(`${owner}/${repo}`) : null;
  if (environmentRows && what === "") {
    return environmentRoute(res, method, environmentRows, environment, body, `${owner}/${repo}`);
  }
  if (environmentRows && !environmentRows.has(environment)) {
    json(res, 404, { error: "not found" });
    return true;
  }

  const kind = what as "secrets" | "vars";
  const rows = seed(org ? `org:${owner}` : inEnvironments ? `env:${owner}/${repo}/${environment}` : `repo:${owner}/${repo}`, kind);
  if (item === "" && method === "GET") {
    const needle = (url.searchParams.get("q") ?? "").toLowerCase();
    const own = org ? "org" : inEnvironments ? "env" : "repo";
    // Wider scopes the list also holds, narrowest first.
    const wider: { scope: "org" | "repo"; rows: Table }[] =
      own === "env" ? [{ scope: "repo", rows: seed(`repo:${owner}/${repo}`, kind) }, { scope: "org", rows: seed(`org:${owner}`, kind) }] : own === "repo" ? [{ scope: "org", rows: seed(`org:${owner}`, kind) }] : [];
    const reaches = (scope: string, row: Item) => scope !== "org" || !row.repositories || row.repositories.includes(`${owner}/${repo}`);
    const view = (name: string, row: Item, scope: "org" | "repo" | "env", shadowed: boolean) => {
      const repositories = scope === "org" ? (row.repositories ?? null) : null;
      const common = { name, updatedAt: row.updatedAt, repositories, scope, shadowed, last_used_at: lastUsed(name) };
      return kind === "secrets" ? common : { ...common, value: row.value };
    };
    const matches = ([name]: [string, Item]) => !needle || name.toLowerCase().includes(needle);
    const byName = ([a]: [string, Item], [b]: [string, Item]) => a.localeCompare(b);
    const list = [
      ...[...rows.entries()].filter(matches).sort(byName).map(([name, row]) => view(name, row, own, false)),
      ...wider.flatMap((level, at) =>
        [...level.rows.entries()]
          .filter(([, row]) => reaches(level.scope, row))
          .filter(matches)
          .sort(byName)
          .map(([name, row]) => view(name, row, level.scope, rows.has(name) || wider.slice(0, at).some((narrower) => narrower.rows.has(name)))),
      ),
    ];
    json(res, 200, paginate(list, url));
    return true;
  }
  const name = validName(item, kind === "secrets" ? "secret" : "variable");
  if (item === "" || typeof name !== "string") {
    json(res, 400, typeof name === "string" ? { error: "not found" } : name);
    return true;
  }
  const auditScope = org ? owner : inEnvironments ? `${owner}/${repo}/${environment}` : `${owner}/${repo}`;
  const auditKind = kind === "secrets" ? "secret" : "var";
  if (method === "DELETE") {
    const removed = rows.delete(name);
    if (removed) recordAudit(`${auditKind}.delete`, auditScope, name);
    json(res, removed ? 204 : 404, { error: "not found" });
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
      recordAudit(`${auditKind}.put`, auditScope, name);
      json(res, 204);
    }
  } else {
    json(res, 405, { error: "method not allowed" });
  }
  return true;
}

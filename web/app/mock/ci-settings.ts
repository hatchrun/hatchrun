// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

// The settings routes of /api/ci under `npm run dev:mock`, in the
// shapes of crates/ci/src/settings_api.rs: an organization's billing
// and the secrets and variables of an organization or a repository.
// Any organization of the mock tenants answers, and so does the
// repository demo of each. Billing has a card (the mock Polar checkout adds one), 320 minutes and a $50
// cap; MOCK_CARD=off drops the card. Values are kept in memory and a
// secret's is never answered.

import type { IncomingMessage, ServerResponse } from "node:http";

interface Item {
  value: string;
  updatedAt: number;
}

type Table = Map<string, Item>;

const orgs = ["acme", "acme-corp", "octo-labs"];
const day = 24 * 3600 * 1000;

const table = (rows: Record<string, string>): Table =>
  new Map(Object.entries(rows).map(([name, value], i) => [name, { value, updatedAt: Date.now() - (i + 1) * day }]));

/** Secrets and vars by scope key: `org:<login>` or `repo:<owner>/<repo>`. */
const secrets = new Map<string, Table>();
const vars = new Map<string, Table>();

function seed(key: string, kind: "secrets" | "vars"): Table {
  const store = kind === "secrets" ? secrets : vars;
  let found = store.get(key);
  if (!found) {
    const org = key.startsWith("org:");
    found = table(
      kind === "secrets"
        ? org
          ? { NPM_TOKEN: "x", DEPLOY_KEY: "x" }
          : { API_TOKEN: "x" }
        : org
          ? { REGION: "ap-south-1" }
          : {},
    );
    store.set(key, found);
  }
  return found;
}

export const billing = { card: process.env.MOCK_CARD !== "off", capDollars: 50 };

const FREE_UNITS = 500;
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
    /^orgs\/([^/]+)\/(billing(?:\/cap)?|secrets|vars)(?:\/([^/]+))?$/.exec(path) ??
    /^repos\/([^/]+)\/([^/]+)\/(secrets|vars)(?:\/([^/]+))?$/.exec(path);
  if (!found) {
    return false;
  }
  const org = path.startsWith("orgs/");
  const owner = decodeURIComponent(found[1] ?? "");
  const repo = org ? "" : decodeURIComponent(found[2] ?? "");
  const what = (org ? found[2] : found[3]) ?? "";
  const item = decodeURIComponent((org ? found[3] : found[4]) ?? "");
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
      const minutes = 320;
      json(res, 200, {
        card: billing.card,
        minutes,
        units: minutes,
        freeUnits: FREE_UNITS,
        spendCents: Math.max(0, minutes - FREE_UNITS) * 0.2,
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

  const kind = what as "secrets" | "vars";
  const rows = seed(org ? `org:${owner}` : `repo:${owner}/${repo}`, kind);
  if (item === "" && method === "GET") {
    const list = [...rows.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, row]) => (kind === "secrets" ? { name, updatedAt: row.updatedAt } : { name, value: row.value, updatedAt: row.updatedAt }));
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
    const value = (body as { value?: unknown } | undefined)?.value;
    if (typeof value !== "string") {
      json(res, 400, { error: 'expected {"value": "..."}' });
    } else if (value === "") {
      json(res, 400, { error: "Values cannot be empty" });
    } else if (Buffer.byteLength(value) > MAX_BYTES) {
      json(res, 413, { error: "Values are at most 48 KB" });
    } else if (!rows.has(name) && rows.size >= (org ? 1000 : 100)) {
      json(res, 409, { error: `${org ? owner : `${owner}/${repo}`} has ${org ? 1000 : 100} ${kind}, the most allowed` });
    } else {
      rows.set(name, { value, updatedAt: Date.now() });
      json(res, 204);
    }
  } else {
    json(res, 405, { error: "method not allowed" });
  }
  return true;
}
